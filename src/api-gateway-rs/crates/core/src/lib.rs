//! Shared reliability engine for AgentRaaS's Rust port — mirrors
//! `src/core/{proxy,mcp}` from the Node implementation. Phase 0 only
//! carries the services-config loader; dedup/circuit-breaker/retry/MCP
//! land in Phase 2.

pub mod circuit_breaker;
pub mod config;
pub mod crypto;
pub mod dedup;
pub mod token_bucket;
pub mod validator;

// Enterprise-tier only (mirrors `src/ee/*`) — gated behind the
// `enterprise` Cargo feature so the Community edition (the public repo)
// never even compiles this code in, not just "doesn't call it."
#[cfg(feature = "enterprise")]
pub mod dlp;
#[cfg(feature = "enterprise")]
pub mod hmac_verify;

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
