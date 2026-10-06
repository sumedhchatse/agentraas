//! Agent Identity (Enterprise) — short-lived, scope-restricted tokens for
//! an already Connect Agent'd (org_id, agent_id) pair, checked at request
//! time in `handle_request` (`crate::agent::mod`) alongside the existing
//! long-lived `api_keys` credential. See
//! `infra/migrations/045_agent_identity_tokens.sql` for the schema.
//!
//! Why a separate credential type instead of extending `api_keys`: the
//! two have opposite lifecycles (an org-issued key never expires until
//! revoked; an identity token is meant to be minted per task/session and
//! die on its own) and opposite trust models (a key can call anything the
//! agent's route table allows; a token can only call what its `scopes`
//! list names). Mixing both into one row/table would need a nullable
//! scopes+expiry pair meaning "no restriction" on every existing key.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::agent::db::{check_org_write_permission, get_user_org_ids, require_tier};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

const TOKEN_PREFIX: &str = "art_live_";
const MIN_TTL_SECONDS: i64 = 60;
const MAX_TTL_SECONDS: i64 = 30 * 24 * 60 * 60; // 30 days — "short-lived" is the point.

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/agent-identity-tokens", post(issue_token).get(list_tokens))
        .route("/api/v1/agent-identity-tokens/:id", axum::routing::delete(revoke_token))
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_token() -> (String, String, String) {
    let mut buf = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut buf);
    let raw = format!("{TOKEN_PREFIX}{}", hex::encode(buf));
    let hash = sha256_hex(&raw);
    let prefix: String = raw.chars().take(16).collect();
    (raw, hash, prefix)
}

/// Result of checking an incoming credential against the identity-token
/// path. `NotIdentityToken` means "this isn't one of ours, fall through
/// to the normal `api_keys` check" — the only outcome that lets
/// `handle_request` proceed to its existing auth path unchanged.
pub enum AuthOutcome {
    NotIdentityToken,
    Ok,
    Unauthorized(&'static str),
    Forbidden(&'static str),
}

/// Fails closed on anything ambiguous: unknown/expired/revoked token, a
/// request claiming a different (org_id, agent_id) than the token was
/// minted for, or a (service, action) not present in `scopes`.
pub async fn authenticate(
    pg: &sqlx::PgPool,
    provided_key: &str,
    org_id: &str,
    agent_id: &str,
    service: &str,
    action: &str,
) -> Result<AuthOutcome, sqlx::Error> {
    if !provided_key.starts_with(TOKEN_PREFIX) {
        return Ok(AuthOutcome::NotIdentityToken);
    }
    let prefix: String = provided_key.chars().take(16).collect();
    let hash = sha256_hex(provided_key);

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        agent_id: String,
        scopes: Value,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT t.id, k.org_id, k.agent_id, t.scopes
         FROM agent_identity_tokens t JOIN api_keys k ON k.id = t.api_key_id
         WHERE t.token_prefix = $1 AND t.token_hash = $2
           AND t.revoked_at IS NULL AND t.expires_at > NOW() AND k.revoked_at IS NULL",
    )
    .bind(&prefix)
    .bind(&hash)
    .fetch_optional(pg)
    .await?;

    let Some(row) = row else {
        return Ok(AuthOutcome::Unauthorized("This agent identity token is invalid, expired, or revoked."));
    };
    if row.org_id != org_id || row.agent_id != agent_id {
        return Ok(AuthOutcome::Unauthorized("This agent identity token was not issued for this org_id/agent_id."));
    }
    let allowed = row.scopes.as_array().is_some_and(|scopes| {
        scopes.iter().any(|s| {
            let svc = s.get("service").and_then(Value::as_str).unwrap_or("");
            let act = s.get("action").and_then(Value::as_str).unwrap_or("");
            (svc == "*" || svc == service) && (act == "*" || act == action)
        })
    });
    if !allowed {
        return Ok(AuthOutcome::Forbidden("This agent identity token is not scoped to call this service/action."));
    }

    let _ = sqlx::query("UPDATE agent_identity_tokens SET last_used_at = NOW() WHERE id = $1").bind(row.id).execute(pg).await;
    Ok(AuthOutcome::Ok)
}

#[derive(Deserialize)]
struct Scope {
    service: String,
    action: String,
}

#[derive(Deserialize)]
struct IssueTokenBody {
    org_id: String,
    agent_id: String,
    scopes: Vec<Scope>,
    ttl_seconds: i64,
}

async fn issue_token(State(state): State<SharedState>, user: AuthUser, Json(body): Json<IssueTokenBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) || !is_valid_identifier(&body.agent_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    require_tier(&state, &body.org_id, agentraas_core::tier::Tier::Enterprise).await?;
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if body.scopes.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "scopes must list at least one {service, action} pair."));
    }
    if body.ttl_seconds < MIN_TTL_SECONDS || body.ttl_seconds > MAX_TTL_SECONDS {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("ttl_seconds must be between {MIN_TTL_SECONDS} and {MAX_TTL_SECONDS}."),
        ));
    }

    let api_key_id: Option<i32> = sqlx::query_scalar("SELECT id FROM api_keys WHERE org_id = $1 AND agent_id = $2 AND revoked_at IS NULL")
        .bind(&body.org_id)
        .bind(&body.agent_id)
        .fetch_optional(&state.pg)
        .await?;
    let Some(api_key_id) = api_key_id else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Connect this agent from the dashboard first — no active api_keys row for this org_id/agent_id."));
    };

    let scopes_json = json!(body.scopes.iter().map(|s| json!({ "service": s.service, "action": s.action })).collect::<Vec<_>>());
    let (raw_token, hash, prefix) = generate_token();
    let (id, expires_at): (i32, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "INSERT INTO agent_identity_tokens (api_key_id, token_hash, token_prefix, scopes, expires_at)
         VALUES ($1, $2, $3, $4, NOW() + make_interval(secs => $5)) RETURNING id, expires_at",
    )
    .bind(api_key_id)
    .bind(&hash)
    .bind(&prefix)
    .bind(&scopes_json)
    .bind(body.ttl_seconds as f64)
    .fetch_one(&state.pg)
    .await?;

    // "Agent Passport" — the product framing for this feature: identity
    // and reliability aren't two separate things, every request this
    // token can make already runs through the same dedup/rate-limit
    // pipeline as everything else. Surfacing that here makes it a visible
    // part of what minting returns, not just a fact buried in the code.
    let rate_limit_per_minute = crate::agent::db::get_effective_rate_limit(&state, &body.org_id).await.unwrap_or(0);
    let mut dedup = Vec::with_capacity(body.scopes.len());
    for scope in &body.scopes {
        let mode = if scope.service == "*" || scope.action == "*" {
            json!({ "service": scope.service, "action": scope.action, "mode": "whole-payload (default; wildcard scope)" })
        } else {
            match crate::agent::db::get_effective_dedup_rule(&state.pg, &body.org_id, &scope.service, &scope.action).await? {
                Some(rule) if !rule.fields.is_empty() => json!({
                    "service": scope.service, "action": scope.action, "mode": "per-field", "fields": rule.fields, "ttl_seconds": rule.ttl_seconds,
                }),
                _ => json!({ "service": scope.service, "action": scope.action, "mode": "whole-payload" }),
            }
        };
        dedup.push(mode);
    }

    Ok(Json(json!({
        "id": id,
        "token": raw_token,
        "expires_at": expires_at,
        "passport": {
            "identity": { "org_id": body.org_id, "agent_id": body.agent_id, "scopes": scopes_json },
            "reliability": { "rate_limit_per_minute": rate_limit_per_minute, "dedup": dedup },
        },
    })))
}

async fn list_tokens(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "tokens": [] })));
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
           'id', t.id, 'org_id', k.org_id, 'agent_id', k.agent_id, 'scopes', t.scopes,
           'created_at', t.created_at, 'expires_at', t.expires_at, 'last_used_at', t.last_used_at, 'revoked_at', t.revoked_at
         )
         FROM agent_identity_tokens t JOIN api_keys k ON k.id = t.api_key_id
         WHERE k.org_id = ANY($1) ORDER BY t.created_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "tokens": rows })))
}

async fn revoke_token(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Agent identity token not found."));
    }
    let revoked: Option<i32> = sqlx::query_scalar(
        "UPDATE agent_identity_tokens SET revoked_at = NOW()
         FROM api_keys k WHERE k.id = agent_identity_tokens.api_key_id
           AND agent_identity_tokens.id = $1 AND k.org_id = ANY($2) AND agent_identity_tokens.revoked_at IS NULL
         RETURNING agent_identity_tokens.id",
    )
    .bind(id)
    .bind(&org_ids)
    .fetch_optional(&state.pg)
    .await?;
    if revoked.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Agent identity token not found."));
    }
    Ok(Json(json!({ "revoked": true })))
}
