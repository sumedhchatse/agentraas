//! Tool Result & Context Pruner dashboard settings — GET/PUT pair for the
//! per-org opt-in toggle backing `agentraas_core::pruner`. Community +
//! Enterprise both get this (no `require_enterprise_mode` gate), unlike
//! `ee::output_sanitization`'s otherwise-identical shape.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids, is_pruning_enabled};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new().route("/api/v1/output-pruning", get(get_setting).put(put_setting))
}

#[derive(Deserialize)]
struct OrgQuery {
    org_id: String,
}

#[derive(Deserialize)]
struct PutBody {
    org_id: String,
    enabled: bool,
}

async fn get_setting(State(state): State<SharedState>, user: AuthUser, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&q.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !org_ids.contains(&q.org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    let enabled = is_pruning_enabled(&state.pg, &q.org_id).await?;
    Ok(Json(json!({ "org_id": q.org_id, "enabled": enabled })))
}

async fn put_setting(State(state): State<SharedState>, user: AuthUser, Json(body): Json<PutBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    sqlx::query(
        "INSERT INTO org_output_pruning (org_id, enabled) VALUES ($1, $2)
         ON CONFLICT (org_id) DO UPDATE SET enabled = EXCLUDED.enabled, updated_at = NOW()",
    )
    .bind(&body.org_id)
    .bind(body.enabled)
    .execute(&state.pg)
    .await?;
    Ok(Json(json!({ "saved": true, "org_id": body.org_id, "enabled": body.enabled })))
}
