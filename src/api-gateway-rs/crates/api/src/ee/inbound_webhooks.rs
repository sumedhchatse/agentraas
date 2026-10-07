//! Inbound webhook receivers — verifies a real per-provider HMAC/ECDSA
//! signature (`agentraas_core::hmac_verify`) before forwarding to the
//! user's own destination URL: the `/api/v1/inbound-webhooks` CRUD and
//! `/v1/inbound/:token` GET (WhatsApp handshake) / POST (receiver).

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use agentraas_core::hmac_verify::{self, timing_safe_equal_strings, VerifyResult};

use crate::agent::db::{check_org_write_permission, require_tier};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};
use crate::util::validate_target_url;

const PROVIDERS: &[&str] = &["stripe", "github", "shopify", "whatsapp", "twilio", "slack", "linear", "mailgun", "sendgrid", "hubspot"];

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/inbound-webhooks", post(create).get(list))
        .route("/api/v1/inbound-webhooks/:id", delete(revoke))
        .route("/v1/inbound/:token", get(handshake).post(receive))
}

#[derive(Deserialize)]
struct CreateBody {
    org_id: Option<String>,
    provider: Option<String>,
    webhook_secret: Option<String>,
    destination_url: Option<String>,
    whatsapp_verify_token: Option<String>,
}

async fn create(State(state): State<SharedState>, user: AuthUser, Json(body): Json<CreateBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let org_id = body.org_id.unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    // Inbound Webhooks moved off the server-wide on/off switch (was
    // `require_enterprise_mode`, independent of the calling org's actual
    // plan) to a graduated per-org tier check — still Enterprise-only.
    // Same shape as
    // HITL's Pro+ gate in ee/hitl.rs.
    require_tier(&state, &org_id, agentraas_core::tier::Tier::Enterprise).await?;
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let provider = body.provider.unwrap_or_default();
    if !PROVIDERS.contains(&provider.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("provider must be one of: {}", PROVIDERS.join(", "))));
    }
    let webhook_secret = body.webhook_secret.unwrap_or_default();
    if webhook_secret.len() < 8 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "webhook_secret is required (the signing secret from the provider's dashboard)."));
    }
    if provider == "whatsapp" && body.whatsapp_verify_token.as_deref().unwrap_or("").len() < 8 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "whatsapp_verify_token is required for the WhatsApp provider (used in Meta's GET handshake)."));
    }
    let destination_url = body.destination_url.unwrap_or_default();
    if let Some(err) = validate_target_url(&destination_url).await {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("destination_url: {err}")));
    }

    let mut token_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let inbound_token = hex::encode(token_bytes);
    let encrypted_secret = state.cipher.encrypt(&webhook_secret);
    let whatsapp_verify_token = if provider == "whatsapp" { body.whatsapp_verify_token } else { None };

    sqlx::query(
        "INSERT INTO inbound_webhooks (user_id, org_id, provider, webhook_secret, destination_url, inbound_token, whatsapp_verify_token)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(user.sub)
    .bind(&org_id)
    .bind(&provider)
    .bind(&encrypted_secret)
    .bind(&destination_url)
    .bind(&inbound_token)
    .bind(&whatsapp_verify_token)
    .execute(&state.pg)
    .await?;

    Ok(Json(json!({
        "inbound_url": format!("{}/v1/inbound/{}", state.public_url, inbound_token),
        "provider": provider,
        "org_id": org_id,
        "destination_url": destination_url,
    })))
}

async fn list(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    // No tier gate — scoped by user_id below, same reasoning as HITL's
    // list_rules: a downgraded org should still see what it already made.
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        provider: String,
        destination_url: String,
        inbound_token: String,
        created_at: chrono::DateTime<chrono::Utc>,
        last_used_at: Option<chrono::DateTime<chrono::Utc>>,
        revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, provider, destination_url, inbound_token, created_at, last_used_at, revoked_at
         FROM inbound_webhooks WHERE user_id = $1 ORDER BY created_at DESC",
    )
    .bind(user.sub)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                json!({
                    "id": r.id, "org_id": r.org_id, "provider": r.provider, "destination_url": r.destination_url,
                    "inbound_token": r.inbound_token, "created_at": r.created_at, "last_used_at": r.last_used_at,
                    "revoked_at": r.revoked_at, "inbound_url": format!("{}/v1/inbound/{}", state.public_url, r.inbound_token),
                })
            })
            .collect(),
    ))
}

async fn revoke(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    // No tier gate — same reasoning as list above.
    let revoked: Option<i32> = sqlx::query_scalar("UPDATE inbound_webhooks SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id")
        .bind(id)
        .bind(user.sub)
        .fetch_optional(&state.pg)
        .await?;
    if revoked.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Inbound webhook not found."));
    }
    Ok(Json(json!({ "revoked": true })))
}

#[derive(Deserialize)]
struct HandshakeQuery {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

/// Meta's WhatsApp webhook setup requires this GET handshake before it
/// will register the callback URL — echoes `hub.challenge` back only if
/// the verify token matches what was configured at registration time.
async fn handshake(State(state): State<SharedState>, Path(token): Path<String>, Query(q): Query<HandshakeQuery>) -> Result<String, ApiError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT provider, whatsapp_verify_token FROM inbound_webhooks WHERE inbound_token = $1 AND revoked_at IS NULL")
        .bind(&token)
        .fetch_optional(&state.pg)
        .await?;
    let Some((provider, whatsapp_verify_token)) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not found."));
    };
    if provider != "whatsapp" {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "This provider does not use a GET handshake."));
    }
    if q.mode.as_deref() == Some("subscribe") {
        if let (Some(provided), Some(configured), Some(challenge)) = (&q.verify_token, &whatsapp_verify_token, &q.challenge) {
            if timing_safe_equal_strings(provided, configured) {
                return Ok(challenge.clone());
            }
        }
    }
    Err(ApiError::new(StatusCode::FORBIDDEN, "Verification failed."))
}

fn parse_form_body(raw: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect()
}

/// The actual inbound receiver — every provider's webhook lands here.
/// Verifies the signature against that provider's real scheme, and only
/// forwards to the user's real destination if it's authentic.
async fn receive(State(state): State<SharedState>, Path(token): Path<String>, headers: HeaderMap, raw_body: Bytes) -> Result<Json<Value>, ApiError> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        provider: String,
        webhook_secret: String,
        destination_url: String,
    }
    let row: Option<Row> = sqlx::query_as("SELECT id, org_id, provider, webhook_secret, destination_url FROM inbound_webhooks WHERE inbound_token = $1 AND revoked_at IS NULL")
        .bind(&token)
        .fetch_optional(&state.pg)
        .await?;
    let Some(row) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not found."));
    };
    let secret = state.cipher.decrypt(&row.webhook_secret).map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred."))?;

    let raw_body_str = String::from_utf8_lossy(&raw_body).into_owned();
    let inbound_url = format!("{}/v1/inbound/{}", state.public_url, token);

    let header_val = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let sig_header_name = hmac_verify::header_name_for(&row.provider);
    let signature_header = sig_header_name.and_then(header_val);

    let result: VerifyResult = match row.provider.as_str() {
        "stripe" => hmac_verify::verify_stripe(&raw_body_str, signature_header, &secret, 300),
        "github" => hmac_verify::verify_github_style(&raw_body_str, signature_header, &secret, "X-Hub-Signature-256"),
        "whatsapp" => hmac_verify::verify_github_style(&raw_body_str, signature_header, &secret, "X-Hub-Signature-256"),
        "shopify" => hmac_verify::verify_shopify(&raw_body_str, signature_header, &secret),
        "twilio" => {
            let params = parse_form_body(&raw_body_str);
            hmac_verify::verify_twilio(&inbound_url, &params, signature_header, &secret)
        }
        "slack" => hmac_verify::verify_slack(&raw_body_str, signature_header, header_val("x-slack-request-timestamp"), &secret, 300),
        "linear" => hmac_verify::verify_linear(&raw_body_str, signature_header, &secret),
        "mailgun" => {
            let params = parse_form_body(&raw_body_str);
            hmac_verify::verify_mailgun(&params, &secret)
        }
        "sendgrid" => hmac_verify::verify_sendgrid(&raw_body_str, signature_header, header_val("x-twilio-email-event-webhook-timestamp"), &secret),
        "hubspot" => hmac_verify::verify_hubspot(&raw_body_str, signature_header, header_val("x-hubspot-request-timestamp"), "POST", &inbound_url, &secret, 300),
        other => VerifyResult { valid: false, reason: Some(format!("Unknown provider: {other}")) },
    };

    if !result.valid {
        tracing::warn!(provider = %row.provider, org_id = %row.org_id, reason = result.reason.as_deref().unwrap_or("signature mismatch"), "inbound webhook signature verification failed");
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid signature."));
    }

    // destination_url was only checked for SSRF once, at registration —
    // re-check here too, since a DNS record can be rebound to an internal
    // address afterward and this destination gets called indefinitely.
    if let Some(err) = validate_target_url(&row.destination_url).await {
        tracing::warn!(provider = %row.provider, org_id = %row.org_id, error = %err, "inbound webhook destination failed a safety re-check");
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "Signature verified, but the destination failed a safety re-check."));
    }

    let content_type = header_val("content-type").unwrap_or("application/json").to_string();
    let forward_result = state
        .http_client
        .post(&row.destination_url)
        .header("Content-Type", content_type)
        .body(raw_body.to_vec())
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    if let Err(err) = forward_result {
        tracing::error!(provider = %row.provider, org_id = %row.org_id, error = %err, "inbound webhook forward to destination failed");
        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "Signature verified, but could not forward to destination."));
    }

    sqlx::query("UPDATE inbound_webhooks SET last_used_at = NOW() WHERE id = $1").bind(row.id).execute(&state.pg).await?;
    Ok(Json(json!({ "received": true })))
}
