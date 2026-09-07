//! Self-host license handling: verification/refresh (this deployment's
//! own license, see the top half below) and issuance (a customer's
//! license, only ever done on agentraas.io — the bottom half).
//!
//! **Verification** reads and periodically re-verifies `LICENSE_TOKEN`
//! (see `agentraas_core::license` for the token format and signature
//! verification itself), keeping `SharedState::license_tier` current.
//! Cloud never touches this at all (see `agent::db::effective_tier`,
//! which branches on `deployment_mode` before ever reaching here).
//!
//! **Issuance** signs a token on demand (`GET /api/v1/licensing/token`)
//! rather than pre-computing one at webhook time — `users.plan` and
//! `subscriptions.current_period_end` are already exactly the inputs a
//! license needs, and both are already kept correct by the existing
//! `paddle_webhook` handler in `long_tail.rs` (untouched by this file).
//! Signing lazily means there's no separate token storage to keep in
//! sync or invalidate — every dashboard fetch just re-signs fresh claims
//! over whatever the billing tables currently say, right up until the
//! subscription's own period end.

use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::get_user_org_ids;
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

// Day/month-scale TTLs don't need frequent re-checking — twice a day is
// plenty to pick up a renewal or catch an expiry without a restart.
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 3600);

fn resolve_from_env() -> agentraas_core::tier::Tier {
    let Ok(token) = std::env::var("LICENSE_TOKEN") else {
        return agentraas_core::tier::Tier::Community;
    };
    if token.trim().is_empty() {
        return agentraas_core::tier::Tier::Community;
    }
    match agentraas_core::license::verify(&token) {
        Some(license) => {
            tracing::info!(tier = ?license.tier, customer_id = %license.customer_id, "license token verified");
            license.tier
        }
        None => {
            tracing::warn!("LICENSE_TOKEN is set but invalid, expired, or tampered — falling back to Community tier");
            agentraas_core::tier::Tier::Community
        }
    }
}

/// Initial value for `AppState::license_tier`, read once at startup —
/// never blocks/fails startup regardless of what `LICENSE_TOKEN` holds.
pub fn initial_tier() -> agentraas_core::tier::Tier {
    resolve_from_env()
}

/// Background loop — re-verifies `LICENSE_TOKEN` periodically so a
/// renewed, corrected, or removed token takes effect without a restart.
/// Mirrors `health_checks::spawn_health_check_loop`'s pattern. A no-op
/// for cloud deployments, which never read `license_tier` at all.
pub fn spawn_license_refresh_loop(state: SharedState) {
    if state.deployment_mode == "cloud" {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        interval.tick().await; // tokio's first tick fires immediately — startup already resolved the initial value, skip re-doing it right away
        loop {
            interval.tick().await;
            let tier = resolve_from_env();
            *state.license_tier.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = tier;
        }
    });
}

// ─── issuance (agentraas.io only) ───

pub fn router() -> Router<SharedState> {
    Router::new().route("/api/v1/licensing/token", get(get_license_token))
}

#[derive(Deserialize)]
struct OrgQuery {
    org_id: String,
}

/// Default license window when the org's plan has no matching active
/// subscription row to read a real `current_period_end` from — an
/// inconsistent state that shouldn't normally happen (plan is only ever
/// set alongside a subscription by the webhook), but if it does, a short
/// fallback window self-corrects on the next dashboard fetch rather than
/// either erroring out or handing out a long-lived token with nothing
/// backing it.
const FALLBACK_LICENSE_HOURS: i64 = 24;

async fn get_license_token(State(state): State<SharedState>, user: AuthUser, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&q.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !org_ids.contains(&q.org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }

    let owner: Option<(i32, String)> = sqlx::query_as("SELECT id, plan FROM users WHERE org_id = $1").bind(&q.org_id).fetch_optional(&state.pg).await?;
    let Some((owner_id, plan)) = owner else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Org not found."));
    };
    let tier = agentraas_core::tier::Tier::from_plan_str(&plan);
    if tier == agentraas_core::tier::Tier::Community {
        // Nothing to license — Community doesn't gate anything a
        // self-hoster would need a token for.
        return Ok(Json(json!({ "tier": "free", "token": null, "expires_at": null })));
    }

    let period_end: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT current_period_end FROM subscriptions WHERE user_id = $1 AND status IN ('active', 'trialing') ORDER BY current_period_end DESC NULLS LAST LIMIT 1")
            .bind(owner_id)
            .fetch_optional(&state.pg)
            .await?
            .flatten();
    let expires_at = period_end.unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(FALLBACK_LICENSE_HOURS));
    let ttl_seconds = (expires_at - chrono::Utc::now()).num_seconds().max(0) as u64;

    let Some(private_key_pem) = crate::util::configured_env("LICENSE_SIGNING_PRIVATE_KEY") else {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Licensing is not configured on this deployment."));
    };
    let token = agentraas_core::license::sign(&q.org_id, tier, ttl_seconds, &private_key_pem)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred."))?;

    Ok(Json(json!({ "tier": tier.as_plan_str(), "token": token, "expires_at": expires_at })))
}
