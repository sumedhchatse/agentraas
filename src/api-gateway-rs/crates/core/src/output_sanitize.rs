//! Tool Output Sanitization (Enterprise, opt-in per org). Neutralizes
//! two categories of risk in an upstream tool response before it reaches
//! the calling agent: leaked PII (reuses `dlp::redact_pii` verbatim) and
//! heuristic prompt-injection markers an upstream could plant to hijack the
//! agent reading the response (role-spoofing line prefixes, "ignore
//! previous instructions"-style phrasing, and fenced code blocks).
//!
//! The injection ruleset is inherently fuzzy — a first pass, not a
//! guarantee. Expect false positives on legitimate technical content that
//! happens to contain a code fence or a line starting with "system:".
//! Ships opt-in per org for exactly this reason (see `crates/api/src/ee/
//! output_sanitization.rs`), never default-on.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

static IGNORE_INSTRUCTIONS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:ignore|disregard|forget)\s+(?:all\s+|any\s+)?(?:previous|prior|above|earlier)\s+instructions?\b")
        .unwrap()
});
static ROLE_SPOOF: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?im)^[ \t]*(?:system|assistant|user)[ \t]*:").unwrap());
static CODE_FENCE: Lazy<Regex> = Lazy::new(|| Regex::new(r"```[\s\S]*?```").unwrap());

fn redact_prompt_injection(text: &str) -> String {
    let text = IGNORE_INSTRUCTIONS.replace_all(text, "[REDACTED:PROMPT_INJECTION]");
    let text = ROLE_SPOOF.replace_all(&text, "[REDACTED:ROLE_SPOOF]");
    CODE_FENCE.replace_all(&text, "[REDACTED:CODE_FENCE]").into_owned()
}

fn walk(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(redact_prompt_injection(s)),
        Value::Array(arr) => Value::Array(arr.iter().map(walk).collect()),
        Value::Object(obj) => Value::Object(obj.iter().map(|(k, v)| (k.clone(), walk(v))).collect()),
        other => other.clone(),
    }
}

/// PII redaction first (reusing `dlp::redact_pii`'s existing tree-walk),
/// then the prompt-injection heuristic pass over the result.
pub fn sanitize_output(value: &Value) -> Value {
    walk(&crate::dlp::redact_pii(value))
}
