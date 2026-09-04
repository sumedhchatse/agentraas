//! Dead Letter Queue (one-click payload replay) — mirrors
//! `/api/v1/dead-letter-queue` in `server.js`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, extract_upstream_error_message, get_user_org_ids, log_audit, resolve_route};
use crate::agent::forward::forward_action;
use crate::auth::{check_dashboard_rate_limit, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/dead-letter-queue", get(list_dlq))
        .route("/api/v1/dead-letter-queue/:id/replay", post(replay_dlq))
        .route("/api/v1/dead-letter-queue/:id", axum::routing::delete(dismiss_dlq))
}

async fn list_dlq(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        req_id: String,
        org_id: String,
        agent_id: String,
        service: String,
        action: String,
        encrypted_payload: String,
        error_message: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, req_id, org_id, agent_id, service, action, encrypted_payload, error_message, created_at
         FROM dead_letter_queue
         WHERE org_id = ANY($1) AND replayed_at IS NULL AND dismissed_at IS NULL
         ORDER BY created_at DESC LIMIT 100",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;

    // The payload is exactly as sensitive as any stored credential —
    // decrypted here (server-side, over the authenticated dashboard
    // session) so the "Edit & Replay" UI has something to prefill.
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let payload = state
                    .cipher
                    .decrypt(&r.encrypted_payload)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok());
                json!({
                    "id": r.id, "req_id": r.req_id, "org_id": r.org_id, "agent_id": r.agent_id,
                    "service": r.service, "action": r.action, "error_message": r.error_message,
                    "created_at": r.created_at, "payload": payload,
                })
            })
            .collect(),
    ))
}

#[derive(Deserialize, Default)]
struct ReplayBody {
    payload: Option<Value>,
}

async fn replay_dlq(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(id): Path<i32>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    let override_payload = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<ReplayBody>(&body).ok().and_then(|b| b.payload)
    };
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        agent_id: String,
        service: String,
        action: String,
        encrypted_payload: String,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, org_id, agent_id, service, action, encrypted_payload
         FROM dead_letter_queue WHERE id = $1 AND org_id = ANY($2) AND replayed_at IS NULL AND dismissed_at IS NULL",
    )
    .bind(id)
    .bind(&org_ids)
    .fetch_optional(&state.pg)
    .await?;
    let Some(row) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Dead-letter entry not found (already replayed, dismissed, or not yours)."));
    };
    if !check_org_write_permission(&state.pg, user.sub, &row.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }

    // The dashboard's "edit parameters" flow — replay the original payload
    // as-is, or an edited one if the caller supplies { payload: {...} }.
    let payload = match override_payload {
        Some(p) => p,
        None => match state.cipher.decrypt(&row.encrypted_payload).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            Some(p) => p,
            None => return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Stored payload could not be decrypted.")),
        },
    };

    let resolved_route = resolve_route(&state, &row.service, &row.action, &row.org_id).await?;
    let Some(resolved_route) = resolved_route else {
        return Err(ApiError::new(
            StatusCode::GONE,
            "This action no longer exists (the service or Custom Action was changed or removed since this failure).",
        ));
    };

    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    let replay_req_id = format!("req_{}", hex::encode(buf));

    match forward_action(&state, &resolved_route, &row.service, &row.action, &row.org_id, &payload, &replay_req_id).await {
        Ok(result) => {
            sqlx::query("UPDATE dead_letter_queue SET replayed_at = NOW() WHERE id = $1").bind(row.id).execute(&state.pg).await?;
            log_audit(
                &state.pg, &replay_req_id, &format!("replay:user_{}", user.sub), &row.org_id, &row.agent_id,
                &row.service, &row.action, "success", None, 0, None, state.enterprise_mode, Some(&payload),
            )
            .await;
            Ok(Json(json!({ "replayed": true, "result": result, "reqId": replay_req_id })))
        }
        Err(err) => {
            let upstream_message = err
                .upstream_body
                .as_ref()
                .and_then(extract_upstream_error_message)
                .unwrap_or_else(|| err.message.clone());
            log_audit(
                &state.pg, &replay_req_id, &format!("replay:user_{}", user.sub), &row.org_id, &row.agent_id,
                &row.service, &row.action, "error", Some(&upstream_message), 0, None, false, None,
            )
            .await;
            let status = err.upstream_status.and_then(|s| StatusCode::from_u16(s).ok()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            Err(ApiError::new(status, upstream_message).with_extra("replayed", json!(false)).with_extra("reqId", json!(replay_req_id)))
        }
    }
}

async fn dismiss_dlq(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Dead-letter entry not found."));
    }
    let dismissed: Option<i32> = sqlx::query_scalar(
        "UPDATE dead_letter_queue SET dismissed_at = NOW() WHERE id = $1 AND org_id = ANY($2) AND replayed_at IS NULL AND dismissed_at IS NULL RETURNING id",
    )
    .bind(id)
    .bind(&org_ids)
    .fetch_optional(&state.pg)
    .await?;
    if dismissed.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Dead-letter entry not found."));
    }
    Ok(Json(json!({ "dismissed": true })))
}
