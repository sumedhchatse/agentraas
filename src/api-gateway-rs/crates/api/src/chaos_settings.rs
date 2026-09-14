//! Chaos mode dashboard settings — GET/PUT pair for the per-org-per-service
//! synthetic failure rate backing `agentraas_core::chaos`. Community +
//! Enterprise both get this (it's a testing tool, not a security/compliance
//! feature) — no tier gate, just org membership/write permission.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new().route("/api/v1/chaos", get(get_setting).put(put_setting))
}

#[derive(Deserialize)]
struct GetQuery {
    org_id: String,
    service: String,
}

#[derive(Deserialize)]
struct PutBody {
    org_id: String,
    service: String,
    /// 0.0 (off) to 1.0 (always fail). Values above 1.0 are clamped.
    fail_rate: f64,
}

async fn get_setting(State(state): State<SharedState>, user: AuthUser, Query(q): Query<GetQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&q.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !org_ids.contains(&q.org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    let mut conn = state.redis_conn_result()?;
    let fail_rate = agentraas_core::chaos::get_fail_rate(&mut conn, &q.org_id, &q.service).await;
    Ok(Json(json!({ "org_id": q.org_id, "service": q.service, "fail_rate": fail_rate })))
}

async fn put_setting(State(state): State<SharedState>, user: AuthUser, Json(body): Json<PutBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if body.service.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "service is required."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let mut conn = state.redis_conn_result()?;
    agentraas_core::chaos::set_fail_rate(&mut conn, &body.org_id, &body.service, body.fail_rate).await?;
    let saved = agentraas_core::chaos::get_fail_rate(&mut conn, &body.org_id, &body.service).await;
    Ok(Json(json!({ "saved": true, "org_id": body.org_id, "service": body.service, "fail_rate": saved })))
}
