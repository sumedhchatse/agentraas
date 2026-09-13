//! Payload hashing + Redis dedup claim/release. The hash format is a
//! stable wire contract in its own right (existing dedup keys, and the
//! golden values in this file's own tests, depend on it never silently
//! shifting) — originally required to be byte-identical to Node's
//! `JSON.stringify`-based hash from when this and a Node server ran
//! side-by-side against the same Redis instance during the Rust port;
//! that's why `serde_json`'s `preserve_order` feature is still required
//! workspace-wide (see Cargo.toml) even now that Node is fully retired: a
//! JS object serializes in insertion order, not alphabetical, and the
//! `payload` field here is exactly whatever order the caller's JSON body
//! arrived in.

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(crate) fn sha256_hex(input: &str) -> String {
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
    // On-Behalf-Of End-User Identity: omitted entirely (not even as null)
    // when absent, so a call with no end_user_id hashes byte-identically
    // to how it always has — existing dedup keys must not shift under
    // callers who never use this feature.
    #[serde(rename = "endUserId", skip_serializing_if = "Option::is_none")]
    end_user_id: Option<&'a str>,
}

/// Default dedup mode: hash the whole payload. Two calls only count as the
/// same request if they're byte-identical (same field order too).
/// `end_user_id`, when present, scopes the hash so two different end-users
/// acting through the same org/agent never dedupe against each other.
pub fn hash_payload(api_key: &str, service: &str, action: &str, payload: &Value, end_user_id: Option<&str>) -> String {
    let input = PayloadHashInput {
        api_key,
        service,
        action,
        payload,
        end_user_id,
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
    #[serde(rename = "endUserId", skip_serializing_if = "Option::is_none")]
    end_user_id: Option<&'a str>,
}

/// Client-supplied idempotency-key mode: the caller controls what counts
/// as a retry instead of the system inferring it from exact payload bytes.
pub fn hash_idempotency_key(api_key: &str, service: &str, action: &str, idempotency_key: &str, end_user_id: Option<&str>) -> String {
    let input = IdempotencyHashInput {
        api_key,
        service,
        action,
        idempotency_key,
        end_user_id,
    };
    sha256_hex(&serde_json::to_string(&input).expect("serializing a hash input never fails"))
}

/// Hash of the payload alone (no api key/service/action) — stashed
/// alongside an idempotency-key-mode cached result so a key reused with a
/// genuinely different payload can be detected and rejected.
pub fn hash_only(payload: &Value) -> String {
    sha256_hex(&serde_json::to_string(payload).expect("serializing a hash input never fails"))
}

/// Collapses trivial formatting/type differences in a single field's value
/// so e.g. `"amount": 100`, `100.0`, and `"100"` all hash identically, and
/// `" Jane "`/`"jane"`/`"Jane"` do too — the "ignoring trivial LLM
/// parameter variations" half of Semantic/Entity-Level Idempotency Keys.
/// Opt-in per rule (see `normalize` below); composite types are left as-is
/// rather than guessed at.
fn normalize_value(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(s.trim().to_lowercase()),
        Value::Number(n) => {
            let canonical = if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 { (f as i64).to_string() } else { f.to_string() }
            } else {
                return v.clone();
            };
            Value::String(canonical)
        }
        other => other.clone(),
    }
}

/// Per-field dedup mode (the Dedup Rules feature): dedupe on a configured
/// subset of the payload's own fields instead of the whole payload or a
/// client-supplied key. Field names are sorted before hashing so the key is
/// stable regardless of the order the rule's fields were configured in;
/// missing fields hash as `null` rather than being skipped. `normalize`
/// applies `normalize_value` to each field's value first (opt-in per rule —
/// off preserves the original byte-exact-match behavior).
pub fn hash_field_values(
    api_key: &str,
    service: &str,
    action: &str,
    payload: &Value,
    fields: &[String],
    normalize: bool,
    end_user_id: Option<&str>,
) -> String {
    let mut sorted_fields = fields.to_vec();
    sorted_fields.sort();

    // BTreeMap here mirrors the JS `values[f] = ...` object being built by
    // iterating the ALREADY-SORTED field list — insertion order equals
    // sorted order in both implementations, so a plain sorted map is fine
    // here (unlike `payload` above, this one doesn't need to preserve an
    // externally-supplied order).
    let mut values: BTreeMap<&str, Value> = BTreeMap::new();
    static NULL: Value = Value::Null;
    for f in &sorted_fields {
        let raw = payload.get(f).unwrap_or(&NULL);
        values.insert(f.as_str(), if normalize { normalize_value(raw) } else { raw.clone() });
    }

    #[derive(Serialize)]
    struct Input<'a> {
        #[serde(rename = "apiKey")]
        api_key: &'a str,
        service: &'a str,
        action: &'a str,
        fields: &'a BTreeMap<&'a str, Value>,
        #[serde(rename = "endUserId", skip_serializing_if = "Option::is_none")]
        end_user_id: Option<&'a str>,
    }
    let input = Input {
        api_key,
        service,
        action,
        fields: &values,
        end_user_id,
    };
    sha256_hex(&serde_json::to_string(&input).expect("serializing a hash input never fails"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Golden values generated independently via Node's own
    // crypto.createHash('sha256') over JSON.stringify of an object built
    // in the exact key order PayloadHashInput/IdempotencyHashInput/the
    // hash_field_values Input struct serialize in — this is the actual
    // cross-server contract this module's doc comment warns about, not
    // just internal self-consistency. If any of these change, a Node
    // caller and a Rust caller would stop deduping each other's retries.
    #[test]
    fn hash_payload_matches_nodes_json_stringify_format() {
        let payload = json!({"amount": 100, "customer": "cus_1"});
        let got = hash_payload("key_abc", "stripe", "create_charge", &payload, None);
        assert_eq!(got, "0338bc421ab64048b2a56691f5a566839b11046eef4282ea15978cfd91b912d5");

        let got_scoped = hash_payload("key_abc", "stripe", "create_charge", &payload, Some("user_42"));
        assert_eq!(got_scoped, "1174a600c569696b7bf6a0c595f0f1c13765de1183514bc11ac1b89b2b3449c4");
    }

    #[test]
    fn hash_idempotency_key_matches_nodes_json_stringify_format() {
        let got = hash_idempotency_key("key_abc", "stripe", "create_charge", "my-key-1", None);
        assert_eq!(got, "8d6d5e2abe32ed65ea76172c00eac98907c2df2255ccc70c138b29a980484ad2");
    }

    #[test]
    fn hash_only_matches_nodes_json_stringify_format() {
        let payload = json!({"amount": 100, "customer": "cus_1"});
        assert_eq!(hash_only(&payload), "d0cc5d551e6fcf5ef46ee2d735b81323f538f3feff878128c36f4b699508f20b");
    }

    #[test]
    fn hash_field_values_matches_nodes_json_stringify_format() {
        let payload = json!({"customer_email": "jane@acme.com"});
        let got = hash_field_values("key_abc", "stripe", "create_charge", &payload, &["customer_email".to_string()], false, None);
        assert_eq!(got, "fd3b59407d1cb80300d984539cc86578129b8633acaa990ef447725c2c85f0b9");
    }

    #[test]
    fn end_user_id_none_hashes_identically_to_omitting_it_entirely() {
        // The doc comment on end_user_id promises byte-identical hashes for
        // callers who never use the feature — this is the regression that
        // promise depends on (skip_serializing_if actually working).
        let payload = json!({"a": 1});
        let a = hash_payload("k", "svc", "act", &payload, None);
        let b = hash_idempotency_key("k", "svc", "act", "idem", None);
        // Same call twice with None must be deterministic, not just equal
        // to some other call.
        assert_eq!(a, hash_payload("k", "svc", "act", &payload, None));
        assert_eq!(b, hash_idempotency_key("k", "svc", "act", "idem", None));
    }

    #[test]
    fn different_end_user_ids_never_collide_even_with_same_payload() {
        let payload = json!({"a": 1});
        let alice = hash_payload("k", "svc", "act", &payload, Some("alice"));
        let bob = hash_payload("k", "svc", "act", &payload, Some("bob"));
        let none = hash_payload("k", "svc", "act", &payload, None);
        assert_ne!(alice, bob);
        assert_ne!(alice, none);
    }

    #[test]
    fn normalize_value_collapses_equivalent_numbers_and_strings() {
        assert_eq!(normalize_value(&json!(100)), normalize_value(&json!(100.0)));
        assert_eq!(normalize_value(&json!(100)), normalize_value(&json!("100")));
        assert_eq!(normalize_value(&json!(" Jane ")), normalize_value(&json!("jane")));
        assert_eq!(normalize_value(&json!(" Jane ")), normalize_value(&json!("Jane")));
        // A fractional float is NOT collapsed into an integer string.
        assert_ne!(normalize_value(&json!(100.5)), normalize_value(&json!(100)));
        // Composite types pass through unchanged (opt-in per rule, not guessed at).
        let arr = json!([1, 2]);
        assert_eq!(normalize_value(&arr), arr);
    }

    #[test]
    fn hash_field_values_is_order_independent_and_treats_missing_as_null() {
        let payload = json!({"b": 2, "a": 1});
        // Field list given in reverse order still hashes the same as forward order.
        let forward = hash_field_values("k", "svc", "act", &payload, &["a".to_string(), "b".to_string()], false, None);
        let reverse = hash_field_values("k", "svc", "act", &payload, &["b".to_string(), "a".to_string()], false, None);
        assert_eq!(forward, reverse);

        // A field absent from the payload hashes as null, not as if it were skipped.
        let with_missing = hash_field_values("k", "svc", "act", &payload, &["a".to_string(), "missing".to_string()], false, None);
        assert_ne!(with_missing, hash_field_values("k", "svc", "act", &payload, &["a".to_string()], false, None));
    }

    #[test]
    fn hash_field_values_normalize_flag_actually_changes_the_hash() {
        let payload = json!({"email": " Jane@Acme.com "});
        let raw = hash_field_values("k", "svc", "act", &payload, &["email".to_string()], false, None);
        let normalized = hash_field_values("k", "svc", "act", &payload, &["email".to_string()], true, None);
        assert_ne!(raw, normalized);

        // But two differently-cased/whitespaced emails DO collide once normalized.
        let other_payload = json!({"email": "jane@acme.com"});
        let other_normalized = hash_field_values("k", "svc", "act", &other_payload, &["email".to_string()], true, None);
        assert_eq!(normalized, other_normalized);
    }
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
    claim_dedup_slot_with_ttl(conn, dedup_hash, None).await
}

/// Same as `claim_dedup_slot`, but honors a dedup rule's own configured
/// window (Semantic/Entity-Level Idempotency Keys' rolling-window
/// setting) instead of always using the 24h system default.
pub async fn claim_dedup_slot_with_ttl(
    conn: &mut redis::aio::MultiplexedConnection,
    dedup_hash: &str,
    ttl_seconds: Option<i64>,
) -> redis::RedisResult<ClaimResult> {
    let key = format!("dedup:{dedup_hash}");
    let claimed: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg(r#"{"pending":true}"#)
        .arg("EX")
        .arg(ttl_seconds.unwrap_or(DEDUP_TTL_SECONDS))
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
    complete_dedup_slot_with_ttl(conn, key, result, None).await
}

/// Same as `complete_dedup_slot`, but honors a dedup rule's own configured
/// window — must match whatever TTL `claim_dedup_slot_with_ttl` used for
/// this same key, so the slot doesn't outlive (or undershoot) the rule's
/// intended dedup window.
pub async fn complete_dedup_slot_with_ttl(
    conn: &mut redis::aio::MultiplexedConnection,
    key: &str,
    result: &Value,
    ttl_seconds: Option<i64>,
) -> redis::RedisResult<()> {
    let serialized = serde_json::to_string(result).expect("result is always valid JSON");
    let _: () = redis::cmd("SET")
        .arg(key)
        .arg(serialized)
        .arg("EX")
        .arg(ttl_seconds.unwrap_or(DEDUP_TTL_SECONDS))
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
