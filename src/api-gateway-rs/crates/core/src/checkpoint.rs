//! State Checkpointing. The caller tags a call with both a `run_id`
//! (the task) and a `step_id` (a stable identifier for this specific
//! step within that task, e.g. "step-1" or "fetch-invoice") — deliberately
//! caller-supplied rather than derived from a call count: a derived
//! ordinal (Nth call to this action within the run) can't tell "the
//! agent is replaying steps 1-3 again after a crash, before reaching
//! new step 4" apart from "this is a genuinely new 2nd/3rd/4th call to
//! the same action" — both look identical from a bare call count. An
//! explicit step_id removes that ambiguity: if the SAME run_id+step_id
//! pair is seen again, it's unambiguously a replay of that exact step,
//! regardless of whether the replayed payload matches byte-for-byte
//! (a stricter, run-scoped variant of dedup's own payload-hash
//! matching). Reuses `dedup`'s slot value shape/helpers, just under a
//! different key namespace.

use serde_json::Value;

use crate::dedup::{complete_dedup_slot_with_ttl, read_dedup_slot};

pub fn step_key(run_id: &str, step_id: &str) -> String {
    format!("checkpoint:{run_id}:{step_id}")
}

pub async fn read_checkpoint(conn: &mut redis::aio::MultiplexedConnection, key: &str) -> redis::RedisResult<Option<Value>> {
    read_dedup_slot(conn, key).await
}

/// `ttl_seconds` is caller-configurable (`SharedState::checkpoint_ttl_seconds`)
/// rather than dedup's fixed default — a long-running, HITL-gated run can
/// easily sit longer than a single-call dedup window ever needs to.
pub async fn write_checkpoint(conn: &mut redis::aio::MultiplexedConnection, key: &str, result: &Value, ttl_seconds: i64) -> redis::RedisResult<()> {
    complete_dedup_slot_with_ttl(conn, key, result, Some(ttl_seconds)).await
}
