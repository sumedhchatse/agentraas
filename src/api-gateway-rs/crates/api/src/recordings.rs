//! Record and replay: run an agent once against the real providers with
//! `X-AgentRaaS-Record: <name>`, then run it again (in a test, in CI, after a
//! prompt change) with `X-AgentRaaS-Replay: <name>` and every call is answered
//! from the recording. Nothing is sent to the provider, no dedup slot is
//! claimed and nothing counts as usage.
//!
//! Playback is by order: the nth `service.action` call of a replay gets the
//! nth recorded `service.action` answer. The position is kept in Redis per
//! org, agent, recording and run id for an hour, so a replay with a new
//! `X-AgentRaaS-Run-Id` starts from the top. Each answer says whether the
//! payload matches what was recorded, which is how a replay shows the agent
//! now does something different.
//!
//! Only successful, non-streaming calls are recorded. Payload and response
//! are encrypted at rest and kept 30 days.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids};
use crate::agent::Response;
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

const KEEP_DAYS: i32 = 30;
const CURSOR_TTL_SECONDS: i64 = 3600;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/recordings", get(list))
        .route("/api/v1/recordings/:name", get(entries).delete(remove))
}

/// Called after a recorded call succeeded. Best-effort: a failure here never
/// changes the caller's response.
#[allow(clippy::too_many_arguments)]
pub async fn record(state: &SharedState, org_id: &str, agent_id: &str, name: &str, req_id: &str, service: &str, action: &str, payload: &Value, result: &Value) {
    // Expired rows go here rather than in a job: only orgs that record pay for it.
    let _ = sqlx::query("DELETE FROM recordings WHERE expires_at < NOW()").execute(&state.pg).await;
    if let Err(err) = sqlx::query(
        "INSERT INTO recordings (org_id, name, agent_id, req_id, service, action, encrypted_payload, encrypted_response, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW() + make_interval(days => $9))",
    )
    .bind(org_id)
    .bind(name)
    .bind(agent_id)
    .bind(req_id)
    .bind(service)
    .bind(action)
    .bind(state.cipher.encrypt(&payload.to_string()))
    .bind(state.cipher.encrypt(&result.to_string()))
    .bind(KEEP_DAYS)
    .execute(&state.pg)
    .await
    {
        tracing::warn!(?err, org_id, req_id, "recording: could not record");
    }
}

/// Answer a call from recording `name` instead of forwarding it.
#[allow(clippy::too_many_arguments)]
pub async fn replay(state: &SharedState, org_id: &str, agent_id: &str, name: &str, run_id: Option<&str>, service: &str, action: &str, payload: &Value, req_id: &str) -> Response {
    let fail = |status: StatusCode, msg: String| -> Response { (status, Json(json!({ "error": msg, "reqId": req_id }))).into() };
    if !is_valid_identifier(name) {
        return fail(StatusCode::BAD_REQUEST, "Recording names are 1-100 characters, letters/numbers/underscore/hyphen only.".into());
    }
    let Ok(mut conn) = state.redis_conn_result() else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred.".into());
    };
    let cursor = format!("replay:{org_id}:{agent_id}:{name}:{}:{service}.{action}", run_id.unwrap_or("-"));
    let position: i64 = match redis::cmd("INCR").arg(&cursor).query_async(&mut conn).await {
        Ok(n) => n,
        Err(err) => {
            tracing::error!(?err, "replay cursor failed");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred.".into());
        }
    };
    let _: Result<(), _> = redis::cmd("EXPIRE").arg(&cursor).arg(CURSOR_TTL_SECONDS).query_async(&mut conn).await;

    let row: Result<Option<(String, String)>, _> = sqlx::query_as(
        "SELECT encrypted_payload, encrypted_response FROM recordings
         WHERE org_id = $1 AND name = $2 AND service = $3 AND action = $4 AND expires_at > NOW()
         ORDER BY id OFFSET $5 LIMIT 1",
    )
    .bind(org_id)
    .bind(name)
    .bind(service)
    .bind(action)
    .bind(position - 1)
    .fetch_optional(&state.pg)
    .await;
    let (recorded_payload, response) = match row {
        Ok(Some(r)) => r,
        Ok(None) => {
            return fail(
                StatusCode::NOT_FOUND,
                format!("Recording \"{name}\" has no call #{position} to {service}.{action}. The agent made a call the recording doesn't have."),
            )
        }
        Err(err) => {
            tracing::error!(?err, "replay lookup failed");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred.".into());
        }
    };
    let decrypt = |s: &str| state.cipher.decrypt(s).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let (Some(recorded_payload), Some(mut result)) = (decrypt(&recorded_payload), decrypt(&response)) else {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "Recorded call could not be decrypted.".into());
    };
    if let Value::Object(ref mut map) = result {
        map.insert("reqId".into(), Value::String(req_id.into()));
        map.insert(
            "replay".into(),
            json!({ "recording": name, "position": position, "payload_matches": &recorded_payload == payload }),
        );
    }
    (StatusCode::OK, Json(result)).into()
}

#[derive(Deserialize)]
struct OrgQuery {
    org_id: String,
}

async fn check_member(state: &SharedState, user: &AuthUser, org_id: &str) -> Result<(), ApiError> {
    check_dashboard_rate_limit(state, user.sub).await?;
    if !is_valid_identifier(org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !get_user_org_ids(&state.pg, user.sub).await?.iter().any(|o| o == org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    Ok(())
}

async fn list(State(state): State<SharedState>, user: AuthUser, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_member(&state, &user, &q.org_id).await?;
    let rows: Vec<(String, i64, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT name, COUNT(*), MIN(created_at), MAX(created_at) FROM recordings
         WHERE org_id = $1 AND expires_at > NOW() GROUP BY name ORDER BY MAX(created_at) DESC LIMIT 200",
    )
    .bind(&q.org_id)
    .fetch_all(&state.pg)
    .await?;
    let recordings: Vec<Value> = rows
        .into_iter()
        .map(|(name, calls, first, last)| json!({ "name": name, "calls": calls, "first_at": first, "last_at": last }))
        .collect();
    Ok(Json(json!({ "recordings": recordings })))
}

async fn entries(State(state): State<SharedState>, user: AuthUser, Path(name): Path<String>, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_member(&state, &user, &q.org_id).await?;
    let rows: Vec<(String, String, String, String, String, String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT agent_id, req_id, service, action, encrypted_payload, encrypted_response, created_at FROM recordings
         WHERE org_id = $1 AND name = $2 AND expires_at > NOW() ORDER BY id LIMIT 1000",
    )
    .bind(&q.org_id)
    .bind(&name)
    .fetch_all(&state.pg)
    .await?;
    let decrypt = |s: &str| state.cipher.decrypt(s).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
    let calls: Vec<Value> = rows
        .into_iter()
        .map(|(agent_id, req_id, service, action, payload, response, created_at)| {
            json!({ "agent_id": agent_id, "req_id": req_id, "service": service, "action": action,
                    "payload": decrypt(&payload), "response": decrypt(&response), "created_at": created_at })
        })
        .collect();
    Ok(Json(json!({ "name": name, "calls": calls })))
}

async fn remove(State(state): State<SharedState>, user: AuthUser, Path(name): Path<String>, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_member(&state, &user, &q.org_id).await?;
    if !check_org_write_permission(&state.pg, user.sub, &q.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let deleted = sqlx::query("DELETE FROM recordings WHERE org_id = $1 AND name = $2")
        .bind(&q.org_id)
        .bind(&name)
        .execute(&state.pg)
        .await?
        .rows_affected();
    Ok(Json(json!({ "deleted": deleted })))
}
