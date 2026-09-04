//! Dashboard analytics routes — mirrors `src/core/dashboard/index.js`:
//! stats, timeseries, by-service, recent activity, usage (+ SSE stream),
//! reliability report, admin users/overview, CSV export, public execution
//! ledger.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::db::{current_month_key, get_effective_limit, get_monthly_usage, get_user_org_ids};
use crate::auth::{check_dashboard_rate_limit, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/dashboard/stats", get(dashboard_stats))
        .route("/api/v1/dashboard/timeseries", get(dashboard_timeseries))
        .route("/api/v1/dashboard/by-service", get(dashboard_by_service))
        .route("/api/v1/recent", get(recent))
        .route("/api/v1/agents", get(agents))
        .route("/api/v1/usage", get(usage))
        .route("/api/v1/usage/stream", get(usage_stream))
        .route("/api/v1/admin/users", get(admin_users))
        .route("/api/v1/public/execution-ledger", get(public_execution_ledger))
        .route("/api/v1/admin/overview", get(admin_overview))
        .route("/api/v1/services", get(services))
        .route("/api/v1/reliability-report", get(reliability_report))
        .route("/api/v1/export/csv", get(export_csv))
        .route("/api/v1/admin/audit/verify-integrity", get(admin_audit_verify_integrity))
        .route("/api/v1/admin/audit/siem-export", get(admin_audit_siem_export))
}

fn range_interval(range: &str) -> Option<&'static str> {
    Some(match range {
        "24h" => "24 hours",
        "7d" => "7 days",
        "30d" => "30 days",
        "90d" => "90 days",
        _ => return None,
    })
}

fn range_bucket(range: &str) -> Option<&'static str> {
    Some(match range {
        "24h" => "hour",
        "7d" | "30d" | "90d" => "day",
        _ => return None,
    })
}

fn range_seconds(range: &str) -> Option<i64> {
    Some(match range {
        "24h" => 86400,
        "7d" => 604800,
        "30d" => 2592000,
        "90d" => 7776000,
        _ => return None,
    })
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct StatsRow {
    total: i64,
    success: i64,
    deduplicated: i64,
    blocked: i64,
    errors: i64,
    avg_duration: Option<f64>,
}

async fn stats(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "total": 0, "success": 0, "deduplicated": 0, "blocked": 0, "errors": 0, "avg_duration": null })));
    }
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let row = sqlx::query_as::<_, StatsRow>(
        "SELECT COUNT(*) as total, COUNT(*) FILTER(WHERE status='success') as success,
                COUNT(*) FILTER(WHERE status='deduplicated') as deduplicated,
                COUNT(*) FILTER(WHERE status='blocked') as blocked,
                COUNT(*) FILTER(WHERE status='error') as errors,
                AVG(duration_ms)::float8 as avg_duration
         FROM audit_log WHERE created_at >= $1::date AND org_id = ANY($2)",
    )
    .bind(&today)
    .bind(&org_ids)
    .fetch_one(&state.pg)
    .await?;
    Ok(Json(json!(row)))
}

#[derive(Deserialize)]
struct RangeQuery {
    range: Option<String>,
}

async fn dashboard_stats(State(state): State<SharedState>, user: AuthUser, Query(q): Query<RangeQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let range = q.range.unwrap_or_else(|| "24h".to_string());
    let Some(interval) = range_interval(&range) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid range. Use one of: 24h, 7d, 30d, 90d."));
    };
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "range": range, "total": 0, "success": 0, "deduplicated": 0, "blocked": 0, "errors": 0, "avg_duration": null })));
    }
    let sql = format!(
        "SELECT COUNT(*) as total, COUNT(*) FILTER(WHERE status='success') as success,
                COUNT(*) FILTER(WHERE status='deduplicated') as deduplicated,
                COUNT(*) FILTER(WHERE status='blocked') as blocked,
                COUNT(*) FILTER(WHERE status='error') as errors,
                AVG(duration_ms)::float8 as avg_duration
         FROM audit_log WHERE created_at >= NOW() - INTERVAL '{interval}' AND org_id = ANY($1)"
    );
    let row = sqlx::query_as::<_, StatsRow>(&sql).bind(&org_ids).fetch_one(&state.pg).await?;
    let mut body = json!(row);
    body["range"] = json!(range);
    Ok(Json(body))
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct TimeseriesPoint {
    bucket: chrono::DateTime<chrono::Utc>,
    total: i64,
}

async fn dashboard_timeseries(State(state): State<SharedState>, user: AuthUser, Query(q): Query<RangeQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let range = q.range.unwrap_or_else(|| "24h".to_string());
    let (Some(interval), Some(bucket)) = (range_interval(&range), range_bucket(&range)) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid range. Use one of: 24h, 7d, 30d, 90d."));
    };
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "range": range, "bucket": bucket, "points": [] })));
    }
    let sql = format!(
        "SELECT (date_trunc('{bucket}', created_at) AT TIME ZONE 'UTC') as bucket, COUNT(*) as total
         FROM audit_log WHERE created_at >= NOW() - INTERVAL '{interval}' AND org_id = ANY($1)
         GROUP BY bucket ORDER BY bucket ASC"
    );
    let points = sqlx::query_as::<_, TimeseriesPoint>(&sql).bind(&org_ids).fetch_all(&state.pg).await?;
    Ok(Json(json!({ "range": range, "bucket": bucket, "points": points })))
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct ServiceCount {
    service: String,
    total: i64,
}

async fn dashboard_by_service(State(state): State<SharedState>, user: AuthUser, Query(q): Query<RangeQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let range = q.range.unwrap_or_else(|| "24h".to_string());
    let Some(interval) = range_interval(&range) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid range. Use one of: 24h, 7d, 30d, 90d."));
    };
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "range": range, "services": [] })));
    }
    let sql = format!(
        "SELECT service, COUNT(*) as total FROM audit_log
         WHERE created_at >= NOW() - INTERVAL '{interval}' AND org_id = ANY($1)
         GROUP BY service ORDER BY total DESC LIMIT 8"
    );
    let services = sqlx::query_as::<_, ServiceCount>(&sql).bind(&org_ids).fetch_all(&state.pg).await?;
    Ok(Json(json!({ "range": range, "services": services })))
}

#[derive(Deserialize)]
struct RecentQuery {
    limit: Option<i64>,
}

async fn recent(State(state): State<SharedState>, user: AuthUser, Query(q): Query<RecentQuery>) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let limit = q.limit.filter(|&l| l > 0).unwrap_or(50);
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        req_id: String,
        api_key: String,
        org_id: String,
        agent_id: String,
        service: String,
        action: String,
        status: String,
        error_type: Option<String>,
        duration_ms: i64,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT req_id, api_key, org_id, agent_id, service, action, status, error_type, duration_ms::bigint as duration_ms, (created_at AT TIME ZONE 'UTC') as created_at
         FROM audit_log WHERE org_id = ANY($1) ORDER BY created_at DESC LIMIT $2",
    )
    .bind(&org_ids)
    .bind(limit)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                json!({
                    "req_id": r.req_id, "api_key": r.api_key, "org_id": r.org_id, "agent_id": r.agent_id,
                    "service": r.service, "action": r.action, "status": r.status, "error_type": r.error_type,
                    "duration_ms": r.duration_ms, "created_at": r.created_at,
                })
            })
            .collect(),
    ))
}

async fn agents(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(vec![]));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        org_id: String,
        agent_id: String,
        total_actions: i64,
        last_seen: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT org_id, agent_id, COUNT(*) as total_actions, (MAX(created_at) AT TIME ZONE 'UTC') as last_seen
         FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours' AND org_id = ANY($1)
         GROUP BY org_id, agent_id ORDER BY total_actions DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "org_id": r.org_id, "agent_id": r.agent_id, "total_actions": r.total_actions, "last_seen": r.last_seen }))
            .collect(),
    ))
}

async fn usage(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let is_admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1").bind(user.sub).fetch_optional(&state.pg).await?;
    let is_exempt = is_admin.unwrap_or(false) || (1..=9).contains(&user.sub);

    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    let cloud = state.deployment_mode == "cloud";

    let mut per_org = Vec::new();
    let mut total: i64 = 0;
    let mut limit: Option<i64> = if cloud { Some(0) } else { None };
    for org_id in &org_ids {
        let count = get_monthly_usage(&state, org_id).await.unwrap_or(0);
        total += count;
        if cloud {
            let org_limit = get_effective_limit(&state, org_id).await?;
            per_org.push(json!({ "org_id": org_id, "count": count, "limit": org_limit }));
            limit = limit.map(|l| l + org_limit);
        } else {
            per_org.push(json!({ "org_id": org_id, "count": count, "limit": null }));
        }
    }

    Ok(Json(json!({
        "deployment_mode": state.deployment_mode,
        "limit": limit,
        "unlimited": limit.is_none(),
        "enforced": cloud && !is_exempt,
        "exempt": is_exempt,
        "total": total,
        "per_org": per_org,
    })))
}

/// Server-Sent Events stream of usage updates for the logged-in user's
/// orgs — pushed the instant an action increments usage (see
/// `increment_monthly_usage`'s Redis PUBLISH), rather than waiting for the
/// dashboard's next poll interval. Polling `GET /api/v1/usage` above still
/// works as a fallback if SSE is unavailable.
async fn usage_stream(
    State(state): State<SharedState>,
    user: AuthUser,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids: std::collections::HashSet<String> = get_user_org_ids(&state.pg, user.sub).await?.into_iter().collect();

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(32);

    let redis_client = state.redis.clone();
    tokio::spawn(async move {
        #[allow(deprecated)]
        let conn_result = redis_client.get_async_connection().await;
        let Ok(conn) = conn_result else { return };
        let mut pubsub = conn.into_pubsub();
        if pubsub.subscribe("usage:updates").await.is_err() {
            return;
        }
        let mut stream = pubsub.on_message();
        while let Some(msg) = stream.next().await {
            let Ok(payload) = msg.get_payload::<String>() else { continue };
            let Ok(val) = serde_json::from_str::<Value>(&payload) else { continue };
            let Some(org_id) = val.get("org_id").and_then(Value::as_str) else { continue };
            if !org_ids.contains(org_id) {
                continue;
            }
            if tx.send(Event::default().data(payload)).await.is_err() {
                break;
            }
        }
    });

    let stream = ReceiverStream::new(rx).map(Ok);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)).text("keepalive")))
}

async fn require_admin(state: &SharedState, user_id: i32) -> Result<(), ApiError> {
    let is_admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1").bind(user_id).fetch_optional(&state.pg).await?;
    if is_admin != Some(true) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Admin access required."));
    }
    Ok(())
}

async fn admin_users(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: i32,
        email: String,
        is_admin: bool,
        email_verified: bool,
        created_at: chrono::DateTime<chrono::Utc>,
        last_login_at: Option<chrono::DateTime<chrono::Utc>>,
        plan: String,
    }
    let users: Vec<UserRow> = sqlx::query_as("SELECT id, email, is_admin, email_verified, created_at, last_login_at, plan FROM users ORDER BY id ASC")
        .fetch_all(&state.pg)
        .await?;
    if users.is_empty() {
        return Ok(Json(vec![]));
    }

    let org_rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT user_id, org_id FROM api_keys UNION SELECT user_id, org_id FROM custom_actions UNION SELECT user_id, org_id FROM service_credentials",
    )
    .fetch_all(&state.pg)
    .await?;
    let custom_actions_rows: Vec<(i32, i64)> = sqlx::query_as("SELECT user_id, COUNT(*) as count FROM custom_actions GROUP BY user_id").fetch_all(&state.pg).await?;
    let credentials_rows: Vec<(i32, i64)> = sqlx::query_as("SELECT user_id, COUNT(*) as count FROM service_credentials WHERE revoked_at IS NULL GROUP BY user_id").fetch_all(&state.pg).await?;
    let api_keys_rows: Vec<(i32, i64)> = sqlx::query_as("SELECT user_id, COUNT(*) as count FROM api_keys GROUP BY user_id").fetch_all(&state.pg).await?;

    let mut orgs_by_user: HashMap<i32, std::collections::HashSet<String>> = HashMap::new();
    let mut all_org_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (user_id, org_id) in org_rows {
        orgs_by_user.entry(user_id).or_default().insert(org_id.clone());
        all_org_ids.insert(org_id);
    }
    let custom_actions_by_user: HashMap<i32, i64> = custom_actions_rows.into_iter().collect();
    let credentials_by_user: HashMap<i32, i64> = credentials_rows.into_iter().collect();
    let api_keys_by_user: HashMap<i32, i64> = api_keys_rows.into_iter().collect();

    let org_id_list: Vec<String> = all_org_ids.into_iter().collect();
    let mut usage_by_org: HashMap<String, i64> = HashMap::new();
    if !org_id_list.is_empty() {
        let keys: Vec<String> = org_id_list.iter().map(|org_id| format!("usage:{org_id}:{}", current_month_key())).collect();
        let mut conn = state.redis.get_multiplexed_async_connection().await?;
        let values: Vec<Option<String>> = redis::cmd("MGET").arg(&keys).query_async(&mut conn).await?;
        for (org_id, v) in org_id_list.iter().zip(values) {
            usage_by_org.insert(org_id.clone(), v.and_then(|s| s.parse().ok()).unwrap_or(0));
        }
    }

    Ok(Json(
        users
            .into_iter()
            .map(|u| {
                let orgs: Vec<String> = orgs_by_user.get(&u.id).map(|s| s.iter().cloned().collect()).unwrap_or_default();
                let usage_this_month: i64 = orgs.iter().map(|o| usage_by_org.get(o).copied().unwrap_or(0)).sum();
                json!({
                    "id": u.id, "email": u.email, "is_admin": u.is_admin, "email_verified": u.email_verified,
                    "created_at": u.created_at, "last_login_at": u.last_login_at, "plan": u.plan,
                    "org_count": orgs.len(), "orgs": orgs, "usage_this_month": usage_this_month,
                    "custom_actions_count": custom_actions_by_user.get(&u.id).copied().unwrap_or(0),
                    "credentials_count": credentials_by_user.get(&u.id).copied().unwrap_or(0),
                    "api_keys_count": api_keys_by_user.get(&u.id).copied().unwrap_or(0),
                })
            })
            .collect(),
    ))
}

/// Unauthenticated, deliberately coarse — global (all-org) 24h counts and
/// average latency only, no per-org/user breakdown. Powers the landing
/// page's live "execution ledger" panel.
async fn public_execution_ledger(State(state): State<SharedState>) -> Result<Json<Value>, ApiError> {
    let row: (i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT COUNT(*) as total, COUNT(*) FILTER (WHERE status = 'deduplicated') as deduplicated, AVG(duration_ms)::float8 as avg_duration
         FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours'",
    )
    .fetch_one(&state.pg)
    .await?;
    Ok(Json(json!({
        "actions_verified": row.0,
        "duplicates_caught": row.1,
        "avg_duration_ms": row.2.map(|v| v.round() as i64),
    })))
}

async fn admin_overview(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;

    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) as count FROM users").fetch_one(&state.pg).await?;
    let org_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT org_id) as count FROM (
           SELECT org_id FROM api_keys UNION SELECT org_id FROM custom_actions UNION SELECT org_id FROM service_credentials
         ) t",
    )
    .fetch_one(&state.pg)
    .await?;
    #[derive(sqlx::FromRow, serde::Serialize)]
    struct MonthTotals {
        success: i64,
        deduplicated: i64,
        blocked: i64,
        errors: i64,
    }
    let month_totals = sqlx::query_as::<_, MonthTotals>(
        "SELECT COUNT(*) FILTER (WHERE status='success') as success,
                COUNT(*) FILTER (WHERE status='deduplicated') as deduplicated,
                COUNT(*) FILTER (WHERE status='blocked') as blocked,
                COUNT(*) FILTER (WHERE status='error') as errors
         FROM audit_log WHERE created_at >= date_trunc('month', NOW())",
    )
    .fetch_one(&state.pg)
    .await?;

    Ok(Json(json!({
        "deployment_mode": state.deployment_mode,
        "total_users": user_count,
        "total_orgs": org_count,
        "this_month": month_totals,
    })))
}

async fn services(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let service_names: Vec<String> = state.curated_services.iter().cloned().collect();
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;

    let mut conn = state.redis.get_multiplexed_async_connection().await?;
    let (circuit_states, _) = agentraas_core::circuit_breaker::get_circuit_states_batch(&mut conn, &service_names).await?;

    #[derive(sqlx::FromRow)]
    struct Row {
        service: String,
        total: i64,
        last_used: chrono::DateTime<chrono::Utc>,
    }
    let rows: Vec<Row> = if org_ids.is_empty() {
        vec![]
    } else {
        sqlx::query_as(
            "SELECT service, COUNT(*) as total, (MAX(created_at) AT TIME ZONE 'UTC') as last_used
             FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours' AND service = ANY($1) AND org_id = ANY($2)
             GROUP BY service",
        )
        .bind(&service_names)
        .bind(&org_ids)
        .fetch_all(&state.pg)
        .await?
    };
    let stats_map: HashMap<String, (i64, chrono::DateTime<chrono::Utc>)> = rows.into_iter().map(|r| (r.service, (r.total, r.last_used))).collect();

    Ok(Json(
        service_names
            .into_iter()
            .map(|svc| {
                let stats = stats_map.get(&svc);
                json!({
                    "name": svc,
                    "circuit_state": circuit_states.get(&svc).cloned().unwrap_or_else(|| "closed".to_string()),
                    "actions_24h": stats.map(|s| s.0).unwrap_or(0),
                    "last_used": stats.map(|s| s.1),
                })
            })
            .collect(),
    ))
}

async fn reliability_report(State(state): State<SharedState>, user: AuthUser, Query(q): Query<RangeQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let range = q.range.unwrap_or_else(|| "24h".to_string());
    let (Some(interval), Some(range_secs)) = (range_interval(&range), range_seconds(&range)) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid range. Use one of: 24h, 7d, 30d, 90d."));
    };
    let service_names: Vec<String> = state.curated_services.iter().cloned().collect();
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;

    let uptime_sql = format!(
        "WITH events AS (
           SELECT service, to_state, occurred_at,
                  LEAD(occurred_at) OVER (PARTITION BY service ORDER BY occurred_at) AS next_at
           FROM circuit_breaker_events
           WHERE service = ANY($1) AND occurred_at >= NOW() - INTERVAL '{interval}'
         )
         SELECT service,
                COALESCE(SUM(EXTRACT(EPOCH FROM (LEAST(COALESCE(next_at, NOW()), NOW()) - occurred_at))) FILTER (WHERE to_state = 'open'), 0)::float8 AS open_seconds
         FROM events GROUP BY service"
    );
    let uptime_rows: Vec<(String, f64)> = sqlx::query_as(&uptime_sql).bind(&service_names).fetch_all(&state.pg).await?;

    #[derive(sqlx::FromRow)]
    struct StatRow {
        service: String,
        success: i64,
        errors: i64,
        duplicates_prevented: i64,
        total_actions: i64,
    }
    let stats_rows: Vec<StatRow> = if org_ids.is_empty() {
        vec![]
    } else {
        let sql = format!(
            "SELECT service,
                    COUNT(*) FILTER (WHERE status = 'success') AS success,
                    COUNT(*) FILTER (WHERE status = 'error') AS errors,
                    COUNT(*) FILTER (WHERE status = 'deduplicated') AS duplicates_prevented,
                    COUNT(*) AS total_actions
             FROM audit_log
             WHERE created_at >= NOW() - INTERVAL '{interval}' AND service = ANY($1) AND org_id = ANY($2)
             GROUP BY service"
        );
        sqlx::query_as(&sql).bind(&service_names).bind(&org_ids).fetch_all(&state.pg).await?
    };

    let open_seconds_map: HashMap<String, f64> = uptime_rows.into_iter().collect();
    let stats_map: HashMap<String, StatRow> = stats_rows.into_iter().map(|r| (r.service.clone(), r)).collect();

    let report: Vec<Value> = service_names
        .into_iter()
        .map(|svc| {
            let open_seconds = open_seconds_map.get(&svc).copied().unwrap_or(0.0).min(range_secs as f64);
            let uptime_pct = ((1.0 - open_seconds / range_secs as f64) * 10000.0).round() / 100.0;
            let stats = stats_map.get(&svc);
            let success = stats.map(|s| s.success).unwrap_or(0);
            let errors = stats.map(|s| s.errors).unwrap_or(0);
            let success_rate = if success + errors > 0 {
                Some(((success as f64 / (success + errors) as f64) * 10000.0).round() / 100.0)
            } else {
                None
            };
            json!({
                "service": svc,
                "uptime_pct": uptime_pct,
                "success_rate": success_rate,
                "duplicates_prevented": stats.map(|s| s.duplicates_prevented).unwrap_or(0),
                "total_actions": stats.map(|s| s.total_actions).unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(json!({ "range": range, "services": report })))
}

async fn export_csv(State(state): State<SharedState>, user: AuthUser) -> Result<impl IntoResponse, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    let headers = ["Timestamp", "Request ID", "Org", "Agent", "Service", "Action", "Status", "Error", "Duration_ms"];

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_TYPE, "text/csv".parse().unwrap());
    response_headers.insert(header::CONTENT_DISPOSITION, "attachment; filename=\"agentraas-audit.csv\"".parse().unwrap());

    if org_ids.is_empty() {
        return Ok((response_headers, headers.join(",")));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        created_at: chrono::DateTime<chrono::Utc>,
        req_id: String,
        org_id: String,
        agent_id: String,
        service: String,
        action: String,
        status: String,
        error_type: Option<String>,
        duration_ms: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT (created_at AT TIME ZONE 'UTC') as created_at, req_id, org_id, agent_id, service, action, status, error_type, duration_ms::bigint as duration_ms
         FROM audit_log WHERE org_id = ANY($1) ORDER BY created_at DESC LIMIT 10000",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;

    fn csv_field(f: &str) -> String {
        format!("\"{}\"", f.replace('"', "\"\""))
    }
    let mut lines = vec![headers.join(",")];
    for r in rows {
        let fields = [
            r.created_at.to_rfc3339(),
            r.req_id,
            r.org_id,
            r.agent_id,
            r.service,
            r.action,
            r.status,
            r.error_type.unwrap_or_default(),
            r.duration_ms.to_string(),
        ];
        lines.push(fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(","));
    }
    Ok((response_headers, lines.join("\n")))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

/// Recomputes each row's hash from its stored fields and compares to what
/// was actually persisted, walking the chain — any row altered after
/// insert breaks the chain from that point forward. The hash chain itself
/// (`prev_hash`/`row_hash`) is computed unconditionally by a Postgres
/// trigger for every row; this endpoint (which makes use of it) is the
/// Enterprise-gated part.
async fn admin_audit_verify_integrity(State(state): State<SharedState>, user: AuthUser, Query(q): Query<LimitQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;
    crate::state::require_enterprise_mode(&state)?;

    let limit = q.limit.unwrap_or(5000).clamp(1, 50000);
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        req_id: String,
        org_id: Option<String>,
        service: String,
        action: String,
        status: String,
        created_at_text: String,
        prev_hash: Option<String>,
        row_hash: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, req_id, org_id, service, action, status, created_at::text AS created_at_text, prev_hash, row_hash
         FROM audit_log WHERE row_hash IS NOT NULL ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.pg)
    .await?;

    let by_row_hash: std::collections::HashSet<&str> = rows.iter().filter_map(|r| r.row_hash.as_deref()).collect();

    let mut broken_ids = Vec::new();
    for row in &rows {
        use sha2::{Digest, Sha256};
        let input = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            row.prev_hash.as_deref().unwrap_or(""),
            row.req_id,
            row.org_id.as_deref().unwrap_or(""),
            row.service,
            row.action,
            row.status,
            row.created_at_text,
        );
        let expected = hex::encode(Sha256::digest(input.as_bytes()));
        if row.row_hash.as_deref() != Some(expected.as_str()) {
            broken_ids.push(row.id);
        }
    }
    let rows_with_unresolved_predecessor = rows.iter().filter(|r| r.prev_hash.as_deref().is_some_and(|p| !by_row_hash.contains(p))).count();

    let checked = rows.len() as i64;
    Ok(Json(json!({
        "checked": checked,
        "intact": broken_ids.is_empty(),
        "broken_ids": broken_ids,
        "rows_with_predecessor_outside_window": rows_with_unresolved_predecessor,
        "note": if checked >= limit {
            format!("Checked the most recent {limit} rows only — pass a higher ?limit to check further back.")
        } else {
            "Checked every hash-chained row in the table.".to_string()
        },
    })))
}

#[derive(Deserialize)]
struct SiemExportQuery {
    since: Option<String>,
    until: Option<String>,
}

/// NDJSON export — the most broadly ingestible shape for Splunk HEC,
/// Datadog, Elastic, and most other SIEM/log-pipeline tools. Defaults to
/// the last 24h. Capped at 50k rows per request.
async fn admin_audit_siem_export(State(state): State<SharedState>, user: AuthUser, Query(q): Query<SiemExportQuery>) -> Result<impl IntoResponse, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;
    crate::state::require_enterprise_mode(&state)?;

    let since = q.since.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(|| chrono::Utc::now() - chrono::Duration::hours(24));
    let until = q.until.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or_else(chrono::Utc::now);

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        req_id: String,
        api_key: String,
        org_id: Option<String>,
        agent_id: String,
        service: String,
        action: String,
        status: String,
        error_type: Option<String>,
        duration_ms: i64,
        payload_hash: Option<String>,
        row_hash: Option<String>,
        prev_hash: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, req_id, api_key, org_id, agent_id, service, action, status, error_type, duration_ms::bigint as duration_ms,
                payload_hash, row_hash, prev_hash, (created_at AT TIME ZONE 'UTC') as created_at
         FROM audit_log WHERE created_at >= $1 AND created_at <= $2 ORDER BY id ASC LIMIT 50000",
    )
    .bind(since)
    .bind(until)
    .fetch_all(&state.pg)
    .await?;

    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::CONTENT_TYPE, "application/x-ndjson".parse().unwrap());
    response_headers.insert(header::CONTENT_DISPOSITION, "attachment; filename=\"agentraas-audit-siem-export.ndjson\"".parse().unwrap());

    if rows.is_empty() {
        return Ok((response_headers, String::new()));
    }
    let mut body = String::new();
    for r in rows {
        let line = json!({
            "source": "agentraas",
            "event_type": "agent_action",
            "event_id": r.id,
            "request_id": r.req_id,
            "timestamp": r.created_at,
            "org_id": r.org_id,
            "agent_id": r.agent_id,
            "api_key_prefix": r.api_key,
            "service": r.service,
            "action": r.action,
            "outcome": r.status,
            "error_type": r.error_type,
            "duration_ms": r.duration_ms,
            "payload_hash": r.payload_hash,
            "integrity": { "row_hash": r.row_hash, "prev_hash": r.prev_hash },
        });
        body.push_str(&line.to_string());
        body.push('\n');
    }
    Ok((response_headers, body))
}
