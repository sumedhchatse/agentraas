//! Undo log: every action that ran and has a known reverse
//! (`agentraas_core::undo::UndoSpec`, set on curated actions in
//! config/services.json or on a custom action) is recorded with the payload
//! that reverses it, and can be undone from the dashboard.
//!
//! Undoing is exactly-once too: the row is claimed (`available` -> `undoing`)
//! before anything is sent, and the reverse call carries a stable
//! `Idempotency-Key` (`agentraas-undo-<id>`). A reverse call with no answer is
//! `unknown`, never retried, like any other outcome-unknown call.
//!
//! Statuses: available, undoing, undone, unknown, expired (past expires_at).
//! A definite failure puts it back to `available` with the error, so it can
//! be tried again.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use agentraas_core::undo::{build_payload, UndoSpec};

use crate::agent::db::{check_org_write_permission, get_user_org_ids, log_audit, resolve_route};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

/// How long an action stays undoable. Long enough for "the agent did
/// something wrong yesterday"; most providers allow far longer (Stripe
/// refunds: 180 days).
const UNDO_WINDOW_DAYS: i32 = 30;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/undo-log", get(list))
        .route("/api/v1/undo-log/:id/undo", post(undo))
}

/// The reverse of `service.action`, if it has one.
async fn spec_for(state: &SharedState, org_id: &str, service: &str, action: &str) -> Option<UndoSpec> {
    if service != "custom" {
        return state.service_routes.get(&format!("{service}.{action}")).and_then(|r| r.undo.clone());
    }
    let row: Option<(Option<String>, Option<Value>)> = sqlx::query_as(
        "SELECT undo_action, undo_with FROM custom_actions WHERE org_id = $1 AND name = $2 AND revoked_at IS NULL",
    )
    .bind(org_id)
    .bind(action)
    .fetch_optional(&state.pg)
    .await
    .ok()
    .flatten();
    let (Some(undo_action), with) = row? else { return None };
    let with = with.and_then(|w| serde_json::from_value(w).ok()).unwrap_or_default();
    Some(UndoSpec { action: undo_action, with })
}

/// Called after an action ran (pipeline, approved HITL call, DLQ replay).
/// Best-effort: an undo log failure never changes the caller's response.
/// `result` is the forward's result; the upstream body is its
/// `upstream_response`.
#[allow(clippy::too_many_arguments)]
pub async fn record(state: &SharedState, org_id: &str, agent_id: &str, req_id: &str, service: &str, action: &str, payload: &Value, result: &Value) {
    let Some(spec) = spec_for(state, org_id, service, action).await else { return };
    let response = result.get("upstream_response").unwrap_or(&Value::Null);
    let Some(undo_payload) = build_payload(&spec, payload, response) else {
        tracing::warn!(org_id, service, action, "undo: a value the reverse action needs is missing from the response; not undoable");
        return;
    };
    if let Err(err) = sqlx::query(
        "INSERT INTO undo_log (org_id, agent_id, req_id, service, action, undo_service, undo_action, undo_payload, expires_at)
         VALUES ($1, $2, $3, $4, $5, $4, $6, $7, NOW() + make_interval(days => $8))",
    )
    .bind(org_id)
    .bind(agent_id)
    .bind(req_id)
    .bind(service)
    .bind(action)
    .bind(&spec.action)
    .bind(&undo_payload)
    .bind(UNDO_WINDOW_DAYS)
    .execute(&state.pg)
    .await
    {
        tracing::warn!(?err, org_id, req_id, "undo: could not record");
    }
}

#[derive(Deserialize)]
struct ListQuery {
    org_id: String,
    limit: Option<i64>,
}

async fn list(State(state): State<SharedState>, user: AuthUser, Query(q): Query<ListQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&q.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !get_user_org_ids(&state.pg, user.sub).await?.iter().any(|o| o == &q.org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let rows: Vec<(i64, String, String, String, String, String, Value, String, Option<String>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT id, agent_id, req_id, service, action, undo_action, undo_payload,
                CASE WHEN status = 'available' AND expires_at <= NOW() THEN 'expired' ELSE status END,
                error, created_at, expires_at, undone_at
         FROM undo_log WHERE org_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(&q.org_id)
    .bind(limit)
    .fetch_all(&state.pg)
    .await?;
    let entries: Vec<Value> = rows
        .into_iter()
        .map(|(id, agent_id, req_id, service, action, undo_action, undo_payload, status, error, created_at, expires_at, undone_at)| {
            json!({
                "id": id, "agent_id": agent_id, "req_id": req_id, "service": service, "action": action,
                "undo_action": undo_action, "undo_payload": undo_payload, "status": status, "error": error,
                "created_at": created_at, "expires_at": expires_at, "undone_at": undone_at,
            })
        })
        .collect();
    Ok(Json(json!({ "entries": entries })))
}

async fn undo(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    let row: Option<(String, String, String, String, Value, String)> =
        sqlx::query_as("SELECT org_id, agent_id, undo_service, undo_action, undo_payload, status FROM undo_log WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pg)
            .await?;
    let Some((org_id, agent_id, service, action, payload, _)) = row.filter(|r| org_ids.contains(&r.0)) else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "No such undo entry."));
    };
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }

    // The claim: only one request ever moves an entry out of `available`.
    let claimed: Option<i64> = sqlx::query_scalar(
        "UPDATE undo_log SET status = 'undoing', undone_by = $2, error = NULL
         WHERE id = $1 AND status = 'available' AND expires_at > NOW() RETURNING id",
    )
    .bind(id)
    .bind(user.sub)
    .fetch_optional(&state.pg)
    .await?;
    if claimed.is_none() {
        let status: String = sqlx::query_scalar(
            "SELECT CASE WHEN status = 'available' AND expires_at <= NOW() THEN 'expired' ELSE status END FROM undo_log WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&state.pg)
        .await?;
        return Err(ApiError::new(StatusCode::CONFLICT, format!("Can't undo: this entry is {status}.")));
    }

    let undo_req_id = format!("undo_{id}");
    let set_status = |status: &'static str, error: Option<String>| {
        let pg = state.pg.clone();
        async move {
            let _ = sqlx::query(
                "UPDATE undo_log SET status = $2, error = $3, undo_req_id = $4,
                        undone_at = CASE WHEN $2 = 'undone' THEN NOW() ELSE undone_at END
                 WHERE id = $1",
            )
            .bind(id)
            .bind(status)
            .bind(error)
            .bind(format!("undo_{id}"))
            .execute(&pg)
            .await;
        }
    };

    let route = match resolve_route(&state, &service, &action, &org_id).await {
        Ok(Some(r)) => r,
        _ => {
            set_status("available", Some(format!("The reverse action {service}.{action} isn't available any more."))).await;
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("The reverse action {service}.{action} isn't available any more.")));
        }
    };
    let circuit_key = if route.credential_key.is_empty() { service.clone() } else { route.credential_key.clone() };
    let key = format!("agentraas-undo-{id}");
    let start = std::time::Instant::now();
    match crate::agent::forward::forward_with_retry(&state, &route, &service, &action, &org_id, &payload, &undo_req_id, &circuit_key, None, Some(&key)).await {
        Ok(result) => {
            set_status("undone", None).await;
            log_audit(&state.pg, &undo_req_id, &format!("undo:user_{}", user.sub), &org_id, &agent_id, &service, &action, "undo", None, start.elapsed().as_millis() as i64, None, false, None, None, None, None).await;
            Ok(Json(json!({ "undone": true, "id": id, "reqId": undo_req_id, "result": result })))
        }
        Err(err) if err.outcome_unknown => {
            set_status("unknown", Some(err.message.clone())).await;
            log_audit(&state.pg, &undo_req_id, &format!("undo:user_{}", user.sub), &org_id, &agent_id, &service, &action, "error", Some("undo_outcome_unknown"), start.elapsed().as_millis() as i64, None, false, None, None, None, None).await;
            Err(ApiError::new(StatusCode::GATEWAY_TIMEOUT, "The provider didn't answer, so the undo may or may not have happened. It won't be retried; check with the provider."))
        }
        Err(err) => {
            set_status("available", Some(err.message.clone())).await;
            log_audit(&state.pg, &undo_req_id, &format!("undo:user_{}", user.sub), &org_id, &agent_id, &service, &action, "error", Some("undo_failed"), start.elapsed().as_millis() as i64, None, false, None, None, None, None).await;
            Err(ApiError::new(StatusCode::BAD_GATEWAY, format!("Undo failed: {}. You can try again.", err.message)))
        }
    }
}
