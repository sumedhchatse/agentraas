//! Regex-based PII/DLP redaction — mirrors `src/ee/dlp/index.js`. Covers
//! credit card numbers (format + Luhn checksum), US SSNs, and common
//! API-key/secret shapes. Never mutates the input; produces a new `Value`
//! for audit-log storage. The original, unredacted payload is still what
//! gets forwarded downstream — DLP protects what gets WRITTEN and STORED.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

// The obvious `\b(?:\d[ -]?){13,19}\b` greedily consumes a TRAILING
// space/dash (e.g. "4111111111111111 ignore..." swallows the space before
// "ignore"), which breaks `\b`-anchored matches right after it, and Tool
// Output Sanitization's prompt-injection patterns rely on that boundary.
// So every separator must sit strictly BETWEEN two digits: one leading
// digit, then 12-18 more "optional separator + digit" units, so the match
// always ends on a digit.
static CARD_CANDIDATE_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b\d(?:[ -]?\d){12,18}\b").unwrap());

pub fn luhn_check(candidate: &str) -> bool {
    let digits: Vec<u32> = candidate.chars().filter_map(|c| c.to_digit(10)).collect();
    if digits.len() < 12 || digits.len() > 19 {
        return false;
    }
    let mut sum = 0u32;
    let mut alternate = false;
    for &d in digits.iter().rev() {
        let mut n = d;
        if alternate {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        alternate = !alternate;
    }
    sum % 10 == 0
}

fn mask_card_number(digits: &str) -> String {
    if digits.chars().count() > 4 {
        let last4: String = digits.chars().skip(digits.chars().count() - 4).collect();
        format!("{}{}", "*".repeat(digits.chars().count() - 4), last4)
    } else {
        "****".to_string()
    }
}

pub fn redact_credit_cards(text: &str) -> String {
    CARD_CANDIDATE_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let m = &caps[0];
            if !luhn_check(m) {
                return m.to_string();
            }
            let digits: String = m.chars().filter(|c| c.is_ascii_digit()).collect();
            mask_card_number(&digits)
        })
        .into_owned()
}

// SSA-invalid-range exclusions (000/666/900-999 area, 00 group, 0000
// serial), plus the two specific widely-known leaked/retired SSNs that show
// up constantly in test data (matching them would almost certainly be a
// false positive from sample data, not a real SSN). Rust's `regex` crate
// has no lookaround, so the three groups are captured and the exclusions
// applied as a post-match filter.
static SSN_PATTERN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{3})[-\s]?(\d{2})[-\s]?(\d{4})\b").unwrap());

pub fn redact_ssns(text: &str) -> String {
    const KNOWN_INVALID: &[&str] = &["078051120", "219099999"];
    SSN_PATTERN
        .replace_all(text, |caps: &regex::Captures| {
            let m = &caps[0];
            let area = &caps[1];
            let group = &caps[2];
            let serial = &caps[3];
            let invalid_area = area == "000" || area == "666" || area.starts_with('9');
            if invalid_area || group == "00" || serial == "0000" {
                return m.to_string();
            }
            let digits_only = format!("{area}{group}{serial}");
            if KNOWN_INVALID.contains(&digits_only.as_str()) {
                return m.to_string();
            }
            format!("***-**-{serial}")
        })
        .into_owned()
}

static STRIPE_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(?:sk|pk|rk)_(?:live|test)_[a-zA-Z0-9]{16,}\b").unwrap());
static GITHUB_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bgh[pousr]_[a-zA-Z0-9]{36,}\b").unwrap());
static AWS_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap());
static AGENTRAAS_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bar_live_[a-f0-9]{48}\b").unwrap());
static SLACK_KEY: Lazy<Regex> = Lazy::new(|| Regex::new(r"\bxox[baprs]-[a-zA-Z0-9-]{10,}\b").unwrap());

pub fn redact_api_keys(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in [&*STRIPE_KEY, &*GITHUB_KEY, &*AWS_KEY, &*AGENTRAAS_KEY, &*SLACK_KEY] {
        result = pattern
            .replace_all(&result, |caps: &regex::Captures| {
                let m = &caps[0];
                if m.len() > 8 {
                    format!("{}{}{}", &m[..4], "*".repeat(m.len() - 8), &m[m.len() - 4..])
                } else {
                    "****".to_string()
                }
            })
            .into_owned();
    }
    result
}

fn redact_string(s: &str) -> String {
    redact_api_keys(&redact_ssns(&redact_credit_cards(s)))
}

/// Walks a payload recursively (objects, arrays, and their string leaves),
/// applying all three redaction passes to every string value found.
pub fn redact_pii(payload: &Value) -> Value {
    match payload {
        Value::String(s) => Value::String(redact_string(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(redact_pii).collect()),
        Value::Object(obj) => Value::Object(obj.iter().map(|(k, v)| (k.clone(), redact_pii(v))).collect()),
        other => other.clone(),
    }
}
