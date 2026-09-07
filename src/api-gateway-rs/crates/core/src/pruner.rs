//! Tool Result & Context Pruner (opt-in per org, Community + Enterprise
//! both get it — this is a cost-saving proxy feature, not a security/
//! compliance one, so unlike `dlp`/`output_sanitize` it isn't gated behind
//! the `enterprise` Cargo feature). Shrinks a large upstream tool response
//! before it reaches the calling agent: strips HTML tags, drops null/empty
//! fields, truncates long strings, and caps array length. New for this
//! feature set, no `src/ee` equivalent to mirror.
//!
//! ponytail: fixed per-field/per-array caps approximate the "150KB -> 3KB"
//! goal rather than enforcing an exact byte budget. If a customer needs a
//! hard total-size guarantee, add a final whole-payload length check that
//! re-truncates strings proportionally.

use serde_json::{Map, Value};

const MAX_STRING_CHARS: usize = 2000;
const MAX_ARRAY_ITEMS: usize = 50;

fn strip_html_tags(s: &str) -> String {
    if !s.contains('<') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_str(s: &str) -> String {
    let char_count = s.chars().count();
    if char_count <= MAX_STRING_CHARS {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX_STRING_CHARS).collect();
    format!("{head}...[truncated {} more chars]", char_count - MAX_STRING_CHARS)
}

fn walk(v: &Value) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(Value::String(truncate_str(&strip_html_tags(s)))),
        Value::Array(arr) => {
            let total = arr.len();
            let mut pruned: Vec<Value> = arr.iter().take(MAX_ARRAY_ITEMS).filter_map(walk).collect();
            if total > MAX_ARRAY_ITEMS {
                pruned.push(Value::String(format!("...[{} more items truncated]", total - MAX_ARRAY_ITEMS)));
            }
            Some(Value::Array(pruned))
        }
        Value::Object(obj) => {
            let pruned: Map<String, Value> = obj.iter().filter_map(|(k, v)| walk(v).map(|v| (k.clone(), v))).collect();
            Some(Value::Object(pruned))
        }
        other => Some(other.clone()),
    }
}

/// Applied only to what's actually surfaced to the calling agent — never to
/// fields extracted for internal tracking/audit (e.g. `upstream_id`), same
/// rule `output_sanitize::sanitize_output` follows.
pub fn prune_output(value: &Value) -> Value {
    walk(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_html_and_drops_empties() {
        let input = json!({
            "title": "<b>Hello</b> <i>World</i>",
            "note": "",
            "ignored": null,
            "items": (0..60).map(|i| json!(i)).collect::<Vec<_>>(),
        });
        let out = prune_output(&input);
        assert_eq!(out["title"], json!("Hello World"));
        assert!(out.get("note").is_none());
        assert!(out.get("ignored").is_none());
        assert_eq!(out["items"].as_array().unwrap().len(), MAX_ARRAY_ITEMS + 1);
    }

    #[test]
    fn truncates_long_strings() {
        let long = "x".repeat(MAX_STRING_CHARS + 500);
        let out = prune_output(&json!({ "body": long }));
        let s = out["body"].as_str().unwrap();
        assert!(s.len() < long.len());
        assert!(s.contains("truncated"));
    }
}
