//! Cross-agent resource lock — distinct from dedup. Dedup catches a
//! retry of the SAME request (identical payload/idempotency key); this
//! catches two DIFFERENT requests (from the same agent, or two different
//! agents) racing to act on the same declared `resource_id` at the same
//! time — e.g. Agent A and Agent B both updating Stripe customer
//! `cus_123`'s subscription within the same second.
//!
//! Opt-in and additive: a caller that never sends `resource_id` sees no
//! behavior change at all.

use redis::aio::MultiplexedConnection;

pub struct LockResult {
    pub acquired: bool,
}

// ponytail: TTL-based expiry only, no explicit release on completion.
// handle_request has many exit paths (validation failure, HITL freeze,
// streaming's async-spawned completion task, forward success/error...) —
// releasing correctly on every one of them would be a much larger, more
// error-prone diff for a lock that's advisory in the first place. A short
// TTL means the worst case is a few extra seconds of latency for a
// second, legitimate action on the same resource_id right after the
// first one finishes — not a correctness problem. Upgrade to explicit
// release (store a token, release with a compare-and-delete Lua script)
// if that wait proves too slow in practice.
pub const DEFAULT_TTL_SECONDS: i64 = 15;

pub async fn acquire(
    conn: &mut MultiplexedConnection,
    org_id: &str,
    resource_id: &str,
    ttl_seconds: i64,
) -> redis::RedisResult<LockResult> {
    let key = format!("resource_lock:{org_id}:{resource_id}");
    let result: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("EX")
        .arg(ttl_seconds)
        .arg("NX")
        .query_async(conn)
        .await?;
    Ok(LockResult { acquired: result.as_deref() == Some("OK") })
}
