//! Pay-as-you-go Cloud billing: $1 per 1,000 actions that ran, after the
//! free monthly allowance (`CLOUD_MONTHLY_LIMIT`, 500), up to a monthly
//! spend cap each org sets. Off unless `BILLING_PAYG_ENABLED=true` (and
//! cloud mode): until then nobody can check out the `payg` plan and the
//! charge loop never starts.
//!
//! Paddle has no metering: a `payg` customer subscribes to a $0/month
//! price (`PADDLE_PAYG_PRICE_ID`) that saves their card, and once a month
//! this file bills the previous month as a one-time charge on that
//! subscription (`POST /subscriptions/{id}/charge`, effective immediately,
//! a non-catalog $1 price on `PADDLE_PAYG_PRODUCT_ID`, quantity = blocks).
//! Charged in whole 1,000-action blocks; the remainder carries over, so
//! nobody gets a sub-dollar charge.
//!
//! "Actions that ran" = `audit_log` rows with status `success` (forwarded
//! and completed, including approved HITL calls and DLQ replays).
//! Duplicates answered from cache, blocked and denied calls, and calls
//! waiting for approval are never billed.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};
use crate::util::configured_env;

pub const BLOCK_ACTIONS: i64 = 1_000;
pub const BLOCK_PRICE_CENTS: i64 = 100;
/// Cap for an org that never set one.
pub const DEFAULT_CAP_USD: i64 = 10;
const CHARGE_LOOP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

pub fn enabled(state: &SharedState) -> bool {
    state.deployment_mode == "cloud" && configured_env("BILLING_PAYG_ENABLED").as_deref() == Some("true")
}

/// (blocks to charge, actions carried into next month).
pub fn compute_charge(actions: i64, carried_in: i64, free: i64) -> (i64, i64) {
    let billable = (actions - free).max(0) + carried_in.max(0);
    (billable / BLOCK_ACTIONS, billable % BLOCK_ACTIONS)
}

/// Actions allowed this month: the free allowance plus what the cap buys,
/// less the remainder already owed from last month.
pub fn monthly_limit(free: i64, cap_usd: i64, carried_in: i64) -> i64 {
    (free + cap_usd * BLOCK_ACTIONS * 100 / BLOCK_PRICE_CENTS - carried_in).max(free)
}

/// "YYYY-MM" for `months_back` months before the current UTC month.
fn month_key(months_back: u32) -> (String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    use chrono::{Datelike, TimeZone};
    let now = chrono::Utc::now();
    let (mut y, mut m) = (now.year(), now.month() as i32 - months_back as i32);
    while m < 1 {
        m += 12;
        y -= 1;
    }
    let start = chrono::Utc.with_ymd_and_hms(y, m as u32, 1, 0, 0, 0).unwrap();
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m as u32 + 1) };
    let end = chrono::Utc.with_ymd_and_hms(ny, nm, 1, 0, 0, 0).unwrap();
    (format!("{y}-{m:02}"), start, end)
}

pub async fn is_payg_org(pg: &sqlx::PgPool, org_id: &str) -> Result<bool, sqlx::Error> {
    let row: Option<i32> = sqlx::query_scalar("SELECT 1 FROM users WHERE org_id = $1 AND plan = 'payg' LIMIT 1").bind(org_id).fetch_optional(pg).await?;
    Ok(row.is_some())
}

async fn cap_usd(pg: &sqlx::PgPool, org_id: &str) -> Result<i64, sqlx::Error> {
    let cap: Option<i32> = sqlx::query_scalar("SELECT monthly_cap_usd FROM billing_caps WHERE org_id = $1").bind(org_id).fetch_optional(pg).await?;
    Ok(cap.map(i64::from).unwrap_or(DEFAULT_CAP_USD))
}

async fn carried_into(pg: &sqlx::PgPool, org_id: &str, months_back: u32) -> Result<i64, sqlx::Error> {
    let (prev, _, _) = month_key(months_back + 1);
    let carried: Option<i64> = sqlx::query_scalar("SELECT carried_out FROM billing_charges WHERE org_id = $1 AND month = $2 AND status IN ('charged', 'skipped')")
        .bind(org_id)
        .bind(&prev)
        .fetch_optional(pg)
        .await?;
    Ok(carried.unwrap_or(0))
}

async fn actions_in(pg: &sqlx::PgPool, org_id: &str, months_back: u32) -> Result<i64, sqlx::Error> {
    let (_, start, end) = month_key(months_back);
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE org_id = $1 AND status = 'success' AND created_at >= $2 AND created_at < $3")
        .bind(org_id)
        .bind(start)
        .bind(end)
        .fetch_one(pg)
        .await
}

/// The current month's action limit for a `payg` org (see `get_effective_limit`).
pub async fn payg_limit(state: &SharedState, org_id: &str) -> Result<i64, sqlx::Error> {
    Ok(monthly_limit(state.cloud_monthly_limit, cap_usd(&state.pg, org_id).await?, carried_into(&state.pg, org_id, 0).await?))
}

pub fn router() -> Router<SharedState> {
    Router::new().route("/api/v1/billing/usage", get(get_usage).put(put_cap))
}

#[derive(Deserialize)]
struct OrgQuery {
    org_id: String,
}

async fn member_org(state: &SharedState, user: &AuthUser, org_id: &str) -> Result<(), ApiError> {
    if !is_valid_identifier(org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !get_user_org_ids(&state.pg, user.sub).await?.iter().any(|o| o == org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    Ok(())
}

async fn get_usage(State(state): State<SharedState>, user: AuthUser, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    member_org(&state, &user, &q.org_id).await?;
    let (month, _, _) = month_key(0);
    let actions = actions_in(&state.pg, &q.org_id, 0).await?;
    let carried_in = carried_into(&state.pg, &q.org_id, 0).await?;
    let (blocks, _) = compute_charge(actions, carried_in, state.cloud_monthly_limit);
    Ok(Json(json!({
        "enabled": enabled(&state),
        "payg": is_payg_org(&state.pg, &q.org_id).await?,
        "month": month,
        "actions": actions,
        "free_actions": state.cloud_monthly_limit,
        "carried_in": carried_in,
        "monthly_cap_usd": cap_usd(&state.pg, &q.org_id).await?,
        "estimated_cents": blocks * BLOCK_PRICE_CENTS,
    })))
}

#[derive(Deserialize)]
struct CapBody {
    org_id: String,
    monthly_cap_usd: i64,
}

async fn put_cap(State(state): State<SharedState>, user: AuthUser, Json(body): Json<CapBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    member_org(&state, &user, &body.org_id).await?;
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if !(1..=100_000).contains(&body.monthly_cap_usd) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "monthly_cap_usd must be a whole number of dollars from 1 to 100000."));
    }
    sqlx::query(
        "INSERT INTO billing_caps (org_id, monthly_cap_usd, updated_at) VALUES ($1, $2, NOW())
         ON CONFLICT (org_id) DO UPDATE SET monthly_cap_usd = EXCLUDED.monthly_cap_usd, updated_at = NOW()",
    )
    .bind(&body.org_id)
    .bind(body.monthly_cap_usd as i32)
    .execute(&state.pg)
    .await?;
    Ok(Json(json!({ "org_id": body.org_id, "monthly_cap_usd": body.monthly_cap_usd })))
}

/// Hourly, so a month still gets billed when the box was off at midnight
/// on the 1st. Each org/month is claimed once in `billing_charges`.
pub fn spawn_monthly_charge_loop(state: SharedState) {
    if !enabled(&state) {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(CHARGE_LOOP_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = charge_previous_month(&state).await {
                tracing::error!(?err, "payg charge run failed");
            }
        }
    });
}

async fn charge_previous_month(state: &SharedState) -> Result<(), sqlx::Error> {
    let (month, _, _) = month_key(1);
    // ponytail: one org per payg account (users.org_id); usage in other
    // orgs the user owns isn't billed. Usage in the month a subscription is
    // canceled isn't billed either (the webhook drops the plan to free).
    let accounts: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (u.org_id) u.org_id, s.paddle_subscription_id FROM users u
         JOIN subscriptions s ON s.user_id = u.id AND s.status IN ('active', 'trialing', 'past_due')
         WHERE u.plan = 'payg' AND u.org_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM billing_charges b WHERE b.org_id = u.org_id AND b.month = $1)
         ORDER BY u.org_id, s.updated_at DESC",
    )
    .bind(&month)
    .fetch_all(&state.pg)
    .await?;

    for (org_id, sub_id) in accounts {
        let actions = actions_in(&state.pg, &org_id, 1).await?;
        let carried_in = carried_into(&state.pg, &org_id, 1).await?;
        let (blocks, carried_out) = compute_charge(actions, carried_in, state.cloud_monthly_limit);
        let status = if blocks == 0 { "skipped" } else { "pending" };
        let claimed = sqlx::query(
            "INSERT INTO billing_charges (org_id, month, actions, carried_in, blocks, carried_out, amount_cents, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (org_id, month) DO NOTHING",
        )
        .bind(&org_id)
        .bind(&month)
        .bind(actions)
        .bind(carried_in)
        .bind(blocks)
        .bind(carried_out)
        .bind(blocks * BLOCK_PRICE_CENTS)
        .bind(status)
        .execute(&state.pg)
        .await?
        .rows_affected();
        if claimed == 0 || blocks == 0 {
            continue;
        }

        let (status, error) = match paddle_charge(state, &sub_id, &month, blocks).await {
            Ok(()) => ("charged", None),
            Err((definite, msg)) => {
                tracing::error!(org_id, month, msg, "payg charge did not go through");
                (if definite { "failed" } else { "unknown" }, Some(msg))
            }
        };
        sqlx::query("UPDATE billing_charges SET status = $3, error = $4, updated_at = NOW() WHERE org_id = $1 AND month = $2")
            .bind(&org_id)
            .bind(&month)
            .bind(status)
            .bind(error)
            .execute(&state.pg)
            .await?;
    }
    Ok(())
}

/// Err((true, _)): Paddle refused, nothing charged. Err((false, _)): no
/// answer, it may have charged; never retried automatically (Paddle's
/// charge endpoint has no idempotency key).
async fn paddle_charge(state: &SharedState, sub_id: &str, month: &str, blocks: i64) -> Result<(), (bool, String)> {
    let (Some(api_key), Some(product_id)) = (configured_env("PADDLE_API_KEY"), configured_env("PADDLE_PAYG_PRODUCT_ID")) else {
        return Err((true, "PADDLE_API_KEY or PADDLE_PAYG_PRODUCT_ID not set".to_string()));
    };
    let base = if std::env::var("PADDLE_ENVIRONMENT").as_deref() == Ok("production") { "https://api.paddle.com" } else { "https://sandbox-api.paddle.com" };
    let body = json!({
        "effective_from": "immediately",
        "items": [{
            "quantity": blocks,
            "price": {
                "description": format!("AgentRaaS Cloud actions, {month} (per 1,000)"),
                "name": "Actions (per 1,000)",
                "product_id": product_id,
                "unit_price": { "amount": BLOCK_PRICE_CENTS.to_string(), "currency_code": "USD" },
            },
        }],
    });
    let res = state
        .http_client
        .post(format!("{base}/subscriptions/{sub_id}/charge"))
        .bearer_auth(api_key)
        .timeout(std::time::Duration::from_secs(60))
        .json(&body)
        .send()
        .await
        .map_err(|e| (e.is_connect(), format!("request error: {e}")))?;
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    let text = res.text().await.unwrap_or_default();
    Err((status.is_client_error(), format!("paddle {status}: {}", text.chars().take(500).collect::<String>())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_allowance_is_never_billed() {
        assert_eq!(compute_charge(0, 0, 500), (0, 0));
        assert_eq!(compute_charge(500, 0, 500), (0, 0));
    }

    #[test]
    fn bills_whole_blocks_and_carries_the_rest() {
        assert_eq!(compute_charge(1_499, 0, 500), (0, 999));
        assert_eq!(compute_charge(1_500, 0, 500), (1, 0));
        assert_eq!(compute_charge(12_345, 0, 500), (11, 845));
        assert_eq!(compute_charge(500, 999, 500), (0, 999));
        assert_eq!(compute_charge(501, 999, 500), (1, 0));
    }

    #[test]
    fn limit_is_free_plus_what_the_cap_buys() {
        assert_eq!(monthly_limit(500, 10, 0), 10_500);
        assert_eq!(monthly_limit(500, 10, 999), 9_501);
        assert_eq!(monthly_limit(500, 1, 5_000), 500);
        // At the limit, the bill is exactly the cap.
        let (blocks, _) = compute_charge(monthly_limit(500, 10, 0), 0, 500);
        assert_eq!(blocks * BLOCK_PRICE_CENTS, 10 * 100);
    }

    #[test]
    fn month_keys_roll_over_the_year() {
        let (now, start, end) = month_key(0);
        assert_eq!(now, start.format("%Y-%m").to_string());
        assert!(end > start);
        let (prev, _, prev_end) = month_key(1);
        assert_eq!(prev_end, start);
        assert_ne!(prev, now);
        let (_, s12, _) = month_key(12);
        assert_eq!(s12.format("%m").to_string(), start.format("%m").to_string());
    }
}
