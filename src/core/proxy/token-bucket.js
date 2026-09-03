// src/core/proxy/token-bucket.js
//
// Distributed token-bucket rate limiting, Redis-backed. Originally scaffolded
// as an Enterprise-only module (src/ee/rate_limiter) - moved into core once
// it became clear Community-tier rate limiting needed the exact same
// algorithm too (just called via tryConsume() — reject over the cap —
// instead of Enterprise's acquire() — queue and smooth). The tier
// distinction is which method core/proxy calls (gated on ENTERPRISE_MODE),
// not which code exists; src/ee/rate_limiter now just re-exports this for
// anything still requiring that path.
//
// Correctness requires atomicity: reading the current token count and
// then separately writing the decremented value is a race condition
// under concurrent requests - two requests could both read "1 token
// available" and both proceed, over-admitting by exactly the number of
// concurrent racers. A Redis Lua script executes as a single atomic
// operation from Redis's perspective, closing that race entirely.

// Lazy-refill token bucket: rather than running a background timer to
// add tokens, each call computes how many tokens *would* have accrued
// since the last recorded refill time, based on elapsed wall-clock time.
// This is standard practice for distributed token buckets - a timer-based
// refill would need to run somewhere specific (which instance? what if
// it crashes?), while lazy refill is stateless and correct regardless of
// which ar-api instance happens to handle any given request.
const TOKEN_BUCKET_SCRIPT = `
local key = KEYS[1]
local capacity = tonumber(ARGV[1])
local refill_rate = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
local requested = tonumber(ARGV[4])

local bucket = redis.call('HMGET', key, 'tokens', 'last_refill')
local tokens = tonumber(bucket[1])
local last_refill = tonumber(bucket[2])

if tokens == nil then
  tokens = capacity
  last_refill = now
end

local elapsed = math.max(0, now - last_refill)
local refilled = math.min(capacity, tokens + (elapsed * refill_rate))

if refilled >= requested then
  local remaining = refilled - requested
  redis.call('HMSET', key, 'tokens', remaining, 'last_refill', now)
  redis.call('EXPIRE', key, 3600)
  return {1, remaining}
else
  redis.call('HMSET', key, 'tokens', refilled, 'last_refill', now)
  redis.call('EXPIRE', key, 3600)
  -- Time until enough tokens accrue for this request, in seconds.
  local deficit = requested - refilled
  local wait_seconds = deficit / refill_rate
  return {0, wait_seconds}
end
`;

class TokenBucket {
  // redis: an ioredis client instance (reuses the app's existing connection,
  // doesn't open a new one). One TokenBucket instance is meant to be
  // shared across all callers - capacity/refillRate are passed per-call
  // (different identities may have different limits, e.g. free vs pro
  // plans, or a custom per-org override), while the Lua script SHA gets
  // cached once on this instance regardless of what limits get requested.
  constructor(redis) {
    this.redis = redis;
    this.scriptSha = null;
  }

  async _ensureScriptLoaded() {
    if (!this.scriptSha) {
      this.scriptSha = await this.redis.script('load', TOKEN_BUCKET_SCRIPT);
    }
    return this.scriptSha;
  }

  // Attempts to consume `cost` tokens (default 1) from the named bucket.
  // capacity: maximum tokens the bucket can hold (the burst allowance).
  // refillRatePerSecond: steady-state tokens added per second.
  // Returns { allowed: true, remaining } if tokens were available, or
  // { allowed: false, retryAfterSeconds } if not - the caller decides
  // whether to reject immediately or wait and retry (see acquire() below
  // for the waiting variant).
  async tryConsume(bucketKey, capacity, refillRatePerSecond, cost = 1) {
    const sha = await this._ensureScriptLoaded();
    const now = Date.now() / 1000;
    let result;
    try {
      result = await this.redis.evalsha(sha, 1, bucketKey, capacity, refillRatePerSecond, now, cost);
    } catch (err) {
      // NOSCRIPT can happen if Redis restarted and lost cached scripts -
      // reload and retry once rather than failing the request.
      if (err.message && err.message.includes('NOSCRIPT')) {
        this.scriptSha = null;
        const freshSha = await this._ensureScriptLoaded();
        result = await this.redis.evalsha(freshSha, 1, bucketKey, capacity, refillRatePerSecond, now, cost);
      } else {
        throw err;
      }
    }
    const [allowedFlag, value] = result;
    return allowedFlag === 1
      ? { allowed: true, remaining: parseFloat(value) }
      : { allowed: false, retryAfterSeconds: parseFloat(value) };
  }

  // Waits for a token to become available, up to maxWaitMs, rather than
  // rejecting immediately - this is the actual "traffic smoothing"
  // behavior the Enterprise tier adds over Community's reject-on-burst.
  // Returns { acquired: true } once a token is consumed, or
  // { acquired: false, retryAfterSeconds } if the wait would exceed
  // maxWaitMs - an unbounded wait during a genuine downstream outage
  // would just delay the inevitable failure rather than surfacing it, so
  // this is capped rather than infinite.
  async acquire(bucketKey, capacity, refillRatePerSecond, { cost = 1, maxWaitMs = 10000, pollIntervalMs = 100 } = {}) {
    const deadline = Date.now() + maxWaitMs;
    while (true) {
      const result = await this.tryConsume(bucketKey, capacity, refillRatePerSecond, cost);
      if (result.allowed) return { acquired: true, remaining: result.remaining };

      const waitMs = result.retryAfterSeconds * 1000;
      if (Date.now() + waitMs > deadline) {
        return { acquired: false, retryAfterSeconds: result.retryAfterSeconds };
      }
      await new Promise((resolve) => setTimeout(resolve, Math.min(waitMs, pollIntervalMs)));
    }
  }
}

module.exports = { TokenBucket };
