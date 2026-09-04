//! Validation Rules + Dedup Rules dashboard CRUD — mirrors the
//! `/api/v1/validation-rules` and `/api/v1/dedup-rules` routes in
//! `server.js`.

use agentraas_core::validator::{is_valid_dedup_rule_definition, is_valid_rule_definition, validate_fields};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids};
use crate::auth::{check_dashboard_rate_limit, is_valid_action_name, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/validation-rules", post(create_validation_rule).get(list_validation_rules))
        .route("/api/v1/validation-rules/:id", delete(delete_validation_rule))
        .route("/api/v1/validation-rules/test", post(test_validation_rule))
        .route("/api/v1/dedup-rules", post(create_dedup_rule).get(list_dedup_rules))
        .route("/api/v1/dedup-rules/:id", delete(delete_dedup_rule))
}

#[derive(Deserialize)]
struct RuleBody {
    org_id: Option<String>,
    service: Option<String>,
    action: Option<String>,
    fields: Value,
}

fn validate_identity(org_id: &Option<String>, service: &Option<String>, action: &Option<String>) -> Result<(String, String, String), ApiError> {
    let org_id = org_id.clone().unwrap_or_default();
    let service = service.clone().unwrap_or_default();
    let action = action.clone().unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !is_valid_identifier(&service) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "service must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !is_valid_action_name(&action) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "action must be 1-100 characters, letters/numbers/dots/underscore/hyphen only."));
    }
    Ok((org_id, service, action))
}

// ─── Validation Rules ───

async fn create_validation_rule(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<RuleBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let (org_id, service, action) = validate_identity(&body.org_id, &body.service, &body.action)?;
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if let Some(err) = is_valid_rule_definition(&body.fields) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        service: String,
        action: String,
        fields: Value,
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    let row = sqlx::query_as::<_, Row>(
        "INSERT INTO custom_validation_rules (org_id, service, action, fields, created_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (org_id, service, action) DO UPDATE SET fields = EXCLUDED.fields, updated_at = NOW()
         RETURNING id, org_id, service, action, fields, updated_at",
    )
    .bind(&org_id)
    .bind(&service)
    .bind(&action)
    .bind(&body.fields)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;

    Ok(Json(json!({
        "saved": true,
        "rule": { "id": row.id, "org_id": row.org_id, "service": row.service, "action": row.action, "fields": row.fields, "updated_at": row.updated_at }
    })))
}

async fn list_validation_rules(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        service: String,
        action: String,
        fields: Value,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, service, action, fields, created_at, updated_at
         FROM custom_validation_rules WHERE org_id = ANY($1) ORDER BY updated_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "id": r.id, "org_id": r.org_id, "service": r.service, "action": r.action, "fields": r.fields, "created_at": r.created_at, "updated_at": r.updated_at }))
            .collect(),
    ))
}

async fn delete_validation_rule(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Validation rule not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM custom_validation_rules WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Validation rule not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct TestRuleBody {
    fields: Value,
    payload: Option<Value>,
}

async fn test_validation_rule(Json(body): Json<TestRuleBody>) -> Result<Json<Value>, ApiError> {
    if let Some(err) = is_valid_rule_definition(&body.fields) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }
    let payload = body.payload.unwrap_or(json!({}));
    let error = validate_fields(&payload, &body.fields);
    Ok(Json(json!({ "valid": error.is_none(), "error": error })))
}

// ─── Dedup Rules ───

async fn create_dedup_rule(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<RuleBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let (org_id, service, action) = validate_identity(&body.org_id, &body.service, &body.action)?;
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if let Some(err) = is_valid_dedup_rule_definition(&body.fields) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        service: String,
        action: String,
        fields: Value,
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    let row = sqlx::query_as::<_, Row>(
        "INSERT INTO custom_dedup_rules (org_id, service, action, fields, created_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (org_id, service, action) DO UPDATE SET fields = EXCLUDED.fields, updated_at = NOW()
         RETURNING id, org_id, service, action, fields, updated_at",
    )
    .bind(&org_id)
    .bind(&service)
    .bind(&action)
    .bind(&body.fields)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;

    Ok(Json(json!({
        "saved": true,
        "rule": { "id": row.id, "org_id": row.org_id, "service": row.service, "action": row.action, "fields": row.fields, "updated_at": row.updated_at }
    })))
}

async fn list_dedup_rules(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        service: String,
        action: String,
        fields: Value,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, service, action, fields, created_at, updated_at
         FROM custom_dedup_rules WHERE org_id = ANY($1) ORDER BY updated_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "id": r.id, "org_id": r.org_id, "service": r.service, "action": r.action, "fields": r.fields, "created_at": r.created_at, "updated_at": r.updated_at }))
            .collect(),
    ))
}

async fn delete_dedup_rule(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Dedup rule not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM custom_dedup_rules WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Dedup rule not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}
