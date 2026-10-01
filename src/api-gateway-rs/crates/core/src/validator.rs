//! Ports `validateFields` from `src/api-gateway/validator.js` exactly,
//! including field-iteration order (first failing field wins — matches
//! `Object.entries` on the JSON `fields` rule, hence the `preserve_order`
//! serde_json feature this crate already needs for dedup hashing).

use serde_json::Value;

/// Returns `Some(error message)` for the first field that fails, or `None`
/// if the payload passes every rule.
pub fn validate_fields(payload: &Value, fields: &Value) -> Option<String> {
    let fields_obj = fields.as_object()?;

    for (field_name, field_rules) in fields_obj {
        // "*" names the whole payload (for rules like no_secrets).
        let value = if field_name == "*" { Some(payload) } else { payload.get(field_name) };
        let is_null_ish = matches!(value, None | Some(Value::Null))
            || matches!(value, Some(Value::String(s)) if s.is_empty());

        if field_rules.get("required").and_then(Value::as_bool).unwrap_or(false) && is_null_ish {
            return Some(format!("{field_name} is required"));
        }

        let Some(value) = value else { continue };
        if value.is_null() {
            continue;
        }

        if let Some(expected_type) = field_rules.get("type").and_then(Value::as_str) {
            let actual_type = json_type_name(value);
            if actual_type != expected_type {
                return Some(format!("{field_name} must be {expected_type}, got {actual_type}"));
            }
        }

        let expects_number = field_rules.get("type").and_then(Value::as_str) == Some("number") || value.is_number();
        if expects_number {
            if let Some(v) = value.as_f64() {
                if let Some(min) = field_rules.get("min").and_then(Value::as_f64) {
                    if v < min {
                        return Some(format!("{field_name} must be at least {}", fmt_num(min)));
                    }
                }
                if let Some(max) = field_rules.get("max").and_then(Value::as_f64) {
                    if v > max {
                        return Some(format!("{field_name} must be at most {}", fmt_num(max)));
                    }
                }
                // Preconditions referencing another field in the SAME
                // payload, not a fixed literal — e.g. "refund_amount must
                // not exceed balance" where balance is only known per
                // request. A missing/non-numeric referenced field is
                // silently skipped (nothing to compare against), not an
                // error — same "can't be checked, so don't block" stance
                // as every other rule here when a value is absent.
                if let Some(max_field) = field_rules.get("maxField").and_then(Value::as_str) {
                    if let Some(other) = payload.get(max_field).and_then(Value::as_f64) {
                        if v > other {
                            return Some(format!("{field_name} ({}) must not exceed {max_field} ({})", fmt_num(v), fmt_num(other)));
                        }
                    }
                }
                if let Some(min_field) = field_rules.get("minField").and_then(Value::as_str) {
                    if let Some(other) = payload.get(min_field).and_then(Value::as_f64) {
                        if v < other {
                            return Some(format!("{field_name} ({}) must be at least {min_field} ({})", fmt_num(v), fmt_num(other)));
                        }
                    }
                }
            }
        }

        if field_rules.get("no_secrets").and_then(Value::as_bool) == Some(true) {
            if let Some(kind) = find_secret(value) {
                // Never echo the secret itself back.
                return Some(format!("{field_name} contains what looks like a {kind}; refusing to send it"));
            }
        }

        // Allowed domains for an email address or URL (or a list of them,
        // e.g. an email API's `to: [...]`): exact domain or a subdomain.
        if let Some(domains) = field_rules.get("domains").and_then(Value::as_array) {
            let allowed: Vec<&str> = domains.iter().filter_map(Value::as_str).collect();
            let values: Vec<&str> = match value {
                Value::String(s) => vec![s.as_str()],
                Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
                _ => vec![],
            };
            if let Some(bad) = values.iter().find(|v| !domain_allowed(v, &allowed)) {
                return Some(format!("{field_name} ({bad}) is not in an allowed domain: {}", allowed.join(", ")));
            }
        }

        if let Value::String(s) = value {
            if let Some(max_len) = field_rules.get("maxLength").and_then(Value::as_u64) {
                if s.chars().count() as u64 > max_len {
                    return Some(format!("{field_name} must be at most {max_len} characters"));
                }
            }
            if let Some(min_len) = field_rules.get("minLength").and_then(Value::as_u64) {
                if (s.chars().count() as u64) < min_len {
                    return Some(format!("{field_name} must be at least {min_len} characters"));
                }
            }
            if field_rules.get("format").and_then(Value::as_str) == Some("email") && !is_valid_email_format(s) {
                return Some(format!("{field_name} must be a valid email"));
            }
            if field_rules.get("format").and_then(Value::as_str) == Some("e164") && !is_valid_e164(s) {
                return Some(format!(
                    "{field_name} must be a valid E.164 phone number (e.g. +14155552671)"
                ));
            }
            if let Some(enum_values) = field_rules.get("enum").and_then(Value::as_array) {
                let allowed: Vec<&str> = enum_values.iter().filter_map(Value::as_str).collect();
                if !allowed.contains(&s.as_str()) {
                    return Some(format!("{field_name} must be one of: {}", allowed.join(", ")));
                }
            }
        }

        if let Value::Array(arr) = value {
            if let Some(min_len) = field_rules.get("minLength").and_then(Value::as_u64) {
                if (arr.len() as u64) < min_len {
                    return Some(format!("{field_name} must have at least {min_len} items"));
                }
            }
        }
    }

    None
}

/// Credential formats an agent should never send out: a prompt-injected
/// agent's usual goal is to leak one. (prefix, min token chars after it, name)
const SECRET_PREFIXES: [(&str, usize, &str); 14] = [
    ("sk_live_", 16, "Stripe secret key"),
    ("rk_live_", 16, "Stripe restricted key"),
    ("whsec_", 16, "webhook signing secret"),
    ("sk-ant-", 20, "Anthropic API key"),
    ("sk-proj-", 20, "OpenAI API key"),
    ("sk-", 32, "API secret key"),
    ("AKIA", 16, "AWS access key"),
    ("ghp_", 30, "GitHub token"),
    ("gho_", 30, "GitHub token"),
    ("ghs_", 30, "GitHub token"),
    ("github_pat_", 30, "GitHub token"),
    ("xoxb-", 20, "Slack token"),
    ("xoxp-", 20, "Slack token"),
    ("ar_live_", 16, "AgentRaaS API key"),
];

fn find_secret(v: &Value) -> Option<&'static str> {
    match v {
        Value::String(s) => find_secret_in_str(s),
        Value::Array(a) => a.iter().find_map(find_secret),
        Value::Object(o) => o.values().find_map(find_secret),
        _ => None,
    }
}

fn find_secret_in_str(s: &str) -> Option<&'static str> {
    if s.contains("PRIVATE KEY-----") {
        return Some("private key");
    }
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    for (prefix, min_len, kind) in SECRET_PREFIXES {
        for (i, _) in s.match_indices(prefix) {
            // Must start a token, not sit inside a longer word ("task-..." isn't "sk-").
            let starts_token = s[..i].chars().next_back().map_or(true, |c| !token_char(c));
            let tail = s[i + prefix.len()..].chars().take_while(|c| token_char(*c)).count();
            if starts_token && tail >= min_len {
                return Some(kind);
            }
        }
    }
    None
}

/// The host an email address or URL points at, lowercased: the part after
/// the last `@` for an email, the authority minus userinfo/port for a URL,
/// otherwise the string itself (a bare domain).
pub fn host_of(s: &str) -> String {
    let s = s.trim();
    let host = if let Some((_, rest)) = s.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let authority = authority.rsplit('@').next().unwrap_or("");
        authority.split(':').next().unwrap_or("")
    } else {
        s.rsplit('@').next().unwrap_or("")
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn domain_allowed(s: &str, allowed: &[&str]) -> bool {
    let host = host_of(s);
    !host.is_empty()
        && allowed.iter().any(|d| {
            let d = d.trim().trim_start_matches('.').to_ascii_lowercase();
            host == d || host.ends_with(&format!(".{d}"))
        })
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Null => "null",
    }
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

fn is_valid_email_format(s: &str) -> bool {
    let mut parts = s.splitn(2, '@');
    match (parts.next(), parts.next()) {
        (Some(local), Some(domain)) if !local.is_empty() && !domain.is_empty() => {
            !local.chars().any(char::is_whitespace)
                && !domain.chars().any(char::is_whitespace)
                && domain.contains('.')
                && domain.rsplit_once('.').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
        }
        _ => false,
    }
}

const VALID_FIELD_TYPES: [&str; 5] = ["string", "number", "boolean", "array", "object"];

/// Structural sanity check for a rule definition submitted from the
/// dashboard's Validation Rules builder — rejects nonsense before it's
/// ever stored.
pub fn is_valid_rule_definition(fields: &Value) -> Option<String> {
    let Some(obj) = fields.as_object() else {
        return Some("At least one field rule is required.".to_string());
    };
    if obj.is_empty() {
        return Some("At least one field rule is required.".to_string());
    }
    for (field_name, field_rules) in obj {
        if field_name.is_empty() || field_name.len() > 100 {
            return Some(format!("\"{field_name}\" is not a valid field name."));
        }
        let Some(rules) = field_rules.as_object() else {
            return Some(format!("Field \"{field_name}\" must have a rules object."));
        };
        if let Some(t) = rules.get("type").and_then(Value::as_str) {
            if !VALID_FIELD_TYPES.contains(&t) {
                return Some(format!(
                    "Field \"{field_name}\": type must be one of {}.",
                    VALID_FIELD_TYPES.join(", ")
                ));
            }
        }
        let min = rules.get("min").filter(|v| !v.is_null());
        let max = rules.get("max").filter(|v| !v.is_null());
        if min.is_some_and(|v| !v.is_number()) {
            return Some(format!("Field \"{field_name}\": min must be a number."));
        }
        if max.is_some_and(|v| !v.is_number()) {
            return Some(format!("Field \"{field_name}\": max must be a number."));
        }
        if let (Some(min), Some(max)) = (min.and_then(Value::as_f64), max.and_then(Value::as_f64)) {
            if min > max {
                return Some(format!("Field \"{field_name}\": min cannot be greater than max."));
            }
        }
        for key in ["minField", "maxField"] {
            if let Some(v) = rules.get(key).filter(|v| !v.is_null()) {
                let valid_name = v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 100);
                if !valid_name {
                    return Some(format!("Field \"{field_name}\": {key} must be a non-empty field name."));
                }
            }
        }
        let min_len = rules.get("minLength").filter(|v| !v.is_null());
        let max_len = rules.get("maxLength").filter(|v| !v.is_null());
        if min_len.is_some_and(|v| !v.as_f64().is_some_and(|n| n >= 0.0)) {
            return Some(format!("Field \"{field_name}\": minLength must be a non-negative number."));
        }
        if max_len.is_some_and(|v| !v.as_f64().is_some_and(|n| n >= 0.0)) {
            return Some(format!("Field \"{field_name}\": maxLength must be a non-negative number."));
        }
        if let (Some(min_len), Some(max_len)) = (min_len.and_then(Value::as_f64), max_len.and_then(Value::as_f64)) {
            if min_len > max_len {
                return Some(format!("Field \"{field_name}\": minLength cannot be greater than maxLength."));
            }
        }
        if let Some(format) = rules.get("format").and_then(Value::as_str).filter(|_| rules.get("format").is_some_and(|v| !v.is_null())) {
            if !["email", "e164"].contains(&format) {
                return Some(format!("Field \"{field_name}\": format only supports \"email\" or \"e164\" currently."));
            }
        }
        for key in ["no_secrets", "new_destination"] {
            if rules.get(key).is_some_and(|v| !v.is_null() && !v.is_boolean()) {
                return Some(format!("Field \"{field_name}\": {key} must be true or false."));
            }
        }
        if let Some(domains) = rules.get("domains").filter(|v| !v.is_null()) {
            let valid = domains
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(|d| d.as_str().is_some_and(|s| !s.trim().is_empty() && !s.contains(['@', '/']))));
            if !valid {
                return Some(format!("Field \"{field_name}\": domains must be a non-empty list of domain names (like \"acme.com\")."));
            }
        }
        if let Some(enum_val) = rules.get("enum").filter(|v| !v.is_null()) {
            let valid = enum_val
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_string));
            if !valid {
                return Some(format!("Field \"{field_name}\": enum must be a non-empty array of strings."));
            }
        }
    }
    None
}

const MAX_DEDUP_FIELDS: usize = 10;

/// Structural sanity check for a per-field dedup rule — just a list of
/// field names, so much thinner than `is_valid_rule_definition`.
/// An empty field list is valid — it means "TTL-only rule": no field-based
/// dedup key, just a custom TTL override on the default whole-payload hash.
pub fn is_valid_dedup_rule_definition(fields: &Value) -> Option<String> {
    let Some(arr) = fields.as_array() else {
        return Some("fields must be an array (use [] for a TTL-only rule).".to_string());
    };
    if arr.len() > MAX_DEDUP_FIELDS {
        return Some(format!("At most {MAX_DEDUP_FIELDS} fields can be used for a dedup key."));
    }
    let mut seen = std::collections::HashSet::new();
    for f in arr {
        let Some(s) = f.as_str() else {
            return Some(format!("\"{f}\" is not a valid field name."));
        };
        if s.is_empty() || s.len() > 100 {
            return Some(format!("\"{s}\" is not a valid field name."));
        }
        if !seen.insert(s) {
            return Some(format!("Field \"{s}\" is listed more than once."));
        }
    }
    None
}

/// `^\+[1-9]\d{7,14}$` — a leading "+", 8-15 digits total, no leading zero
/// after the "+".
fn is_valid_e164(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('+') else {
        return false;
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if rest.starts_with('0') {
        return false;
    }
    (8..=15).contains(&rest.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_fields_is_valid_ttl_only_rule() {
        assert_eq!(is_valid_dedup_rule_definition(&json!([])), None);
    }

    #[test]
    fn non_array_is_rejected() {
        assert!(is_valid_dedup_rule_definition(&json!("not an array")).is_some());
    }

    #[test]
    fn too_many_fields_is_rejected() {
        let fields: Vec<String> = (0..=MAX_DEDUP_FIELDS).map(|i| format!("f{i}")).collect();
        assert!(is_valid_dedup_rule_definition(&json!(fields)).is_some());
    }

    #[test]
    fn duplicate_field_is_rejected() {
        assert!(is_valid_dedup_rule_definition(&json!(["a", "a"])).is_some());
    }

    #[test]
    fn non_string_element_is_rejected() {
        assert!(is_valid_dedup_rule_definition(&json!(["a", 1])).is_some());
    }

    #[test]
    fn normal_field_list_is_valid() {
        assert_eq!(is_valid_dedup_rule_definition(&json!(["order_id", "customer_id"])), None);
    }

    #[test]
    fn max_field_precondition_rejects_when_over() {
        let fields = json!({ "refund_amount": { "type": "number", "maxField": "balance" } });
        let payload = json!({ "refund_amount": 150, "balance": 100 });
        let err = validate_fields(&payload, &fields).unwrap();
        assert!(err.contains("refund_amount"));
        assert!(err.contains("balance"));
    }

    #[test]
    fn max_field_precondition_passes_when_within() {
        let fields = json!({ "refund_amount": { "type": "number", "maxField": "balance" } });
        let payload = json!({ "refund_amount": 50, "balance": 100 });
        assert_eq!(validate_fields(&payload, &fields), None);
    }

    #[test]
    fn min_field_precondition_rejects_when_under() {
        let fields = json!({ "deposit": { "type": "number", "minField": "minimum_required" } });
        let payload = json!({ "deposit": 5, "minimum_required": 10 });
        assert!(validate_fields(&payload, &fields).is_some());
    }

    #[test]
    fn field_precondition_skipped_when_referenced_field_missing() {
        // Nothing to compare against — same "can't check it, don't block"
        // stance as every other rule when a value is absent.
        let fields = json!({ "refund_amount": { "type": "number", "maxField": "balance" } });
        let payload = json!({ "refund_amount": 150 });
        assert_eq!(validate_fields(&payload, &fields), None);
    }

    #[test]
    fn max_field_definition_requires_non_empty_string() {
        assert!(is_valid_rule_definition(&json!({ "amount": { "type": "number", "maxField": "" } })).is_some());
        assert!(is_valid_rule_definition(&json!({ "amount": { "type": "number", "maxField": "balance" } })).is_none());
    }

    #[test]
    fn domains_rule_matches_email_url_and_subdomain_only() {
        let fields = json!({ "to": { "domains": ["acme.com"] } });
        for ok in ["bob@acme.com", "Bob@Mail.ACME.com", "https://acme.com/x", "https://u:p@api.acme.com:8443/y"] {
            assert_eq!(validate_fields(&json!({ "to": ok }), &fields), None, "{ok}");
        }
        for bad in ["bob@evilacme.com", "bob@acme.com.evil.io", "https://acme.com@evil.io/", "evil.io", ""] {
            assert!(validate_fields(&json!({ "to": bad }), &fields).is_some(), "{bad}");
        }
        assert_eq!(validate_fields(&json!({ "to": ["a@acme.com", "b@x.acme.com"] }), &fields), None);
        assert!(validate_fields(&json!({ "to": ["a@acme.com", "b@gmail.com"] }), &fields).is_some());
    }

    #[test]
    fn domains_definition_must_be_plain_domain_names() {
        assert!(is_valid_rule_definition(&json!({ "to": { "domains": [] } })).is_some());
        assert!(is_valid_rule_definition(&json!({ "to": { "domains": ["a@acme.com"] } })).is_some());
        assert!(is_valid_rule_definition(&json!({ "to": { "domains": ["acme.com"] } })).is_none());
    }

    #[test]
    fn no_secrets_finds_keys_anywhere_without_echoing_them() {
        let fields = json!({ "*": { "no_secrets": true } });
        let key = format!("sk_live_{}", "a1B2c3D4e5F6g7H8");
        let payload = json!({ "to": "a@b.com", "body": { "lines": ["hi", format!("here: {key} thanks")] } });
        let err = validate_fields(&payload, &fields).unwrap();
        assert!(err.contains("Stripe secret key") && !err.contains(&key), "{err}");
        assert!(validate_fields(&json!({ "k": "-----BEGIN RSA PRIVATE KEY-----" }), &fields).is_some());
        assert!(validate_fields(&json!({ "k": format!("AKIA{}", "ABCDEFGHIJKLMNOP") }), &fields).is_some());
    }

    #[test]
    fn no_secrets_ignores_lookalikes() {
        let fields = json!({ "body": { "no_secrets": true } });
        for ok in ["ask-me-anything-about-this-product-please-now-ok", "task-1234", "sk_live_short", "Refund for order sk_test_123"] {
            assert_eq!(validate_fields(&json!({ "body": ok }), &fields), None, "{ok}");
        }
    }
}
