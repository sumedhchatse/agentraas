//! Self-host license tokens — an offline-verifiable proof of an org's
//! paid tier. A self-hosted binary has no billing database to query (see
//! `crates/api/src/agent/db.rs`'s `effective_tier`, the cloud-path
//! equivalent, which just reads `users.plan` directly) — cloud never
//! touches this module at all.
//!
//! Signed with RS256, the same algorithm this codebase already uses for
//! OIDC ID-token verification (`ee/sso.rs`). Deliberately asymmetric:
//! the public key embedded here is safe to ship in every private-repo
//! binary, including ones running on a customer's own infrastructure,
//! because verifying a token can never be used to forge one — only the
//! matching private key (kept solely on the issuing side, agentraas.io,
//! never shipped in any binary) can sign a new one. An HMAC scheme would
//! have made that impossible: the verification "key" and the signing
//! key would be the same secret.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tier::Tier;

/// Verifies license tokens — safe to embed, grants no ability to mint
/// one. This is the real production key, matching the
/// `LICENSE_SIGNING_PRIVATE_KEY` set only on agentraas.io's production
/// environment (never committed anywhere). Test builds use a separate,
/// clearly-fake keypair below instead of this one — its matching
/// private key is inlined in `tests::TEST_PRIVATE_KEY_PEM` for anyone to
/// see, which would be a forgery risk if this constant ever pointed at
/// it in a real build.
#[cfg(not(test))]
const LICENSE_PUBLIC_KEY_PEM: &str = include_str!("../license_public_key.pem");

#[cfg(test)]
const LICENSE_PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAqyUWJWTsPYaDFM+n0dmv
RYhkdeMQUuQqw6kBw54AfCzN6KRnO61OKWBXovn7Gn3qTprkIzEAr7O/EgUuQwLc
sSwxPq9bwddNkIT/HjAHWYmQTW0VXm3a5D718KdjbfaxxSczudn4REb2neFJdDHM
E4h7GvGcJvx+C00gmou/P/K6jcmOhzG16XiRvkygPeTHZq7rz/17RVC29Hysr406
W5oJuc7yc6l7pLvYWLz2+kWylXgIrnrIry00ZNt15tJq60lIFh/YFUrbn3SkktW8
mYhgn3B3dZbQ9FtWyyD8cPYxN0Jxx6Nb1Ok0t3ebKsUqv4mygEFvsbuIUKFUKVT9
JwIDAQAB
-----END PUBLIC KEY-----";

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    /// Customer/org identifier this license was issued for.
    sub: String,
    /// `Tier::as_plan_str()` — reuses the exact same string form as
    /// `users.plan` so the two stay trivially consistent.
    tier: String,
    /// Unix seconds — jsonwebtoken's standard claim name, checked
    /// automatically by `Validation::validate_exp`.
    exp: u64,
    iat: u64,
}

pub struct License {
    pub customer_id: String,
    pub tier: Tier,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock is before 1970").as_secs()
}

/// Signs a license token. Only ever called from the issuing side — the
/// private key never ships in any binary, so this function is only
/// reachable from `crates/api/src/licensing.rs` (a later task, driven by
/// a Paddle webhook), never from a self-hosted deployment.
pub fn sign(customer_id: &str, tier: Tier, ttl_seconds: u64, private_key_pem: &str) -> Result<String, String> {
    let claims = Claims {
        sub: customer_id.to_string(),
        tier: tier.as_plan_str().to_string(),
        iat: now_unix(),
        exp: now_unix() + ttl_seconds,
    };
    let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).map_err(|e| e.to_string())?;
    encode(&Header::new(Algorithm::RS256), &claims, &key).map_err(|e| e.to_string())
}

/// Verifies a license token, returning `None` for any failure —
/// malformed, wrong signature, wrong algorithm, or expired. Never
/// partially trusts a token; a broken/tampered/expired token is
/// indistinguishable from no token at all to every caller (self-host's
/// tier resolution falls back to `Tier::Community` either way, whether
/// this returns `None` because the token is missing or because it's
/// invalid — see `agent/db.rs`'s `effective_tier`).
pub fn verify(token: &str) -> Option<License> {
    let key = DecodingKey::from_rsa_pem(LICENSE_PUBLIC_KEY_PEM.as_bytes()).ok()?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.validate_exp = true;
    // jsonwebtoken defaults to a 60s leeway on `exp`, meant for
    // short-lived auth tokens tolerating minor clock skew. License TTLs
    // are day/month-scale and checked periodically in the background —
    // no reason to trust a token past its stated expiry even briefly.
    validation.leeway = 0;
    let data = decode::<Claims>(token, &key, &validation).ok()?;
    Some(License { customer_id: data.claims.sub, tier: Tier::from_plan_str(&data.claims.tier) })
}

#[cfg(test)]
mod tests {
    use super::*;

    // TEST-ONLY private key, matching the #[cfg(test)] LICENSE_PUBLIC_KEY_PEM
    // above — generated solely for these unit tests, never used for
    // anything real, and never the production keypair (that one only
    // ever exists as an env var on agentraas.io). Inlined (not a
    // separate file) so this test has no dependency outside the repo.
    const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCrJRYlZOw9hoMU
z6fR2a9FiGR14xBS5CrDqQHDngB8LM3opGc7rU4pYFei+fsafepOmuQjMQCvs78S
BS5DAtyxLDE+r1vB102QhP8eMAdZiZBNbRVebdrkPvXwp2Nt9rHFJzO52fhERvad
4Ul0McwTiHsa8Zwm/H4LTSCai78/8rqNyY6HMbXpeJG+TKA95MdmruvP/XtFULb0
fKyvjTpbmgm5zvJzqXuku9hYvPb6RbKVeAiuesivLTRk23Xm0mrrSUgWH9gVStuf
dKSS1byZiGCfcHd1ltD0W1bLIPxw9jE3QnHHo1vU6TS3d5sqxSq/ibKAQW+xu4hQ
oVQpVP0nAgMBAAECggEAA5mPmjDBwVeLeUwW4RSdma5RQqOIi93NwnjTFyzDINmG
aT7QBxLRopAqt7xfWkLMw2OBqfXVaFy1B6mBPBqazsU5sfJZUT34nTIW9akX9nus
w9woB2jzIjrqzGmQ71axjY6SCXY6wSDm/hIni+CiRMTppfrwCGfmNNGl/mozFyK4
lwMwuTn26BpDcNjosS3hPJajp6id/UV3dT8x1n9HslafSNAa7GPTbrR+mf94gXDt
Noe5svVWZFLaGYyi5HNW0oFE1xvcWiFxUg1hOs1diPxU3ZDpds3/GIVTpDrnGKLm
KpxLCNxkHqwCQT9I7j3W3RLlPaY19+xtS78WMK7KfQKBgQDZTsPmw7NBR3cIEWsv
c07jeOR1nZInbN6lv91+8RwwLIwhjm6FI48Ec+nO1NcGMKGfY07TaIDNqvRDnJDE
ABkiAMPHWUbZujwVHxESY55JVGJwJiUTboxXBc3XMoPTLTLGwUR43XCI2pMTfrA1
M0j6CxVOwGOQ6QyG115Te1rAwwKBgQDJniVeJTwh+tYS5M6Vc9iTVkZ4XsDRryIr
cZlQKEYa4CleemqY3NMX6osKEuF7nyBJJwelIQN6wSLa+tqJlhVAxhsHBy/R4UVK
Q1inmZw6SPrh6idzTursmqBpHWNAkF6FA/llP1wl4qJ4RZXfsbODFUD7jvD59yFc
n+k5XRrLzQKBgD/qVtxs+zBcILqSxP/z3mQxjqC5c998ug/uWuuXZz8UGzNTfVZT
myEoJsDbAVOkwiTrRKgRuLDFc4rfZgUAMmQ57VuY+qnXiQx9Urwh6NCQrVNnJMiO
X2DJKD3/cZ6PULv85HLYTt0xzMiTHqjHKNPCpsW++IoKwdB3UBsl0Q+ZAoGBALDq
2Rd3zQB0P41swepbMVx4hHXzj3dwGqfMkx/Hd1z1/tcszIU+oO2HnmJElyAHTili
2k6IXalF+PP20/WPgS7Jp8XPBKNC7a5w0kafgHuUtrGu6tdAFN1yAfi7FPD+vjIy
fpHdu1pzOOYZCZ61LDSGXfNgRwzRUrEYkWsIzA6xAoGACRPPrI7OB2Ez6UXJiv1J
Z9nTfLv5VX/bwn3yHbdlG0dBmNQtEaTQK78LgogYH/O/HhvmChcJyoZn3etYa0BV
pLdTsoww6oP7NLqwIzNTK9KU9yqCR7FglZrthwncpwWSx56qIm0JfYLpcoCvLzYr
d3q+rjmNxLCEkFdc0ZvvPbs=
-----END PRIVATE KEY-----";

    #[test]
    fn valid_token_verifies_and_carries_the_right_tier() {
        let token = sign("cust_123", Tier::Agency, 3600, TEST_PRIVATE_KEY_PEM).unwrap();
        let license = verify(&token).expect("a freshly-signed, unexpired token must verify");
        assert_eq!(license.customer_id, "cust_123");
        assert_eq!(license.tier, Tier::Agency);
    }

    #[test]
    fn tampered_token_fails_verification() {
        let token = sign("cust_123", Tier::Enterprise, 3600, TEST_PRIVATE_KEY_PEM).unwrap();
        // Flip one character roughly in the middle (the payload segment) —
        // base64url is all-ASCII, so this stays valid UTF-8.
        let mut chars: Vec<char> = token.chars().collect();
        let mid = chars.len() / 2;
        chars[mid] = if chars[mid] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(verify(&tampered).is_none());
    }

    #[test]
    fn expired_token_fails_verification() {
        let token = sign("cust_123", Tier::Pro, 0, TEST_PRIVATE_KEY_PEM).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(verify(&token).is_none());
    }

    #[test]
    fn missing_or_garbage_token_fails_verification() {
        assert!(verify("").is_none());
        assert!(verify("not-a-real-token").is_none());
    }
}
