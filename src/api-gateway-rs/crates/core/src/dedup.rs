//! Payload hashing + Redis dedup claim/release — mirrors the top half of
//! `src/core/proxy/index.js`. Both servers share the same Redis instance,
//! so the hash MUST be byte-identical to Node's `JSON.stringify`-based
//! hash for a request to dedupe correctly regardless of which server
//! handled the first vs. the retry — this is why `serde_json`'s
//! `preserve_order` feature is required workspace-wide (see Cargo.toml):
//! a JS object serializes in insertion order, not alphabetical, and the
//! `payload` field here is exactly whatever order the caller's JSON body
//! arrived in.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Serialize)]
struct PayloadHashInput<'a> {
    #[serde(rename = "apiKey")]
    api_key: &'a str,
    service: &'a str,
    action: &'a str,
    payload: &'a Value,
}

/// Default dedup mode: hash the whole payload. Two calls only count as the
/// same request if they're byte-identical (same field order too).
pub fn hash_payload(api_key: &str, service: &str, action: &str, payload: &Value) -> String {
    let input = PayloadHashInput {
        api_key,
        service,
        action,
        payload,
    };
    sha256_hex(&serde_json::to_string(&input).expect("serializing a hash input never fails"))
}

#[derive(Serialize)]
struct IdempotencyHashInput<'a> {
    #[serde(rename = "apiKey")]
    api_key: &'a str,
    service: &'a str,
    action: &'a str,
    #[serde(rename = "idempotencyKey")]
    idempotency_key: &'a str,
}

/// Client-supplied idempotency-key mode: the caller controls what counts
/// as a retry instead of the system inferring it from exact payload bytes.
pub fn hash_idempotency_key(api_key: &str, service: &str, action: &str, idempotency_key: &str) -> String {
    let input = IdempotencyHashInput {
        api_key,
        service,
        action,
        idempotency_key,
    };
    sha256_hex(&serde_json::to_string(&input).expect("serializing a hash input never fails"))
}

/// Hash of the payload alone (no api key/service/action) — stashed
/// alongside an idempotency-key-mode cached result so a key reused with a
/// genuinely different payload can be detected and rejected.
pub fn hash_only(payload: &Value) -> String {
    sha256_hex(&serde_json::to_string(payload).expect("serializing a hash input never fails"))
}

/// Per-field dedup mode (the Dedup Rules feature): dedupe on a configured
/// subset of the payload's own fields instead of the whole payload or a
/// client-supplied key. Field names are sorted before hashing so the key is
/// stable regardless of the order the rule's fields were configured in;
/// missing fields hash as `null` rather than being skipped.
pub fn hash_field_values(
    api_key: &str,
    service: &str,
    action: &str,
    payload: &Value,
    fields: &[String],
) -> String {
    let mut sorted_fields = fields.to_vec();
    sorted_fields.sort();

    // BTreeMap here mirrors the JS `values[f] = ...` object being built by
    // iterating the ALREADY-SORTED field list — insertion order equals
    // sorted order in both implementations, so a plain sorted map is fine
    // here (unlike `payload` above, this one doesn't need to preserve an
    // externally-supplied order).
    let mut values: BTreeMap<&str, &Value> = BTreeMap::new();
    static NULL: Value = Value::Null;
    for f in &sorted_fields {
        values.insert(f.as_str(), payload.get(f).unwrap_or(&NULL));
    }

    #[derive(Serialize)]
    struct Input<'a> {
        #[serde(rename = "apiKey")]
        api_key: &'a str,
        service: &'a str,
        action: &'a str,
        fields: &'a BTreeMap<&'a str, &'a Value>,
    }
    let input = Input {
        api_key,
        service,
        action,
        fields: &values,
    };
    sha256_hex(&serde_json::to_string(&input).expect("serializing a hash input never fails"))
}

const DEDUP_TTL_SECONDS: i64 = 86400;

pub struct ClaimResult {
    pub key: String,
    pub claimed: bool,
}

/// Atomically claims a dedup slot via `SET key val EX ttl NX` — only one
/// caller can win. The loser either gets back a completed result, or finds
/// the winner's request still in flight (`pending: true`).
pub async fn claim_dedup_slot(
    conn: &mut redis::aio::MultiplexedConnection,
    dedup_hash: &str,
) -> redis::RedisResult<ClaimResult> {
    let key = format!("dedup:{dedup_hash}");
    let claimed: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg(r#"{"pending":true}"#)
        .arg("EX")
        .arg(DEDUP_TTL_SECONDS)
        .arg("NX")
        .query_async(conn)
        .await?;
    Ok(ClaimResult {
        key,
        claimed: claimed.as_deref() == Some("OK"),
    })
}

pub async fn read_dedup_slot(
    conn: &mut redis::aio::MultiplexedConnection,
    key: &str,
) -> redis::RedisResult<Option<Value>> {
    let raw: Option<String> = redis::cmd("GET").arg(key).query_async(conn).await?;
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
}

pub async fn complete_dedup_slot(
    conn: &mut redis::aio::MultiplexedConnection,
    key: &str,
    result: &Value,
) -> redis::RedisResult<()> {
    let serialized = serde_json::to_string(result).expect("result is always valid JSON");
    let _: () = redis::cmd("SET")
        .arg(key)
        .arg(serialized)
        .arg("EX")
        .arg(DEDUP_TTL_SECONDS)
        .query_async(conn)
        .await?;
    Ok(())
}

pub async fn release_dedup_slot(
    conn: &mut redis::aio::MultiplexedConnection,
    key: &str,
) -> redis::RedisResult<()> {
    let _: () = redis::cmd("DEL").arg(key).query_async(conn).await?;
    Ok(())
}
