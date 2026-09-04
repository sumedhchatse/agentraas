//! Notification Webhooks (instant outage notifications) — mirrors
//! `/api/v1/notification-webhooks` plus `sendOutageNotification` /
//! `notifyCircuitOpen` in `server.js`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};
use crate::util::validate_target_url;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/notification-webhooks", post(create_webhook).get(list_webhooks))
        .route("/api/v1/notification-webhooks/:id", delete(delete_webhook))
        .route("/api/v1/notification-webhooks/:id/test", post(test_webhook))
}

/// Fires when the circuit breaker blocks a request for a service this org
/// actually uses. Rate-limited to once per org+service per 60s window via
/// Redis SETNX, so a burst of blocked requests during one outage sends one
/// notification, not one per request.
pub async fn notify_circuit_open(state: &SharedState, org_id: &str, service: &str) {
    let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await else { return };
    let key = format!("circuit-notified:{org_id}:{service}");
    let claimed: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("EX")
        .arg(60)
        .arg("NX")
        .query_async(&mut conn)
        .await
        .unwrap_or(None);
    if claimed.as_deref() != Some("OK") {
        return;
    }
    send_outage_notification(
        state,
        org_id,
        &format!("⚠️ AgentRaaS Circuit Breaker Activated: {service} is unresponsive. Requests to it are being shielded for your org until it recovers."),
    )
    .await;
}

pub async fn send_outage_notification(state: &SharedState, org_id: &str, message: &str) {
    #[derive(sqlx::FromRow)]
    struct Row {
        r#type: String,
        encrypted_target: String,
        extra: Option<String>,
    }
    let rows: Vec<Row> = match sqlx::query_as(
        "SELECT type, encrypted_target, extra FROM notification_webhooks WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_all(&state.pg)
    .await
    {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(?err, org_id, "failed to load notification webhooks");
            return;
        }
    };

    for row in rows {
        let target = match state.cipher.decrypt(&row.encrypted_target) {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(?err, org_id, r#type = %row.r#type, "failed to decrypt notification target");
                continue;
            }
        };
        let result = match row.r#type.as_str() {
            "slack" => state.http_client.post(&target).json(&json!({ "text": message })).timeout(std::time::Duration::from_secs(5)).send().await,
            "discord" => state.http_client.post(&target).json(&json!({ "content": message })).timeout(std::time::Duration::from_secs(5)).send().await,
            "telegram" => {
                let url = format!("https://api.telegram.org/bot{target}/sendMessage");
                state.http_client.post(&url).json(&json!({ "chat_id": row.extra, "text": message })).timeout(std::time::Duration::from_secs(5)).send().await
            }
            _ => continue,
        };
        if let Err(err) = result {
            tracing::warn!(org_id, r#type = %row.r#type, error = %err, "outage notification failed");
        }
    }
}

#[derive(Deserialize)]
struct CreateWebhookBody {
    org_id: Option<String>,
    r#type: Option<String>,
    target: Option<String>,
    chat_id: Option<String>,
}

async fn create_webhook(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<CreateWebhookBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let org_id = body.org_id.unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    let webhook_type = body.r#type.unwrap_or_default();
    if !["slack", "discord", "telegram"].contains(&webhook_type.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "type must be one of: slack, discord, telegram."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let target = body.target.unwrap_or_default();
    if target.is_empty() || target.chars().count() > 500 {
        let msg = if webhook_type == "telegram" { "target (bot token) is required." } else { "target (webhook URL) is required." };
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, msg));
    }
    if webhook_type == "slack" || webhook_type == "discord" {
        if let Some(err) = validate_target_url(&target).await {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
        }
    }
    if webhook_type == "telegram" && body.chat_id.as_deref().unwrap_or("").is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "chat_id is required for type \"telegram\"."));
    }

    let encrypted_target = state.cipher.encrypt(&target);
    let extra = if webhook_type == "telegram" { body.chat_id.clone() } else { None };

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        r#type: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let row = sqlx::query_as::<_, Row>(
        "INSERT INTO notification_webhooks (org_id, type, encrypted_target, extra, created_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (org_id, type) DO UPDATE SET encrypted_target = EXCLUDED.encrypted_target, extra = EXCLUDED.extra
         RETURNING id, org_id, type, created_at",
    )
    .bind(&org_id)
    .bind(&webhook_type)
    .bind(&encrypted_target)
    .bind(&extra)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;

    Ok(Json(json!({
        "saved": true,
        "webhook": { "id": row.id, "org_id": row.org_id, "type": row.r#type, "created_at": row.created_at },
    })))
}

async fn list_webhooks(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        r#type: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, type, created_at FROM notification_webhooks WHERE org_id = ANY($1) ORDER BY created_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "id": r.id, "org_id": r.org_id, "type": r.r#type, "created_at": r.created_at }))
            .collect(),
    ))
}

async fn delete_webhook(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Notification webhook not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM notification_webhooks WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Notification webhook not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}

async fn test_webhook(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    let org_id: Option<String> = sqlx::query_scalar("SELECT org_id FROM notification_webhooks WHERE id = $1 AND org_id = ANY($2)")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    let Some(org_id) = org_id else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Notification webhook not found."));
    };
    send_outage_notification(&state, &org_id, "🔔 AgentRaaS test notification — if you can see this, outage alerts are wired up correctly.").await;
    Ok(Json(json!({ "sent": true })))
}
