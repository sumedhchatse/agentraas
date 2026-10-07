//! Undo log, pure part: how an action that ran is reversed. A curated
//! action (`config/services.json`) or a custom action names its reverse
//! action and how to fill it from the original call, e.g. a Stripe charge:
//!
//! ```json
//! "undo": { "action": "charge.refund", "with": { "charge": "response.id" } }
//! ```
//!
//! `response.<path>` reads the upstream's response, `payload.<path>` the
//! original payload (dotted paths, arrays by index). The DB side (recording
//! and running undos) is `crates/api/src/undo.rs`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndoSpec {
    /// The reverse action, in the same service (or another custom action).
    pub action: String,
    /// Undo payload field -> where its value comes from.
    #[serde(default)]
    pub with: BTreeMap<String, String>,
}

/// Why a spec can't be used, checked when it is saved.
pub fn validate(spec: &UndoSpec) -> Result<(), String> {
    if spec.action.trim().is_empty() {
        return Err("undo.action must name the action that reverses this one".into());
    }
    for (field, source) in &spec.with {
        if field.is_empty() {
            return Err("undo.with has an empty field name".into());
        }
        let ok = source
            .split_once('.')
            .is_some_and(|(root, rest)| (root == "response" || root == "payload") && !rest.is_empty());
        if !ok {
            return Err(format!("undo.with.{field}: \"{source}\" must start with response. or payload."));
        }
    }
    Ok(())
}

fn lookup<'a>(mut v: &'a Value, path: &str) -> Option<&'a Value> {
    for part in path.split('.') {
        v = match v {
            Value::Object(m) => m.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    (!v.is_null()).then_some(v)
}

/// The reverse action's payload, or `None` if any value it needs is
/// missing (then the action is recorded as not undoable rather than
/// undone with a half-empty payload). `response` is the upstream's body.
pub fn build_payload(spec: &UndoSpec, payload: &Value, response: &Value) -> Option<Value> {
    let mut out = Map::new();
    for (field, source) in &spec.with {
        let (root, path) = source.split_once('.')?;
        let from = match root {
            "response" => response,
            "payload" => payload,
            _ => return None,
        };
        out.insert(field.clone(), lookup(from, path)?.clone());
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(action: &str, with: &[(&str, &str)]) -> UndoSpec {
        UndoSpec { action: action.into(), with: with.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect() }
    }

    #[test]
    fn fills_from_response_and_payload() {
        let s = spec("message.delete", &[("channel", "response.channel"), ("ts", "response.ts"), ("note", "payload.meta.reason")]);
        let out = build_payload(&s, &json!({"meta": {"reason": "oops"}}), &json!({"ok": true, "channel": "C1", "ts": "171.2"})).unwrap();
        assert_eq!(out, json!({"channel": "C1", "ts": "171.2", "note": "oops"}));
    }

    #[test]
    fn missing_or_null_value_means_not_undoable() {
        let s = spec("charge.refund", &[("charge", "response.id")]);
        assert_eq!(build_payload(&s, &json!({}), &json!({"error": "x"})), None);
        assert_eq!(build_payload(&s, &json!({}), &json!({"id": null})), None);
    }

    #[test]
    fn array_index_and_empty_with() {
        let s = spec("x", &[("id", "response.items.1.id")]);
        assert_eq!(build_payload(&s, &json!({}), &json!({"items": [{"id": 1}, {"id": 2}]})), Some(json!({"id": 2})));
        assert_eq!(build_payload(&spec("x", &[]), &json!({}), &json!({})), Some(json!({})));
    }

    #[test]
    fn validate_rejects_bad_sources() {
        assert!(validate(&spec("refund", &[("charge", "response.id")])).is_ok());
        assert!(validate(&spec("", &[])).is_err());
        assert!(validate(&spec("refund", &[("charge", "id")])).is_err());
        assert!(validate(&spec("refund", &[("charge", "headers.id")])).is_err());
        assert!(validate(&spec("refund", &[("charge", "response.")])).is_err());
    }
}
