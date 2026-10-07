//! Team/Enterprise features (FSL-licensed, see LICENSE.md), compiled in with
//! the `enterprise` Cargo feature. Each route also checks the org's tier
//! (`require_tier`) or the server's `ENTERPRISE_MODE` switch.

pub mod hitl;
pub mod identity;
pub mod inbound_webhooks;
pub mod maintenance;
pub mod output_sanitization;
pub mod sso;

use axum::http::StatusCode;

use crate::state::{ApiError, SharedState};

pub use crate::state::require_enterprise_mode;

pub const ROLE_VALUES: &[&str] = &["admin", "developer", "auditor"];

/// Fresh-from-DB role lookup — deliberately not cached, same "always
/// recheck, never trust a stale value" pattern as `requireAdmin`.
pub async fn get_role(state: &SharedState, user_id: i32, org_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT role FROM org_members WHERE user_id = $1 AND org_id = $2")
        .bind(user_id)
        .bind(org_id)
        .fetch_optional(&state.pg)
        .await
}

/// Upserts the `org_members` row for (userId, orgId) with the freshly
/// resolved role — always overwrites, never merges.
pub async fn upsert_membership(state: &SharedState, user_id: i32, org_id: &str, role: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO org_members (user_id, org_id, role, updated_at) VALUES ($1, $2, $3, NOW())
         ON CONFLICT (user_id, org_id) DO UPDATE SET role = EXCLUDED.role, updated_at = NOW()",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(role)
    .execute(&state.pg)
    .await?;
    Ok(())
}

/// Per-org admin check for Enterprise SSO/RBAC — a DIFFERENT axis from
/// `is_admin` (one system-wide cloud-operator account). Checks
/// `org_members.role` for the org named in the route, fresh from the DB.
/// Bootstrap fallback: a brand-new org has no `org_members` admin row yet,
/// so falls back to the existing loose ownership notion
/// (`get_user_org_ids`) — whoever already owns this org_id in that sense
/// can bootstrap it into a real admin membership row, once.
pub async fn require_org_admin(state: &SharedState, user_id: i32, org_id: &str) -> Result<(), ApiError> {
    if get_role(state, user_id, org_id).await?.as_deref() == Some("admin") {
        return Ok(());
    }

    let admin_exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM org_members WHERE org_id = $1 AND role = 'admin' LIMIT 1")
        .bind(org_id)
        .fetch_optional(&state.pg)
        .await?;
    if admin_exists.is_none() {
        let owned_org_ids = crate::agent::db::get_user_org_ids(&state.pg, user_id).await?;
        if owned_org_ids.iter().any(|id| id == org_id) {
            sqlx::query("INSERT INTO orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING").bind(org_id).execute(&state.pg).await?;
            upsert_membership(state, user_id, org_id, "admin").await?;
            return Ok(());
        }
    }

    Err(ApiError::new(StatusCode::FORBIDDEN, "Org admin access required."))
}
