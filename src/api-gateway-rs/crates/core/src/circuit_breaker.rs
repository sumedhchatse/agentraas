//! Redis-backed circuit breaker — mirrors `getCircuitState`,
//! `getCircuitStatesBatch`, `recordFailure`, `recordSuccess` in
//! `src/core/proxy/index.js`. State is stored per-service (shared across
//! every org calling that service), same Redis key shape
//! (`circuit:<service>`) and JSON shape (`{"state":..,"failures":N,
//! "openedAt":ms}`) as Node, since both servers read/write the same Redis.
//!
//! DB persistence of transitions (`circuit_breaker_events`, for the
//! dashboard's uptime report) is deliberately NOT done here — this crate
//! stays DB-agnostic. Callers get back any `Transition`s that happened and
//! are responsible for the best-effort `INSERT`, exactly mirroring Node's
//! `logCircuitTransition` being fire-and-forget and never allowed to affect
//! the breaker's own (Redis) behavior.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const HALF_OPEN_AFTER_MS: i64 = 30_000;
const FAILURE_THRESHOLD: u32 = 5;
const STATE_TTL_SECONDS: i64 = 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CircuitData {
    state: String,
    #[serde(default)]
    failures: u32,
    #[serde(rename = "openedAt", skip_serializing_if = "Option::is_none")]
    opened_at: Option<i64>,
}

pub struct Transition {
    pub service: String,
    pub from_state: String,
    pub to_state: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

pub async fn get_circuit_state(
    conn: &mut redis::aio::MultiplexedConnection,
    service: &str,
) -> redis::RedisResult<(String, Option<Transition>)> {
    let key = format!("circuit:{service}");
    let raw: Option<String> = redis::cmd("GET").arg(&key).query_async(conn).await?;
    let Some(raw) = raw else {
        return Ok(("closed".to_string(), None));
    };
    let data: CircuitData = serde_json::from_str(&raw).unwrap_or(CircuitData {
        state: "closed".to_string(),
        failures: 0,
        opened_at: None,
    });

    if data.state == "open" {
        let opened_at = data.opened_at.unwrap_or(0);
        if now_ms() - opened_at > HALF_OPEN_AFTER_MS {
            let new_data = CircuitData {
                state: "half-open".to_string(),
                failures: 0,
                opened_at: None,
            };
            let _: () = redis::cmd("SETEX")
                .arg(&key)
                .arg(STATE_TTL_SECONDS)
                .arg(serde_json::to_string(&new_data).unwrap())
                .query_async(conn)
                .await?;
            return Ok((
                "half-open".to_string(),
                Some(Transition {
                    service: service.to_string(),
                    from_state: "open".to_string(),
                    to_state: "half-open".to_string(),
                }),
            ));
        }
        return Ok(("open".to_string(), None));
    }
    Ok((data.state, None))
}

/// Batched version for callers needing every service's state at once (the
/// dashboard's services list). One MGET instead of N sequential GETs.
pub async fn get_circuit_states_batch(
    conn: &mut redis::aio::MultiplexedConnection,
    services: &[String],
) -> redis::RedisResult<(HashMap<String, String>, Vec<Transition>)> {
    if services.is_empty() {
        return Ok((HashMap::new(), Vec::new()));
    }
    let keys: Vec<String> = services.iter().map(|s| format!("circuit:{s}")).collect();
    let raw_states: Vec<Option<String>> = redis::cmd("MGET").arg(&keys).query_async(conn).await?;

    let mut results = HashMap::new();
    let mut transitions = Vec::new();
    let mut pipeline = redis::pipe();
    let mut has_writes = false;

    for (i, service) in services.iter().enumerate() {
        let Some(raw) = &raw_states[i] else {
            results.insert(service.clone(), "closed".to_string());
            continue;
        };
        let data: CircuitData = serde_json::from_str(raw).unwrap_or(CircuitData {
            state: "closed".to_string(),
            failures: 0,
            opened_at: None,
        });
        if data.state == "open" {
            let opened_at = data.opened_at.unwrap_or(0);
            if now_ms() - opened_at > HALF_OPEN_AFTER_MS {
                results.insert(service.clone(), "half-open".to_string());
                let new_data = CircuitData {
                    state: "half-open".to_string(),
                    failures: 0,
                    opened_at: None,
                };
                pipeline
                    .cmd("SETEX")
                    .arg(&keys[i])
                    .arg(STATE_TTL_SECONDS)
                    .arg(serde_json::to_string(&new_data).unwrap())
                    .ignore();
                has_writes = true;
                transitions.push(Transition {
                    service: service.clone(),
                    from_state: "open".to_string(),
                    to_state: "half-open".to_string(),
                });
            } else {
                results.insert(service.clone(), "open".to_string());
            }
        } else {
            results.insert(service.clone(), data.state);
        }
    }

    if has_writes {
        let _: () = pipeline.query_async(conn).await?;
    }
    Ok((results, transitions))
}

pub async fn record_failure(
    conn: &mut redis::aio::MultiplexedConnection,
    service: &str,
) -> redis::RedisResult<Option<Transition>> {
    let key = format!("circuit:{service}");
    let raw: Option<String> = redis::cmd("GET").arg(&key).query_async(conn).await?;
    let mut data: CircuitData = raw
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(CircuitData {
            state: "closed".to_string(),
            failures: 0,
            opened_at: None,
        });
    let from_state = data.state.clone();
    data.failures += 1;

    if data.state == "half-open" {
        data.state = "open".to_string();
        data.opened_at = Some(now_ms());
    } else if data.failures >= FAILURE_THRESHOLD {
        data.state = "open".to_string();
        data.opened_at = Some(now_ms());
    }

    let _: () = redis::cmd("SETEX")
        .arg(&key)
        .arg(STATE_TTL_SECONDS)
        .arg(serde_json::to_string(&data).unwrap())
        .query_async(conn)
        .await?;

    if from_state != data.state {
        Ok(Some(Transition {
            service: service.to_string(),
            from_state,
            to_state: data.state,
        }))
    } else {
        Ok(None)
    }
}

/// A successful call is the only signal that a half-open probe worked.
/// No-ops (no Redis write at all) when already closed.
pub async fn record_success(
    conn: &mut redis::aio::MultiplexedConnection,
    service: &str,
) -> redis::RedisResult<Option<Transition>> {
    let key = format!("circuit:{service}");
    let raw: Option<String> = redis::cmd("GET").arg(&key).query_async(conn).await?;
    let Some(raw) = raw else { return Ok(None) };
    let data: CircuitData = match serde_json::from_str(&raw) {
        Ok(d) => d,
        Err(_) => return Ok(None),
    };
    if data.state != "half-open" {
        return Ok(None);
    }
    let new_data = CircuitData {
        state: "closed".to_string(),
        failures: 0,
        opened_at: None,
    };
    let _: () = redis::cmd("SETEX")
        .arg(&key)
        .arg(STATE_TTL_SECONDS)
        .arg(serde_json::to_string(&new_data).unwrap())
        .query_async(conn)
        .await?;
    Ok(Some(Transition {
        service: service.to_string(),
        from_state: "half-open".to_string(),
        to_state: "closed".to_string(),
    }))
}
