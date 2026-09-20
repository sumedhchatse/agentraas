//! Upstream contract/schema drift detection (SPEC-SCHEMA-DRIFT.md) — pure
//! fingerprinting and comparison logic. The DB-backed baseline storage and
//! notification orchestration live in `crates/api/src/schema_drift.rs`
//! (same `crates/core` pure-logic / `crates/api` DB-orchestration split
//! `tier.rs`/`agent/db.rs` already established).

use serde_json::Value;
use std::collections::BTreeSet;

const MAX_DEPTH: usize = 4;
const MAX_PATHS: usize = 50;

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn walk(v: &Value, prefix: &str, depth: usize, out: &mut BTreeSet<String>) {
    if out.len() >= MAX_PATHS || depth >= MAX_DEPTH {
        return;
    }
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if out.len() >= MAX_PATHS {
                    return;
                }
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match val {
                    Value::Object(_) | Value::Array(_) => walk(val, &path, depth + 1, out),
                    _ => {
                        out.insert(format!("{path}:{}", json_type_name(val)));
                    }
                }
            }
        }
        Value::Array(items) => {
            // Only the first element's shape - good enough to catch a
            // field rename/removal/type-change without walking every row
            // of a potentially large array.
            if let Some(first) = items.first() {
                let path = format!("{prefix}[]");
                match first {
                    Value::Object(_) | Value::Array(_) => walk(first, &path, depth + 1, out),
                    _ => {
                        out.insert(format!("{path}:{}", json_type_name(first)));
                    }
                }
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.insert(format!("{prefix}:{}", json_type_name(v)));
            }
        }
    }
}

/// Fingerprints a JSON response body as a sorted, deduplicated list of
/// `"path:type"` strings, e.g. `["amount:number", "customer.email:string",
/// "items[].sku:string"]`. Bounded (`MAX_DEPTH`/`MAX_PATHS`) so a
/// pathological payload can't make this expensive. `None` for anything
/// that isn't a JSON object at the top level (nothing meaningful to
/// fingerprint).
pub fn fingerprint(body: &Value) -> Option<Vec<String>> {
    if !matches!(body, Value::Object(_)) {
        return None;
    }
    let mut out = BTreeSet::new();
    walk(body, "", 0, &mut out);
    Some(out.into_iter().collect())
}

fn field_and_type(entry: &str) -> (&str, &str) {
    entry.rsplit_once(':').unwrap_or((entry, ""))
}

pub struct DriftResult {
    /// Paths present in the baseline but missing from this response.
    pub removed: Vec<String>,
    /// Paths present in both, but with a different type.
    pub type_changed: Vec<String>,
    /// The baseline as it should be going forward — the old baseline
    /// unioned with any newly-seen (benign) fields, and reflecting the
    /// new type for any type-changed field so the same drift doesn't
    /// re-trigger every subsequent request.
    pub next_baseline: Vec<String>,
}

/// Compares a new fingerprint against the current baseline. Adding a
/// field is always benign (real APIs evolve additively) and is folded
/// into `next_baseline` without being reported as drift; a field
/// disappearing or changing type is real drift.
pub fn compare(baseline: &[String], current: &[String]) -> DriftResult {
    use std::collections::BTreeMap;

    let baseline_fields: BTreeMap<&str, &str> = baseline.iter().map(|e| field_and_type(e)).collect();
    let current_fields: BTreeMap<&str, &str> = current.iter().map(|e| field_and_type(e)).collect();

    let mut removed = Vec::new();
    let mut type_changed = Vec::new();

    for (field, base_type) in &baseline_fields {
        match current_fields.get(field) {
            None => removed.push(format!("{field}:{base_type}")),
            Some(cur_type) if cur_type != base_type => {
                type_changed.push(format!("{field}: {base_type} -> {cur_type}"));
            }
            _ => {}
        }
    }

    // Next baseline = every field currently present, plus any baseline
    // field not present now kept too (so a transient hiccup doesn't
    // silently drop it from history) - deduped, sorted.
    let mut next: BTreeSet<String> = current.iter().cloned().collect();
    for entry in baseline {
        next.insert(entry.clone());
    }
    // But a type-changed field's OLD entry must not linger in
    // next_baseline (it would re-trigger every request otherwise) -
    // remove the stale-typed entry, the current-typed one is already in
    // from `current`.
    for (field, base_type) in &baseline_fields {
        if let Some(cur_type) = current_fields.get(field) {
            if cur_type != base_type {
                next.remove(&format!("{field}:{base_type}"));
            }
        }
    }

    DriftResult { removed, type_changed, next_baseline: next.into_iter().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprints_flat_object() {
        let body = json!({"id": "abc", "amount": 100, "paid": true});
        let fp = fingerprint(&body).unwrap();
        assert_eq!(fp, vec!["amount:number", "id:string", "paid:bool"]);
    }

    #[test]
    fn fingerprints_nested_and_arrays() {
        let body = json!({"customer": {"email": "a@b.com"}, "items": [{"sku": "X"}]});
        let fp = fingerprint(&body).unwrap();
        assert_eq!(fp, vec!["customer.email:string", "items[].sku:string"]);
    }

    #[test]
    fn non_object_top_level_has_no_fingerprint() {
        assert_eq!(fingerprint(&json!([1, 2, 3])), None);
        assert_eq!(fingerprint(&json!("plain string")), None);
    }

    #[test]
    fn first_seen_has_no_baseline_to_compare_against_is_caller_responsibility() {
        // compare() always needs a baseline; the "first observation, no
        // drift" case is handled by the caller checking for an existing
        // baseline row before calling compare() at all - see
        // crates/api/src/schema_drift.rs.
        let fp = fingerprint(&json!({"a": 1})).unwrap();
        let result = compare(&fp, &fp);
        assert!(result.removed.is_empty());
        assert!(result.type_changed.is_empty());
    }

    #[test]
    fn added_field_is_benign_and_folded_into_next_baseline() {
        let baseline = vec!["id:string".to_string()];
        let current = vec!["id:string".to_string(), "new_field:number".to_string()];
        let result = compare(&baseline, &current);
        assert!(result.removed.is_empty());
        assert!(result.type_changed.is_empty());
        assert!(result.next_baseline.contains(&"new_field:number".to_string()));
    }

    #[test]
    fn removed_field_is_flagged() {
        let baseline = vec!["id:string".to_string(), "amount:number".to_string()];
        let current = vec!["id:string".to_string()];
        let result = compare(&baseline, &current);
        assert_eq!(result.removed, vec!["amount:number"]);
        assert!(result.type_changed.is_empty());
    }

    #[test]
    fn type_changed_field_is_flagged_and_stale_entry_dropped_from_next_baseline() {
        let baseline = vec!["amount:number".to_string()];
        let current = vec!["amount:string".to_string()];
        let result = compare(&baseline, &current);
        assert_eq!(result.type_changed, vec!["amount: number -> string"]);
        assert!(!result.next_baseline.contains(&"amount:number".to_string()));
        assert!(result.next_baseline.contains(&"amount:string".to_string()));
    }
}
