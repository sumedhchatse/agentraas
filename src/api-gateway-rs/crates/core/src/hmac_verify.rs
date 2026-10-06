//! Real per-provider inbound-webhook signature verification — mirrors
//! `src/ee/hmac/index.js`. Each provider has its own header name, hashing
//! algorithm, encoding, and what exactly gets signed; these are NOT
//! interchangeable. All comparisons are constant-time (`subtle`-style,
//! via a manual timing-safe compare) — never a plain `==`, which leaks
//! timing information an attacker can use to guess a valid signature one
//! byte at a time.

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha256;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct VerifyResult {
    pub valid: bool,
    pub reason: Option<String>,
}

impl VerifyResult {
    fn ok() -> Self {
        Self { valid: true, reason: None }
    }
    fn fail(reason: impl Into<String>) -> Self {
        Self { valid: false, reason: Some(reason.into()) }
    }
}

pub use crate::timing_safe_equal_strings;

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn hmac_sha256_hex(secret: &str, message: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn hmac_sha256_base64(secret: &str, message: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

fn hmac_sha1_base64(secret: &str, message: &str) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(message.as_bytes());
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

/// Header: `Stripe-Signature: t=<unix-seconds>,v1=<hex>[,v0=<hex>]`.
/// Signed string: `"{timestamp}.{rawBody}"`, HMAC-SHA256, hex. Includes
/// replay protection (default 5-minute tolerance).
pub fn verify_stripe(raw_body: &str, signature_header: Option<&str>, secret: &str, tolerance_seconds: i64) -> VerifyResult {
    let Some(header) = signature_header else {
        return VerifyResult::fail("Missing Stripe-Signature header.");
    };
    let parts: HashMap<&str, &str> = header
        .split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let (Some(timestamp), Some(v1)) = (parts.get("t"), parts.get("v1")) else {
        return VerifyResult::fail("Malformed Stripe-Signature header.");
    };
    let Ok(ts) = timestamp.parse::<i64>() else {
        return VerifyResult::fail("Malformed Stripe-Signature header.");
    };
    if (now_seconds() - ts).abs() > tolerance_seconds {
        return VerifyResult::fail("Signature timestamp outside tolerance window (possible replay).");
    }
    let signed_payload = format!("{timestamp}.{raw_body}");
    let expected = hmac_sha256_hex(secret, &signed_payload);
    if timing_safe_equal_strings(v1, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// `X-Hub-Signature-256: sha256=<hex>` — raw body, HMAC-SHA256, hex. Used
/// by both GitHub and WhatsApp/Meta (same underlying webhook infra).
pub fn verify_github_style(raw_body: &str, signature_header: Option<&str>, secret: &str, header_name: &str) -> VerifyResult {
    let Some(header) = signature_header else {
        return VerifyResult::fail(format!("Missing {header_name} header."));
    };
    let provided = header.strip_prefix("sha256=").unwrap_or(header);
    let expected = hmac_sha256_hex(secret, raw_body);
    if timing_safe_equal_strings(provided, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// `X-Shopify-Hmac-SHA256: <base64>` — raw body, HMAC-SHA256, base64 (not
/// hex — a common, silent mix-up with GitHub's scheme).
pub fn verify_shopify(raw_body: &str, signature_header: Option<&str>, secret: &str) -> VerifyResult {
    let Some(header) = signature_header else {
        return VerifyResult::fail("Missing X-Shopify-Hmac-SHA256 header.");
    };
    let expected = hmac_sha256_base64(secret, raw_body);
    if timing_safe_equal_strings(header, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// `X-Twilio-Signature: <base64>` — genuinely different scheme: signs the
/// full request URL with each POST parameter, sorted alphabetically by
/// key, appended as key+value with no separator, HMAC-SHA1, base64.
pub fn verify_twilio(request_url: &str, params: &HashMap<String, String>, signature_header: Option<&str>, auth_token: &str) -> VerifyResult {
    let Some(header) = signature_header else {
        return VerifyResult::fail("Missing X-Twilio-Signature header.");
    };
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    let mut signed_string = request_url.to_string();
    for k in keys {
        signed_string.push_str(k);
        signed_string.push_str(&params[k]);
    }
    let expected = hmac_sha1_base64(auth_token, &signed_string);
    if timing_safe_equal_strings(header, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// `X-Slack-Signature: v0=<hex>` + `X-Slack-Request-Timestamp`. Signed
/// string: `"v0:{timestamp}:{rawBody}"`, HMAC-SHA256, hex.
pub fn verify_slack(raw_body: &str, signature_header: Option<&str>, timestamp_header: Option<&str>, secret: &str, tolerance_seconds: i64) -> VerifyResult {
    let (Some(sig), Some(ts_str)) = (signature_header, timestamp_header) else {
        return VerifyResult::fail("Missing X-Slack-Signature or X-Slack-Request-Timestamp header.");
    };
    let Ok(ts) = ts_str.parse::<i64>() else {
        return VerifyResult::fail("Malformed X-Slack-Request-Timestamp header.");
    };
    if (now_seconds() - ts).abs() > tolerance_seconds {
        return VerifyResult::fail("Signature timestamp outside tolerance window (possible replay).");
    }
    let signed_payload = format!("v0:{ts_str}:{raw_body}");
    let expected = format!("v0={}", hmac_sha256_hex(secret, &signed_payload));
    if timing_safe_equal_strings(sig, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// `Linear-Signature: <hex>` — raw body, HMAC-SHA256, hex.
pub fn verify_linear(raw_body: &str, signature_header: Option<&str>, secret: &str) -> VerifyResult {
    let Some(header) = signature_header else {
        return VerifyResult::fail("Missing Linear-Signature header.");
    };
    let expected = hmac_sha256_hex(secret, raw_body);
    if timing_safe_equal_strings(header, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// Mailgun signs inside the (form-encoded) body itself, not a header:
/// `timestamp`/`token`/`signature` fields. Signed: `"{timestamp}{token}"`,
/// HMAC-SHA256, hex.
pub fn verify_mailgun(params: &HashMap<String, String>, secret: &str) -> VerifyResult {
    let (Some(timestamp), Some(token), Some(signature)) = (params.get("timestamp"), params.get("token"), params.get("signature")) else {
        return VerifyResult::fail("Missing timestamp/token/signature in the webhook body.");
    };
    let expected = hmac_sha256_hex(secret, &format!("{timestamp}{token}"));
    if timing_safe_equal_strings(signature, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

/// SendGrid Event Webhook — the one genuinely asymmetric scheme: ECDSA
/// (P-256) verified with the account's *public* key (not a shared
/// secret). Signed payload: timestamp bytes concatenated directly with
/// the raw body.
pub fn verify_sendgrid(raw_body: &str, signature_header: Option<&str>, timestamp_header: Option<&str>, public_key_pem: &str) -> VerifyResult {
    let (Some(sig_b64), Some(ts)) = (signature_header, timestamp_header) else {
        return VerifyResult::fail("Missing signature or timestamp header.");
    };
    use base64::Engine;
    let Ok(sig_bytes) = base64::engine::general_purpose::STANDARD.decode(sig_b64) else {
        return VerifyResult::fail("Malformed signature or public key.");
    };
    let mut signed_payload = ts.as_bytes().to_vec();
    signed_payload.extend_from_slice(raw_body.as_bytes());

    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature, VerifyingKey};
    use p256::pkcs8::DecodePublicKey;

    let Ok(verifying_key) = VerifyingKey::from_public_key_pem(public_key_pem) else {
        return VerifyResult::fail("Malformed signature or public key.");
    };
    let Ok(signature) = Signature::from_der(&sig_bytes) else {
        return VerifyResult::fail("Malformed signature or public key.");
    };
    match verifying_key.verify(&signed_payload, &signature) {
        Ok(()) => VerifyResult::ok(),
        Err(_) => VerifyResult::fail("Signature mismatch."),
    }
}

/// HubSpot Webhook v3: `X-HubSpot-Signature-v3` (base64) +
/// `X-HubSpot-Request-Timestamp` (unix *milliseconds*). Signed string:
/// `"{method}{uri}{rawBody}{timestamp}"`, HMAC-SHA256, base64.
#[allow(clippy::too_many_arguments)]
pub fn verify_hubspot(raw_body: &str, signature_header: Option<&str>, timestamp_header: Option<&str>, method: &str, uri: &str, secret: &str, tolerance_seconds: i64) -> VerifyResult {
    let (Some(sig), Some(ts_str)) = (signature_header, timestamp_header) else {
        return VerifyResult::fail("Missing X-HubSpot-Signature-v3 or X-HubSpot-Request-Timestamp header.");
    };
    let Ok(ts_ms) = ts_str.parse::<i64>() else {
        return VerifyResult::fail("Malformed X-HubSpot-Request-Timestamp header.");
    };
    let now_ms = now_seconds() * 1000;
    if (now_ms - ts_ms).abs() > tolerance_seconds * 1000 {
        return VerifyResult::fail("Signature timestamp outside tolerance window (possible replay).");
    }
    let signed_string = format!("{method}{uri}{raw_body}{ts_str}");
    let expected = hmac_sha256_base64(secret, &signed_string);
    if timing_safe_equal_strings(sig, &expected) { VerifyResult::ok() } else { VerifyResult::fail("Signature mismatch.") }
}

pub const HEADER_NAMES: &[(&str, &str)] = &[
    ("stripe", "stripe-signature"),
    ("github", "x-hub-signature-256"),
    ("whatsapp", "x-hub-signature-256"),
    ("shopify", "x-shopify-hmac-sha256"),
    ("twilio", "x-twilio-signature"),
    ("slack", "x-slack-signature"),
    ("linear", "linear-signature"),
    ("sendgrid", "x-twilio-email-event-webhook-signature"),
    ("hubspot", "x-hubspot-signature-v3"),
];

pub fn header_name_for(provider: &str) -> Option<&'static str> {
    HEADER_NAMES.iter().find(|(p, _)| *p == provider).map(|(_, h)| *h)
}
