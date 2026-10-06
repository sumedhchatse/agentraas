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
    /// Random value stored under the key, so `release` only deletes a
    /// lock this caller still owns (not one re-acquired after its TTL ran out).
    pub token: String,
}

/// Safety net only: `handle_request` releases the lock as soon as the call
/// finishes. The TTL covers a crash, and a forward whose outcome is unknown
/// (upstream may still be acting on the resource), which keeps the lock.
pub const DEFAULT_TTL_SECONDS: i64 = 15;

pub async fn acquire(
    conn: &mut MultiplexedConnection,
    org_id: &str,
    resource_id: &str,
    ttl_seconds: i64,
) -> redis::RedisResult<LockResult> {
    let token = format!("{:032x}", rand::random::<u128>());
    let result: Option<String> = redis::cmd("SET")
        .arg(key(org_id, resource_id))
        .arg(&token)
        .arg("EX")
        .arg(ttl_seconds)
        .arg("NX")
        .query_async(conn)
        .await?;
    Ok(LockResult { acquired: result.as_deref() == Some("OK"), token })
}

/// Compare-and-delete: a no-op if the lock expired and someone else holds it now.
pub async fn release(
    conn: &mut MultiplexedConnection,
    org_id: &str,
    resource_id: &str,
    token: &str,
) -> redis::RedisResult<()> {
    redis::Script::new("if redis.call('GET', KEYS[1]) == ARGV[1] then return redis.call('DEL', KEYS[1]) end return 0")
        .key(key(org_id, resource_id))
        .arg(token)
        .invoke_async::<_, i64>(conn)
        .await?;
    Ok(())
}

fn key(org_id: &str, resource_id: &str) -> String {
    format!("resource_lock:{org_id}:{resource_id}")
}
