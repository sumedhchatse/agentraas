//! Fuzzy/semantic similarity dedup — the layer exact-hash (`dedup::hash_payload`)
//! and field-hash (`dedup::hash_field_values`) dedup can't cover: two payloads
//! that mean the same thing but aren't byte- or field-identical, e.g.
//! "Transfer $500 to John" vs "Send five hundred dollars to John Doe".
//!
//! Deliberately **not** embedding/LLM-based: no external API, no per-request
//! cost, nothing that can go down. Instead: normalize each payload's text
//! into a token bag (numbers canonicalized, common number-words converted to
//! digits, case/whitespace collapsed), then compare via Jaccard similarity
//! against a small rolling window of recent requests in the same scope
//! (same api_key+service+action+end_user_id dedup normally uses). A match
//! above the configured threshold is treated as a duplicate of whichever
//! prior request it matched, reusing that request's existing `dedup:{hash}`
//! Redis slot — no changes needed to the claim/complete/response flow at all.
//!
//! ponytail: word-number conversion only covers common English cardinals up
//! to millions, no ordinals/decimals-in-words/other languages. Upgrade path
//! if this proves insufficient: swap `normalize_for_similarity`'s output for
//! a real embedding vector + cosine similarity behind the same interface —
//! everything downstream (the rolling window, the threshold, the wiring in
//! `agent/mod.rs`) stays the same.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

use crate::dedup::sha256_hex;

const DEFAULT_WINDOW_TTL_SECONDS: i64 = 86400;
const WINDOW_CAP: isize = 20;

fn word_to_digit(word: &str) -> Option<u64> {
    Some(match word {
        "zero" => 0, "one" => 1, "two" => 2, "three" => 3, "four" => 4,
        "five" => 5, "six" => 6, "seven" => 7, "eight" => 8, "nine" => 9,
        "ten" => 10, "eleven" => 11, "twelve" => 12, "thirteen" => 13,
        "fourteen" => 14, "fifteen" => 15, "sixteen" => 16, "seventeen" => 17,
        "eighteen" => 18, "nineteen" => 19, "twenty" => 20, "thirty" => 30,
        "forty" => 40, "fifty" => 50, "sixty" => 60, "seventy" => 70,
        "eighty" => 80, "ninety" => 90,
        _ => return None,
    })
}

fn multiplier(word: &str) -> Option<u64> {
    Some(match word {
        "hundred" => 100,
        "thousand" => 1_000,
        "million" => 1_000_000,
        _ => return None,
    })
}

/// Converts runs of English cardinal number-words to digits (e.g. "five
/// hundred" -> "500", "twenty one thousand" -> "21000"). Non-number tokens
/// pass through unchanged. No ordinals/decimals-in-words.
pub fn word_numbers_to_digits(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if word_to_digit(tokens[i]).is_none() && multiplier(tokens[i]).is_none() {
            out.push(tokens[i].to_string());
            i += 1;
            continue;
        }
        // Standard grouped-number parse: ones/tens accumulate into `current`,
        // "hundred" scales the current group, "thousand"/"million" close the
        // group into `result` at that magnitude and start a fresh group.
        let mut result: u64 = 0;
        let mut current: u64 = 0;
        let mut j = i;
        while j < tokens.len() {
            if let Some(v) = word_to_digit(tokens[j]) {
                current += v;
                j += 1;
            } else if let Some(m) = multiplier(tokens[j]) {
                if m == 100 {
                    current = if current == 0 { 100 } else { current * 100 };
                } else {
                    result += if current == 0 { m } else { current * m };
                    current = 0;
                }
                j += 1;
            } else {
                break;
            }
        }
        out.push((result + current).to_string());
        i = j;
    }
    out.join(" ")
}

fn push_leaf_text(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(&s.trim().to_lowercase());
            out.push(' ');
        }
        Value::Number(n) => {
            out.push_str(&n.to_string());
            out.push(' ');
        }
        Value::Bool(b) => {
            out.push_str(if *b { "true" } else { "false" });
            out.push(' ');
        }
        Value::Array(items) => {
            for item in items {
                push_leaf_text(item, out);
            }
        }
        Value::Object(map) => {
            for (_key, v) in map {
                push_leaf_text(v, out);
            }
        }
        Value::Null => {}
    }
}

/// Flattens a payload into a normalized token bag suitable for similarity
/// comparison: every string/number leaf value (field names are deliberately
/// excluded — "amount":500 and "total":500 should still be able to match),
/// lowercased/trimmed, with number-words converted to digits.
pub fn normalize_for_similarity(payload: &Value) -> String {
    let mut raw = String::new();
    push_leaf_text(payload, &mut raw);
    word_numbers_to_digits(raw.trim())
}

/// Token-set (not multiset) Jaccard similarity, 0.0-1.0. Two payloads with
/// no comparable tokens at all are treated as NOT similar (0.0), not as a
/// vacuous match.
pub fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let set_a: HashSet<&str> = a.split_whitespace().collect();
    let set_b: HashSet<&str> = b.split_whitespace().collect();
    if set_a.is_empty() && set_b.is_empty() {
        return 0.0;
    }
    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

#[derive(Serialize, Deserialize)]
struct WindowEntry {
    text: String,
    hash: String,
}

fn window_key(scope: &str) -> String {
    format!("semantic_window:{scope}")
}

/// Same scoping as the rest of dedup (api_key+service+action+end_user_id) so
/// semantic matching never crosses an isolation boundary the exact-hash path
/// already respects.
pub fn window_scope(api_key: &str, service: &str, action: &str, end_user_id: Option<&str>) -> String {
    sha256_hex(&format!("{api_key}\u{1}{service}\u{1}{action}\u{1}{}", end_user_id.unwrap_or("")))
}

/// Checks a normalized text against the scope's rolling window of recent
/// requests; returns the matched entry's dedup hash if any prior entry's
/// similarity is >= threshold. Fails open (returns `Ok(None)`, logs nothing
/// itself — caller decides) on any Redis error, since this is a best-effort
/// enhancement layer, not a correctness guarantee like exact-hash dedup.
pub async fn check_window(
    conn: &mut redis::aio::MultiplexedConnection,
    scope: &str,
    text: &str,
    threshold: f64,
) -> redis::RedisResult<Option<String>> {
    let key = window_key(scope);
    let raw: Vec<String> = redis::cmd("LRANGE").arg(&key).arg(0).arg(-1).query_async(conn).await?;
    for entry in raw {
        if let Ok(parsed) = serde_json::from_str::<WindowEntry>(&entry) {
            if jaccard_similarity(text, &parsed.text) >= threshold {
                return Ok(Some(parsed.hash));
            }
        }
    }
    Ok(None)
}

/// Records this request's normalized text + dedup hash into the scope's
/// rolling window, capped to `WINDOW_CAP` most-recent entries.
pub async fn record_window(
    conn: &mut redis::aio::MultiplexedConnection,
    scope: &str,
    text: &str,
    hash: &str,
    ttl_seconds: Option<i64>,
) -> redis::RedisResult<()> {
    let key = window_key(scope);
    let entry = serde_json::to_string(&WindowEntry { text: text.to_string(), hash: hash.to_string() })
        .expect("WindowEntry always serializes");
    let _: () = redis::cmd("RPUSH").arg(&key).arg(entry).query_async(conn).await?;
    let _: () = redis::cmd("LTRIM").arg(&key).arg(-WINDOW_CAP).arg(-1).query_async(conn).await?;
    let _: () = redis::cmd("EXPIRE").arg(&key).arg(ttl_seconds.unwrap_or(DEFAULT_WINDOW_TTL_SECONDS)).query_async(conn).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn word_numbers_to_digits_handles_the_readme_example() {
        assert_eq!(word_numbers_to_digits("send five hundred dollars"), "send 500 dollars");
        assert_eq!(word_numbers_to_digits("twenty one thousand widgets"), "21000 widgets");
        assert_eq!(word_numbers_to_digits("just some text"), "just some text");
        assert_eq!(word_numbers_to_digits("three hundred"), "300");
    }

    #[test]
    fn catches_numeric_and_formatting_equivalence_not_lexical_synonyms() {
        // What this free method CAN do: the same action expressed with a
        // number-word instead of a digit, plus trivial casing/whitespace
        // differences, still matches.
        let a = normalize_for_similarity(&json!({"action": "transfer", "amount": 500, "recipient": "John"}));
        let b = normalize_for_similarity(&json!({"action": "transfer", "amount_words": "five hundred", "recipient": " john "}));
        assert!(jaccard_similarity(&a, &b) >= 0.65, "a={a:?} b={b:?}");

        // What it CANNOT do, and shouldn't be assumed to: genuine lexical
        // synonyms ("transfer" vs "send") share no tokens at all, so this
        // correctly scores low, not high. Real synonym-level matching needs
        // an actual embedding/NLP model behind the same interface (see the
        // module doc's upgrade path) — this is a documented boundary, not a
        // bug to "fix" by loosening the threshold.
        let c = normalize_for_similarity(&json!({"action": "transfer", "amount": 500, "recipient": "John"}));
        let d = normalize_for_similarity(&json!({"action": "send", "amount_words": "five hundred", "recipient": "John Doe"}));
        assert!(jaccard_similarity(&c, &d) < 0.5, "expected low similarity for synonym-only overlap, got a={c:?} b={d:?}");
    }

    #[test]
    fn identical_payloads_have_similarity_one() {
        let a = normalize_for_similarity(&json!({"x": 1, "y": "Hello"}));
        assert_eq!(jaccard_similarity(&a, &a), 1.0);
    }

    #[test]
    fn empty_payloads_never_falsely_match() {
        assert_eq!(jaccard_similarity("", ""), 0.0);
    }

    #[test]
    fn completely_different_payloads_have_low_similarity() {
        let a = normalize_for_similarity(&json!({"customer": "alice", "amount": 5}));
        let b = normalize_for_similarity(&json!({"widget": "sprocket", "color": "blue"}));
        assert!(jaccard_similarity(&a, &b) < 0.2);
    }

    #[test]
    fn field_names_dont_leak_into_the_token_bag() {
        // Different field names holding the same value should still be
        // comparable — only leaf values matter, not keys.
        let a = normalize_for_similarity(&json!({"amount": 500}));
        let b = normalize_for_similarity(&json!({"total": 500}));
        assert_eq!(a, b);
    }
}
