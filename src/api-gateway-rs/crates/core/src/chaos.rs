//! Chaos mode: an opt-in, per-org-per-service synthetic failure rate,
//! for testing an agent's own retry/circuit-breaker resilience against a
//! real curated service without needing to actually break that service.
//! Off by default (no Redis key = 0% fail rate) — purely additive, and
//! deliberately Redis-only (no migration) since this is an ops/testing
//! toggle, not a persistent business entity.
//!
//! Checked once per individual upstream attempt (inside forward_action,
//! not once per top-level request), so a fail_rate under 1.0 lets a
//! retry legitimately succeed — simulating a flaky/degraded upstream,
//! not just a hard outage.

use rand::Rng;
use redis::aio::MultiplexedConnection;

fn key(org_id: &str, service: &str) -> String {
    format!("chaos:{org_id}:{service}")
}

pub async fn should_fail(conn: &mut MultiplexedConnection, org_id: &str, service: &str) -> bool {
    let raw: Option<String> = redis::cmd("GET").arg(key(org_id, service)).query_async(conn).await.unwrap_or(None);
    let Some(rate) = raw.and_then(|s| s.parse::<f64>().ok()) else { return false };
    if rate <= 0.0 {
        return false;
    }
    rand::thread_rng().gen::<f64>() < rate.min(1.0)
}

pub async fn get_fail_rate(conn: &mut MultiplexedConnection, org_id: &str, service: &str) -> f64 {
    let raw: Option<String> = redis::cmd("GET").arg(key(org_id, service)).query_async(conn).await.unwrap_or(None);
    raw.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0)
}

pub async fn set_fail_rate(conn: &mut MultiplexedConnection, org_id: &str, service: &str, rate: f64) -> redis::RedisResult<()> {
    let k = key(org_id, service);
    if rate <= 0.0 {
        let _: () = redis::cmd("DEL").arg(&k).query_async(conn).await?;
    } else {
        let _: () = redis::cmd("SET").arg(&k).arg(rate.min(1.0).to_string()).query_async(conn).await?;
    }
    Ok(())
}
