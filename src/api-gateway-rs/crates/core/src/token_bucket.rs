//! Redis-backed atomic token bucket rate limiter — functionally equivalent
//! to `src/core/proxy/token-bucket.js`'s `TokenBucket` class (lazy refill
//! computed from elapsed wall-clock time, correct across multiple
//! stateless instances, atomic via a Lua script so check-then-act can't
//! race). Not required to be byte-identical to Node's own script — this is
//! ephemeral rate-limit accounting, not data either server needs to read
//! back from the other, unlike the dedup hash format.

use redis::Script;

const SCRIPT: &str = r#"
local key = KEYS[1]
local capacity = tonumber(ARGV[1])
local refill_rate = tonumber(ARGV[2])
local cost = tonumber(ARGV[3])
local now = tonumber(ARGV[4])

local tokens = capacity
local last_refill = now
local raw = redis.call('GET', key)
if raw then
  local parsed = cjson.decode(raw)
  tokens = parsed.tokens
  last_refill = parsed.last_refill
end

local elapsed = (now - last_refill) / 1000.0
if elapsed > 0 then
  tokens = math.min(capacity, tokens + elapsed * refill_rate)
end

local allowed = 0
if tokens >= cost then
  tokens = tokens - cost
  allowed = 1
end

redis.call('SET', key, cjson.encode({tokens = tokens, last_refill = now}), 'EX', 3600)
return {allowed, tostring(tokens)}
"#;

pub struct ConsumeResult {
    pub allowed: bool,
    pub remaining: f64,
    pub retry_after_seconds: Option<u64>,
}

pub struct TokenBucket {
    script: Script,
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new()
    }
}

impl TokenBucket {
    pub fn new() -> Self {
        Self {
            script: Script::new(SCRIPT),
        }
    }

    /// Community-tier behavior: reject immediately over the cap.
    pub async fn try_consume(
        &self,
        conn: &mut redis::aio::MultiplexedConnection,
        bucket_key: &str,
        capacity: f64,
        refill_rate_per_second: f64,
        cost: f64,
    ) -> redis::RedisResult<ConsumeResult> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let (allowed, remaining_str): (i64, String) = self
            .script
            .key(bucket_key)
            .arg(capacity)
            .arg(refill_rate_per_second)
            .arg(cost)
            .arg(now_ms)
            .invoke_async(conn)
            .await?;

        let remaining: f64 = remaining_str.parse().unwrap_or(0.0);
        let retry_after_seconds = if allowed == 0 {
            let deficit = cost - remaining;
            Some((deficit / refill_rate_per_second).ceil().max(1.0) as u64)
        } else {
            None
        };

        Ok(ConsumeResult {
            allowed: allowed == 1,
            remaining,
            retry_after_seconds,
        })
    }

    /// Enterprise-tier behavior: poll until a token frees up or `max_wait`
    /// elapses, instead of rejecting immediately — smooths bursts rather
    /// than failing them. Not wired into any Phase 2 caller yet (Community
    /// mode only so far); included now since it's cheap given try_consume
    /// already exists, and Phase 5 (Enterprise) will need it.
    pub async fn acquire(
        &self,
        conn: &mut redis::aio::MultiplexedConnection,
        bucket_key: &str,
        capacity: f64,
        refill_rate_per_second: f64,
        cost: f64,
        max_wait: std::time::Duration,
    ) -> redis::RedisResult<ConsumeResult> {
        let deadline = std::time::Instant::now() + max_wait;
        loop {
            let result = self
                .try_consume(conn, bucket_key, capacity, refill_rate_per_second, cost)
                .await?;
            if result.allowed || std::time::Instant::now() >= deadline {
                return Ok(result);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}
