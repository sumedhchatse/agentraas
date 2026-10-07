//! AgentRaaS's reliability engine as pure logic (no database): dedup, circuit
//! breaker, rate limits, validation, checkpoints, tiers, crypto and the
//! services config. The `api` crate wires it to Postgres, Redis and HTTP.

pub mod agent_run;
pub mod chaos;
pub mod checkpoint;
pub mod circuit_breaker;
pub mod config;
pub mod crypto;
pub mod dedup;
pub mod pruner;
pub mod resource_lock;
pub mod schema_drift;
pub mod semantic_dedup;
pub mod tier;
pub mod undo;
pub mod token_bucket;
pub mod validator;

// Enterprise-tier only (mirrors `src/ee/*`) — gated behind the
// `enterprise` Cargo feature so the Community edition (the public repo)
// never even compiles this code in, not just "doesn't call it."
#[cfg(feature = "enterprise")]
pub mod dlp;
#[cfg(feature = "enterprise")]
pub mod hmac_verify;
#[cfg(feature = "enterprise")]
pub mod output_sanitize;

/// Constant-time string comparison — used both by Enterprise inbound-
/// webhook HMAC verification (`hmac_verify`) and by Paddle billing's
/// webhook signature check (Community-tier, not Enterprise-gated), so it
/// lives here rather than inside the gated `hmac_verify` module.
pub fn timing_safe_equal_strings(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
