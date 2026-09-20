//! Per-agent action spend/call caps (SPEC-SPEND-CAPS.md). Call-count
//! based, not dollar-based — v1 deliberately reuses the exact windowed
//! Redis INCR+EXPIRE shape `increment_monthly_usage` (agent/db.rs) already
//! uses, just parameterized by hour/day instead of hardcoded month.
//!
//! Deliberately NOT under `ee/`, unlike HITL: block-only rules must work
//! on the Community/public build too (this is a safety control on the
//! org's own spend, not a paid feature to gate). The `on_exceed = "hitl"`
//! option requires Tier::Team at creation time (checked here) AND the
//! `enterprise` Cargo feature to be compiled in (also checked here, since
//! the actual freeze call in `agent/mod.rs` is `#[cfg(feature =
//! "enterprise")]`-gated like every other HITL call site) — rejecting at
//! creation time means the enforcement path never has to guess what to do
//! with an 'hitl' rule it can't act on.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids, require_tier};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/spend-cap-rules", post(create_rule).get(list_rules))
        .route("/api/v1/spend-cap-rules/:id", axum::routing::delete(delete_rule))
}

#[derive(Deserialize)]
struct CreateRuleBody {
    org_id: String,
    agent_id: Option<String>,
    service: String,
    action: String,
    window: String,
    max_calls: i32,
    on_exceed: String,
}

async fn create_rule(State(state): State<SharedState>, user: AuthUser, Json(body): Json<CreateRuleBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if body.service.is_empty() || body.action.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "service and action are required."));
    }
    if let Some(agent_id) = &body.agent_id {
        if !is_valid_identifier(agent_id) {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
        }
    }
    if !matches!(body.window.as_str(), "hour" | "day") {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "window must be \"hour\" or \"day\"."));
    }
    if body.max_calls <= 0 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "max_calls must be a positive number."));
    }
    match body.on_exceed.as_str() {
        "block" => {}
        "hitl" => {
            require_tier(&state, &body.org_id, agentraas_core::tier::Tier::Team).await?;
            if !cfg!(feature = "enterprise") {
                return Err(ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Routing to HITL approval needs the Enterprise-featured build (ee/) - this deployment doesn't have it. Use on_exceed=\"block\" instead.",
                ));
            }
        }
        _ => return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "on_exceed must be \"block\" or \"hitl\".")),
    }

    let id: i32 = sqlx::query_scalar(
        "INSERT INTO spend_cap_rules (org_id, agent_id, service, action, time_window, max_calls, on_exceed, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(&body.org_id)
    .bind(&body.agent_id)
    .bind(&body.service)
    .bind(&body.action)
    .bind(&body.window)
    .bind(body.max_calls)
    .bind(&body.on_exceed)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;
    Ok(Json(json!({ "id": id })))
}

async fn list_rules(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    // No tier gate here (same reasoning as hitl::list_rules): spans every
    // org the user belongs to, and a downgraded org should still be able
    // to see/clean up rules it made while on Team. Access stays scoped by
    // get_user_org_ids below.
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "rules": [] })));
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'org_id', org_id, 'agent_id', agent_id, 'service', service,
                'action', action, 'window', time_window, 'max_calls', max_calls, 'on_exceed', on_exceed)
         FROM spend_cap_rules WHERE org_id = ANY($1) ORDER BY created_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "rules": rows })))
}

async fn delete_rule(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Spend-cap rule not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM spend_cap_rules WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Spend-cap rule not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}

// ─── enforcement (called from agent::mod::handle_request) ───

#[derive(sqlx::FromRow, Clone)]
pub struct MatchedSpendCapRule {
    pub id: i32,
    pub max_calls: i32,
    pub window: String,
    pub on_exceed: String,
}

pub struct SpendCapCheck {
    pub allowed: bool,
    pub rule: Option<MatchedSpendCapRule>,
    pub count: i64,
}

fn window_bucket(window: &str) -> String {
    let now = chrono::Utc::now();
    if window == "hour" {
        now.format("%Y-%m-%dT%H").to_string()
    } else {
        now.format("%Y-%m-%d").to_string()
    }
}

fn window_ttl_seconds(window: &str) -> i64 {
    // A little slack past the window boundary so a key never expires
    // mid-check due to clock/Redis timing skew - matches
    // increment_monthly_usage's own "longer than the window" TTL choice.
    if window == "hour" { 3600 + 300 } else { 86400 + 300 }
}

/// Looks up the most specific matching rule (a per-agent row wins over an
/// org-wide `agent_id IS NULL` row for the same service.action - same
/// "most specific match" convention `resolve_route` already uses for
/// Custom Actions), then atomically increments its window counter.
/// No matching rule = always allowed; this is the common case, so it's a
/// single indexed query away rather than a per-request Redis lookup when
/// there's nothing to enforce.
pub async fn check_and_increment(state: &SharedState, org_id: &str, agent_id: &str, service: &str, action: &str) -> Result<SpendCapCheck, ApiError> {
    let rule: Option<MatchedSpendCapRule> = sqlx::query_as(
        "SELECT id, max_calls, time_window AS window, on_exceed FROM spend_cap_rules
         WHERE org_id = $1 AND service = $2 AND action = $3 AND (agent_id = $4 OR agent_id IS NULL)
         ORDER BY agent_id NULLS LAST LIMIT 1",
    )
    .bind(org_id)
    .bind(service)
    .bind(action)
    .bind(agent_id)
    .fetch_optional(&state.pg)
    .await?;

    let Some(rule) = rule else {
        return Ok(SpendCapCheck { allowed: true, rule: None, count: 0 });
    };

    let key = format!("spendcap:{}:{}:{}:{}:{}", org_id, agent_id, service, action, window_bucket(&rule.window));
    let mut conn = state
        .redis_conn_result()
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Spend-cap counter unavailable."))?;
    let count: i64 = redis::cmd("INCR")
        .arg(&key)
        .query_async(&mut conn)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Spend-cap counter failed."))?;
    if count == 1 {
        let _: Result<(), _> = redis::cmd("EXPIRE").arg(&key).arg(window_ttl_seconds(&rule.window)).query_async(&mut conn).await;
    }

    let allowed = count <= rule.max_calls as i64;
    Ok(SpendCapCheck { allowed, rule: Some(rule), count })
}
