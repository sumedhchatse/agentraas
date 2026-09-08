//! Agentgateway plugin (beta): dashboard CRUD for the list of other
//! MCP/tool backends an org wants unified behind one gateway alongside
//! AgentRaaS's own MCP endpoint. Community + Enterprise both get this —
//! it's a deployment-topology feature, not a tier gate, matching
//! `pruning_settings`.
//!
//! Storage/CRUD only: this does not yet provision a running gateway
//! process for the org (that needs a container-orchestration decision
//! kept separate from this scaffolding).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};
use crate::util::validate_target_url;

const MAX_TARGETS_PER_ORG: i64 = 20;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/agentgateway-targets", get(list_targets).post(create_target))
        .route("/api/v1/agentgateway-targets/:id", delete(delete_target))
}

#[derive(Deserialize)]
struct CreateBody {
    org_id: Option<String>,
    name: Option<String>,
    target_url: Option<String>,
}

async fn create_target(State(state): State<SharedState>, user: AuthUser, Json(body): Json<CreateBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_id = body.org_id.unwrap_or_default();
    let name = body.name.unwrap_or_default();
    let target_url = body.target_url.unwrap_or_default();

    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if name.trim().is_empty() || name.len() > 100 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "name must be 1-100 characters."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if let Some(err) = validate_target_url(&target_url).await {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("target_url: {err}")));
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_agentgateway_targets WHERE org_id = $1")
        .bind(&org_id)
        .fetch_one(&state.pg)
        .await?;
    if count >= MAX_TARGETS_PER_ORG {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("At most {MAX_TARGETS_PER_ORG} targets per org.")));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        name: String,
        target_url: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let row = sqlx::query_as::<_, Row>(
        "INSERT INTO org_agentgateway_targets (org_id, name, target_url, created_by)
         VALUES ($1, $2, $3, $4)
         RETURNING id, org_id, name, target_url, created_at",
    )
    .bind(&org_id)
    .bind(&name)
    .bind(&target_url)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;

    Ok(Json(json!({
        "saved": true,
        "target": { "id": row.id, "org_id": row.org_id, "name": row.name, "target_url": row.target_url, "created_at": row.created_at }
    })))
}

async fn list_targets(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        name: String,
        target_url: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, name, target_url, created_at FROM org_agentgateway_targets
         WHERE org_id = ANY($1) ORDER BY created_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "id": r.id, "org_id": r.org_id, "name": r.name, "target_url": r.target_url, "created_at": r.created_at }))
            .collect(),
    ))
}

async fn delete_target(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Target not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM org_agentgateway_targets WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Target not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}
