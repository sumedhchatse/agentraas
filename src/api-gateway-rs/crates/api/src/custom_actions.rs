//! Custom Actions — register any endpoint, not just a curated service.
//! Mirrors `/api/v1/custom-actions` in `server.js`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_agency_tenant_cap, check_org_write_permission};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::credentials::save_credential;
use crate::state::{ApiError, SharedState};
use crate::util::{is_valid_header_name, validate_target_url};

const MAX_EXTRA_HEADERS: usize = 10;
const MAX_FANOUT_URLS: usize = 5;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/custom-actions", post(create_custom_action).get(list_custom_actions))
        .route("/api/v1/custom-actions/:id", delete(revoke_custom_action))
}

#[derive(Deserialize)]
struct ExtraHeaderIn {
    name: String,
    value: String,
    #[serde(default)]
    secret: bool,
}

#[derive(Deserialize)]
struct CreateCustomActionBody {
    org_id: Option<String>,
    name: Option<String>,
    method: Option<String>,
    target_url: Option<String>,
    auth_type: Option<String>,
    auth_header_name: Option<String>,
    content_type: Option<String>,
    credential: Option<Value>,
    extra_headers: Option<Vec<ExtraHeaderIn>>,
    fanout_urls: Option<Vec<String>>,
}

/// Validates and (for `secret: true` entries) encrypts a submitted
/// `extra_headers` array. Returns the JSONB-ready array or an error string.
fn prepare_extra_headers(state: &SharedState, headers: Option<Vec<ExtraHeaderIn>>) -> Result<Value, String> {
    let Some(headers) = headers else {
        return Ok(json!([]));
    };
    if headers.len() > MAX_EXTRA_HEADERS {
        return Err(format!("extra_headers supports at most {MAX_EXTRA_HEADERS} entries."));
    }
    let mut prepared = Vec::with_capacity(headers.len());
    for h in headers {
        if !is_valid_header_name(&h.name) {
            return Err(format!("\"{}\" is not a valid header name.", h.name));
        }
        if h.value.is_empty() || h.value.chars().count() > 2000 {
            return Err(format!("extra_headers entry \"{}\" needs a non-empty value (max 2000 characters).", h.name));
        }
        if h.secret {
            prepared.push(json!({ "name": h.name, "secret": true, "value": state.cipher.encrypt(&h.value) }));
        } else {
            prepared.push(json!({ "name": h.name, "secret": false, "value": h.value }));
        }
    }
    Ok(Value::Array(prepared))
}

/// Multi-Destination Fan-Out — validates a submitted `fanout_urls` array
/// with the exact same SSRF guard as `target_url`.
async fn prepare_fanout_urls(urls: Option<Vec<String>>) -> Result<Vec<String>, String> {
    let Some(urls) = urls else {
        return Ok(vec![]);
    };
    if urls.len() > MAX_FANOUT_URLS {
        return Err(format!("fanout_urls supports at most {MAX_FANOUT_URLS} destinations."));
    }
    for url in &urls {
        if let Some(err) = validate_target_url(url).await {
            return Err(format!("fanout_urls: {err}"));
        }
    }
    Ok(urls)
}

async fn create_custom_action(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<CreateCustomActionBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let org_id = body.org_id.unwrap_or_default();
    let name = body.name.unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !is_valid_identifier(&name) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "name must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if name == "custom" {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "\"custom\" is reserved as the service keyword — pick a different name."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let tenant_cap = check_agency_tenant_cap(&state, user.sub, &org_id).await?;
    if !tenant_cap.ok {
        return Err(ApiError::new(
            StatusCode::PAYMENT_REQUIRED,
            format!("Agency plan is limited to {} client tenants. Contact support@agentraas.io to increase this.", tenant_cap.limit),
        ));
    }

    const ALLOWED_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE"];
    let upper_method = body.method.unwrap_or_else(|| "POST".to_string()).to_uppercase();
    if !ALLOWED_METHODS.contains(&upper_method.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("method must be one of: {}", ALLOWED_METHODS.join(", "))));
    }

    const ALLOWED_AUTH_TYPES: &[&str] = &["none", "bearer", "basic", "header"];
    let auth_type = body.auth_type.unwrap_or_else(|| "none".to_string());
    if !ALLOWED_AUTH_TYPES.contains(&auth_type.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("auth_type must be one of: {}", ALLOWED_AUTH_TYPES.join(", "))));
    }
    if auth_type == "header" && body.auth_header_name.as_deref().unwrap_or("").is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "auth_header_name is required when auth_type is \"header\"."));
    }
    let credential_valid = body.credential.as_ref().is_some_and(|c| c.as_object().is_some_and(|o| !o.is_empty()));
    if auth_type != "none" && !credential_valid {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "A credential is required when auth_type is not \"none\"."));
    }

    let target_url = body.target_url.unwrap_or_default();
    if let Some(err) = validate_target_url(&target_url).await {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    let extra_headers = prepare_extra_headers(&state, body.extra_headers)
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let fanout_urls = prepare_fanout_urls(body.fanout_urls)
        .await
        .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e))?;

    let content_type = body.content_type.unwrap_or_else(|| "application/json".to_string());

    let mut tx = state.pg.begin().await?;
    sqlx::query("UPDATE custom_actions SET revoked_at = NOW() WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL")
        .bind(&org_id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO custom_actions (user_id, org_id, name, method, target_url, auth_type, auth_header_name, content_type, extra_headers, fanout_urls)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(user.sub)
    .bind(&org_id)
    .bind(&name)
    .bind(&upper_method)
    .bind(&target_url)
    .bind(&auth_type)
    .bind(&body.auth_header_name)
    .bind(&content_type)
    .bind(&extra_headers)
    .bind(json!(fanout_urls))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if auth_type != "none" {
        if let Some(credential) = &body.credential {
            save_credential(&state, user.sub, &org_id, &format!("custom:{name}"), credential).await?;
        }
    }

    Ok(Json(json!({
        "saved": true,
        "org_id": org_id,
        "name": name,
        "note": format!("Agents can now call this via service:\"custom\", action:\"{name}\"."),
    })))
}

async fn list_custom_actions(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        name: String,
        method: String,
        target_url: String,
        auth_type: String,
        created_at: chrono::DateTime<chrono::Utc>,
        extra_header_count: Option<i32>,
        fanout_url_count: Option<i32>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, name, method, target_url, auth_type, created_at,
                jsonb_array_length(extra_headers) as extra_header_count,
                jsonb_array_length(fanout_urls) as fanout_url_count
         FROM custom_actions WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
    )
    .bind(user.sub)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                json!({
                    "id": r.id, "org_id": r.org_id, "name": r.name, "method": r.method,
                    "target_url": r.target_url, "auth_type": r.auth_type, "created_at": r.created_at,
                    "extra_header_count": r.extra_header_count.unwrap_or(0),
                    "fanout_url_count": r.fanout_url_count.unwrap_or(0),
                })
            })
            .collect(),
    ))
}

async fn revoke_custom_action(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let revoked: Option<i32> = sqlx::query_scalar(
        "UPDATE custom_actions SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id",
    )
    .bind(id)
    .bind(user.sub)
    .fetch_optional(&state.pg)
    .await?;
    if revoked.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Custom action not found."));
    }
    Ok(Json(json!({ "revoked": true })))
}
