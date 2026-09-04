//! Self-serve Credentials panel — mirrors `/api/v1/credentials` in
//! `server.js`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_agency_tenant_cap, check_org_write_permission};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/credentials", post(create_credential).get(list_credentials))
        .route("/api/v1/credentials/:id", delete(revoke_credential))
}

/// `credentials.api_key || credentials.username || Object.values(...)[0]`
fn masked_preview(credentials: &Value) -> String {
    let val = credentials
        .get("api_key")
        .and_then(Value::as_str)
        .or_else(|| credentials.get("username").and_then(Value::as_str))
        .or_else(|| credentials.as_object().and_then(|o| o.values().next()).and_then(Value::as_str));
    let Some(val) = val.filter(|v| !v.is_empty()) else {
        return "••••".to_string();
    };
    if val.chars().count() > 8 {
        let chars: Vec<char> = val.chars().collect();
        let first4: String = chars[..4].iter().collect();
        let last4: String = chars[chars.len() - 4..].iter().collect();
        format!("{first4}••••{last4}")
    } else {
        "••••".to_string()
    }
}

/// Shared by both the standalone Credentials panel and Custom Action
/// creation.
pub async fn save_credential(
    state: &SharedState,
    user_id: i32,
    org_id: &str,
    service_key: &str,
    credentials: &Value,
) -> Result<String, sqlx::Error> {
    let encrypted = state.cipher.encrypt(&credentials.to_string());
    let preview = masked_preview(credentials);

    let mut tx = state.pg.begin().await?;
    sqlx::query("UPDATE service_credentials SET revoked_at = NOW() WHERE org_id=$1 AND service=$2 AND revoked_at IS NULL")
        .bind(org_id)
        .bind(service_key)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO service_credentials (user_id, org_id, service, encrypted_payload, masked_preview) VALUES ($1,$2,$3,$4,$5)")
        .bind(user_id)
        .bind(org_id)
        .bind(service_key)
        .bind(&encrypted)
        .bind(&preview)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(preview)
}

#[derive(Deserialize)]
struct CreateCredentialBody {
    org_id: Option<String>,
    service: Option<String>,
    credentials: Option<Value>,
}

async fn create_credential(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<CreateCredentialBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let org_id = body.org_id.unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
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

    let service = body.service.unwrap_or_default();
    let is_custom_key = service.starts_with("custom:");
    if service.is_empty() || (!state.curated_services.contains(&service) && !is_custom_key) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "A valid service name is required."));
    }
    if is_custom_key {
        let custom_name = &service["custom:".len()..];
        let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM custom_actions WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL")
            .bind(&org_id)
            .bind(custom_name)
            .fetch_optional(&state.pg)
            .await?;
        if exists.is_none() {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("No custom action named \"{custom_name}\" registered for this org.")));
        }
    }

    let credentials = body.credentials.unwrap_or(Value::Null);
    let valid_credentials = credentials.as_object().is_some_and(|o| !o.is_empty());
    if !valid_credentials {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "credentials object is required (e.g. { \"api_key\": \"...\" })."));
    }

    let preview = save_credential(&state, user.sub, &org_id, &service, &credentials).await?;

    Ok(Json(json!({ "saved": true, "org_id": org_id, "service": service, "masked_preview": preview })))
}

async fn list_credentials(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        service: String,
        masked_preview: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, service, masked_preview, created_at
         FROM service_credentials WHERE user_id = $1 AND revoked_at IS NULL ORDER BY service",
    )
    .bind(user.sub)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "id": r.id, "org_id": r.org_id, "service": r.service, "masked_preview": r.masked_preview, "created_at": r.created_at }))
            .collect(),
    ))
}

async fn revoke_credential(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let revoked: Option<i32> = sqlx::query_scalar("UPDATE service_credentials SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id")
        .bind(id)
        .bind(user.sub)
        .fetch_optional(&state.pg)
        .await?;
    if revoked.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Credential not found."));
    }
    Ok(Json(json!({ "revoked": true })))
}
