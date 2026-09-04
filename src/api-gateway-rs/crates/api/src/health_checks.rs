//! Active Health Checks (proactive, opt-in, per-org monitoring) — mirrors
//! `/api/v1/health-checks` plus `runHealthChecks`/`HEALTH_CHECK_SPECS` in
//! `server.js`. Separate from the passive circuit breaker, which only
//! reacts to real agent traffic: this pings a service directly, on a
//! timer, using an org's own stored credentials.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_credential, get_user_org_ids, ResolvedRoute};
use crate::agent::forward::forward_action;
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::notifications::send_outage_notification;
use crate::state::{ApiError, SharedState};

const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Only a small, deliberately curated set of services: each entry is a
/// genuinely read-only, side-effect-free, well-established endpoint.
fn health_check_specs() -> HashMap<&'static str, ResolvedRoute> {
    let mut m = HashMap::new();
    m.insert(
        "stripe",
        ResolvedRoute {
            method: "GET".to_string(),
            url: "https://api.stripe.com/v1/balance".to_string(),
            internal: false,
            auth_type: String::new(),
            auth_header: Some("Authorization".to_string()),
            content_type: "application/x-www-form-urlencoded".to_string(),
            extra_headers: None,
            fanout_urls: Vec::new(),
            credential_key: "stripe".to_string(),
        },
    );
    m.insert(
        "slack",
        ResolvedRoute {
            method: "POST".to_string(),
            url: "https://slack.com/api/auth.test".to_string(),
            internal: false,
            auth_type: String::new(),
            auth_header: Some("Authorization".to_string()),
            content_type: "application/json".to_string(),
            extra_headers: None,
            fanout_urls: Vec::new(),
            credential_key: "slack".to_string(),
        },
    );
    m.insert(
        "mockpay",
        ResolvedRoute {
            method: "POST".to_string(),
            url: "http://localhost:3000/internal/mockpay".to_string(),
            internal: true,
            auth_type: "none".to_string(),
            auth_header: None,
            content_type: "application/json".to_string(),
            extra_headers: None,
            fanout_urls: Vec::new(),
            credential_key: "mockpay".to_string(),
        },
    );
    m
}

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/health-checks", get(list_health_checks).post(enable_health_check))
        .route("/api/v1/health-checks/:org_id/:service", axum::routing::delete(disable_health_check))
}

async fn list_health_checks(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let specs = health_check_specs();
    let supported_services: Vec<&str> = specs.keys().copied().collect();
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "supported_services": supported_services, "enabled": [] })));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: String,
        service: String,
        enabled_at: chrono::DateTime<chrono::Utc>,
        last_ok: Option<bool>,
        last_latency_ms: Option<i32>,
        last_error: Option<String>,
        last_checked_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT hs.org_id, hs.service, hs.enabled_at,
                lr.ok AS last_ok, lr.latency_ms AS last_latency_ms, lr.error AS last_error, lr.checked_at AS last_checked_at
         FROM health_check_settings hs
         LEFT JOIN LATERAL (
           SELECT ok, latency_ms, error, checked_at FROM health_check_results
           WHERE org_id = hs.org_id AND service = hs.service ORDER BY checked_at DESC LIMIT 1
         ) lr ON true
         WHERE hs.org_id = ANY($1) ORDER BY hs.enabled_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;

    let enabled: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "org_id": r.org_id, "service": r.service, "enabled_at": r.enabled_at,
                "last_ok": r.last_ok, "last_latency_ms": r.last_latency_ms,
                "last_error": r.last_error, "last_checked_at": r.last_checked_at,
            })
        })
        .collect();
    Ok(Json(json!({ "supported_services": supported_services, "enabled": enabled })))
}

#[derive(Deserialize)]
struct EnableBody {
    org_id: Option<String>,
    service: Option<String>,
}

async fn enable_health_check(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<EnableBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_id = body.org_id.unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    let service = body.service.unwrap_or_default();
    let specs = health_check_specs();
    let Some(spec) = specs.get(service.as_str()) else {
        let supported: Vec<&str> = specs.keys().copied().collect();
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Active health checks aren't available for \"{service}\" yet. Supported: {}.", supported.join(", ")),
        ));
    };
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if !spec.internal && get_credential(&state, &service, &org_id).await.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("No {service} credentials configured for this org yet. Add them from the Credentials panel first."),
        ));
    }

    sqlx::query("INSERT INTO health_check_settings (org_id, service, enabled_by) VALUES ($1, $2, $3) ON CONFLICT (org_id, service) DO NOTHING")
        .bind(&org_id)
        .bind(&service)
        .bind(user.sub)
        .execute(&state.pg)
        .await?;

    Ok(Json(json!({
        "enabled": true, "org_id": org_id, "service": service,
        "interval_minutes": HEALTH_CHECK_INTERVAL.as_secs() / 60,
    })))
}

async fn disable_health_check(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((org_id, service)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Health check not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM health_check_settings WHERE org_id = $1 AND service = $2 AND org_id = ANY($3) RETURNING id")
        .bind(&org_id)
        .bind(&service)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Health check not found."));
    }
    Ok(Json(json!({ "disabled": true })))
}

/// Background loop — spawned once at startup, runs forever on a fixed
/// interval. Errors loading settings or writing a result are logged and
/// skipped, never crash the loop.
pub fn spawn_health_check_loop(state: SharedState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEALTH_CHECK_INTERVAL);
        loop {
            interval.tick().await;
            run_health_checks(&state).await;
        }
    });
}

async fn run_health_checks(state: &SharedState) {
    #[derive(sqlx::FromRow)]
    struct Setting {
        org_id: String,
        service: String,
    }
    let settings: Vec<Setting> = match sqlx::query_as("SELECT org_id, service FROM health_check_settings").fetch_all(&state.pg).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!(?err, "health check run failed to load settings");
            return;
        }
    };
    let specs = health_check_specs();

    let mut tasks = Vec::new();
    for setting in settings {
        let Some(spec) = specs.get(setting.service.as_str()).cloned() else { continue };
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            run_one_health_check(&state, &setting.org_id, &setting.service, &spec).await;
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
}

async fn run_one_health_check(state: &SharedState, org_id: &str, service: &str, spec: &ResolvedRoute) {
    let start = std::time::Instant::now();
    let payload = if service == "mockpay" { json!({ "amount": 1, "fail": false }) } else { json!({}) };
    let req_id = format!("healthcheck_{}", hex::encode(rand::random::<[u8; 6]>()));

    let (ok, error) = match forward_action(state, spec, service, "health_check", org_id, &payload, &req_id).await {
        Ok(_) => (true, None),
        Err(err) => {
            let msg = crate::agent::db::extract_upstream_error_message(&err.upstream_body.clone().unwrap_or(Value::Null))
                .unwrap_or(err.message);
            (false, Some(msg.chars().take(500).collect::<String>()))
        }
    };
    let latency_ms = start.elapsed().as_millis() as i32;

    if let Err(err) = sqlx::query("INSERT INTO health_check_results (org_id, service, ok, latency_ms, error) VALUES ($1, $2, $3, $4, $5)")
        .bind(org_id)
        .bind(service)
        .bind(ok)
        .bind(latency_ms)
        .bind(&error)
        .execute(&state.pg)
        .await
    {
        tracing::warn!(?err, org_id, service, "health check result write failed");
    }

    let notified_key = format!("healthcheck-notified:{org_id}:{service}");
    let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await else { return };
    if !ok {
        let claimed: Option<String> = redis::cmd("SET").arg(&notified_key).arg("1").arg("EX").arg(1800).arg("NX").query_async(&mut conn).await.unwrap_or(None);
        if claimed.as_deref() == Some("OK") {
            send_outage_notification(
                state,
                org_id,
                &format!("🔴 AgentRaaS Active Monitoring: your {service} credentials failed an automated health check ({}). This checks your stored credentials directly — separate from live traffic.", error.unwrap_or_default()),
            )
            .await;
        }
    } else {
        let was_notified: Option<String> = redis::cmd("GET").arg(&notified_key).query_async(&mut conn).await.unwrap_or(None);
        if was_notified.is_some() {
            let _: Result<(), _> = redis::cmd("DEL").arg(&notified_key).query_async(&mut conn).await;
            send_outage_notification(state, org_id, &format!("✅ AgentRaaS Active Monitoring: your {service} health check is passing again.")).await;
        }
    }
}
