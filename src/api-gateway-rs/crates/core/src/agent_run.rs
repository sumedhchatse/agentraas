//! Agent Run Budgeting & Loop Detection. An agent optionally tags a
//! multi-step task with a stable `run_id` (analogous to how an
//! idempotency key tags a single call); this tracks how many times
//! that run has invoked the same `service.action`, and trips an "Agent
//! Circuit Breaker" after too many repeats — the signal is "the agent
//! itself may be stuck in a loop," a different axis from the existing
//! per-service circuit breaker (which reacts to the *upstream* being
//! unresponsive) and from dedup (which reacts to *identical* payloads).
//! Deliberately optional and additive: a caller that never sends a
//! `run_id` sees no behavior change at all.

use redis::aio::MultiplexedConnection;

pub struct LoopCheckResult {
    pub tripped: bool,
    pub count: i64,
}

fn key(org_id: &str, agent_id: &str, run_id: &str, service: &str, action: &str) -> String {
    format!("agentrun:{org_id}:{agent_id}:{run_id}:{service}:{action}")
}

/// Increments the call count for this run+tool and reports whether it
/// has now exceeded `max_repeats`. Every call counts, including ones
/// that end up dedup-cached — the point is the agent's own repeated
/// invocation pattern, not how AgentRaaS happened to answer it.
pub async fn record_call_and_check(
    conn: &mut MultiplexedConnection,
    org_id: &str,
    agent_id: &str,
    run_id: &str,
    service: &str,
    action: &str,
    max_repeats: i64,
    run_ttl_seconds: i64,
) -> redis::RedisResult<LoopCheckResult> {
    let redis_key = key(org_id, agent_id, run_id, service, action);
    let count: i64 = redis::cmd("INCR").arg(&redis_key).query_async(conn).await?;
    if count == 1 {
        let _: () = redis::cmd("EXPIRE").arg(&redis_key).arg(run_ttl_seconds).query_async(conn).await?;
    }
    Ok(LoopCheckResult { tripped: count > max_repeats, count })
}
