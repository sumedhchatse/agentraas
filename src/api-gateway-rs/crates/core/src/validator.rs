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
        let value = payload.get(field_name);
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
pub fn is_valid_dedup_rule_definition(fields: &Value) -> Option<String> {
    let Some(arr) = fields.as_array() else {
        return Some("At least one field name is required.".to_string());
    };
    if arr.is_empty() {
        return Some("At least one field name is required.".to_string());
    }
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
