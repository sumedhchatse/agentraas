//! Pause & Buffer (Enterprise maintenance mode) — mirrors
//! `src/ee/maintenance/index.js`'s `MaintenanceQueue`. A single
//! deployment-wide toggle (not per-org — an operator concern) that, while
//! on, makes incoming webhooks get queued in Redis instead of forwarded
//! immediately. Resuming drains the queue, replaying each buffered
//! request through the exact same pipeline a live request goes through.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{check_dashboard_rate_limit, AuthUser};
use crate::state::{ApiError, SharedState};

use super::require_enterprise_mode;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/admin/maintenance", get(status))
        .route("/api/v1/admin/maintenance/pause", post(pause))
        .route("/api/v1/admin/maintenance/resume", post(resume))
}

const PAUSED_KEY: &str = "maintenance:paused";
const QUEUE_KEY: &str = "maintenance:queue";

pub async fn is_paused(state: &SharedState) -> redis::RedisResult<bool> {
    let mut conn = state.redis_conn_result()?;
    let val: Option<String> = redis::cmd("GET").arg(PAUSED_KEY).query_async(&mut conn).await?;
    Ok(val.as_deref() == Some("true"))
}

async fn pause_queue(state: &SharedState) -> redis::RedisResult<()> {
    let mut conn = state.redis_conn_result()?;
    redis::cmd("SET").arg(PAUSED_KEY).arg("true").query_async(&mut conn).await
}

async fn resume_queue(state: &SharedState) -> redis::RedisResult<()> {
    let mut conn = state.redis_conn_result()?;
    redis::cmd("DEL").arg(PAUSED_KEY).query_async(&mut conn).await
}

async fn queue_length(state: &SharedState) -> redis::RedisResult<i64> {
    let mut conn = state.redis_conn_result()?;
    redis::cmd("LLEN").arg(QUEUE_KEY).query_async(&mut conn).await
}

#[derive(Serialize, Deserialize)]
struct BufferedBody {
    service: String,
    action: String,
    payload: Value,
}

#[derive(Serialize, Deserialize)]
struct BufferedItem {
    #[serde(rename = "orgId")]
    org_id: String,
    #[serde(rename = "agentId")]
    agent_id: String,
    #[serde(rename = "apiKey")]
    api_key: String,
    body: BufferedBody,
}

pub async fn enqueue(state: &SharedState, org_id: &str, agent_id: &str, api_key: &str, service: &str, action: &str, payload: &Value) {
    let item = BufferedItem {
        org_id: org_id.to_string(),
        agent_id: agent_id.to_string(),
        api_key: api_key.to_string(),
        body: BufferedBody { service: service.to_string(), action: action.to_string(), payload: payload.clone() },
    };
    let Ok(mut conn) = state.redis_conn_result() else { return };
    let Ok(serialized) = serde_json::to_string(&item) else { return };
    let _: Result<(), _> = redis::cmd("RPUSH").arg(QUEUE_KEY).arg(serialized).query_async(&mut conn).await;
}

/// Drains up to whatever was queued at call time, replaying each item
/// through the real `handle_request` pipeline in FIFO order. A failed item
/// is logged and skipped, not requeued or retried — this exists to survive
/// a known, bounded maintenance window, not to be a fully durable queue.
async fn drain(state: &SharedState) -> (i64, i64) {
    let total = queue_length(state).await.unwrap_or(0);
    let mut processed = 0i64;
    let mut failed = 0i64;
    for _ in 0..total {
        let raw: Option<String> = {
            let Ok(mut conn) = state.redis_conn_result() else { break };
            redis::cmd("LPOP").arg(QUEUE_KEY).query_async(&mut conn).await.unwrap_or(None)
        };
        let Some(raw) = raw else { break };
        let Ok(item) = serde_json::from_str::<BufferedItem>(&raw) else {
            failed += 1;
            continue;
        };
        let status = crate::agent::replay_webhook(
            state,
            item.org_id,
            item.agent_id,
            item.api_key,
            item.body.service,
            item.body.action,
            item.body.payload,
        )
        .await
        .status_code();
        // 409 (an identical request already in flight) is a benign race,
        // not a real failure.
        if status.as_u16() >= 400 && status != StatusCode::CONFLICT {
            failed += 1;
        } else {
            processed += 1;
        }
    }
    (processed, failed)
}

async fn status(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;
    require_enterprise_mode(&state)?;
    let paused = is_paused(&state).await?;
    let queued = queue_length(&state).await?;
    Ok(Json(json!({ "paused": paused, "queued": queued })))
}

async fn pause(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;
    require_enterprise_mode(&state)?;
    pause_queue(&state).await?;
    Ok(Json(json!({ "paused": true })))
}

async fn resume(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;
    require_enterprise_mode(&state)?;
    resume_queue(&state).await?;
    let (processed, failed) = drain(&state).await;
    Ok(Json(json!({ "paused": false, "flushed": processed, "failed": failed })))
}

async fn require_admin(state: &SharedState, user_id: i32) -> Result<(), ApiError> {
    let is_admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1").bind(user_id).fetch_optional(&state.pg).await?;
    if is_admin != Some(true) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Admin access required."));
    }
    Ok(())
}

