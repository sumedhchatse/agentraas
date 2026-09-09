//! DB-backed helpers `handle_request` depends on — mirrors the standalone
//! functions in `server.js` of the same name (getEffectiveValidationRule,
//! getEffectiveDedupRule, getCredential, verifyApiKey, getEffectiveRateLimit,
//! checkUsageLimit, incrementMonthlyUsage, getOrgOwnerPlan, getUserOrgIds,
//! checkAgencyTenantCap, checkOrgWritePermission, resolveCustomRoute,
//! logAudit).

use axum::http::StatusCode;
use serde_json::Value;
use sqlx::PgPool;

use crate::state::{ApiError, SharedState};

pub struct EffectiveValidationRule {
    pub fields: Value,
}

/// Custom org rule always wins if one exists; otherwise the curated
/// service's static config-driven rule (from `config/services.json`'s
/// `validation` block); `null` for Custom Actions with no rule.
pub async fn get_effective_validation_rule(
    state: &SharedState,
    org_id: &str,
    service: &str,
    action: &str,
) -> Result<Option<EffectiveValidationRule>, sqlx::Error> {
    let custom: Option<Value> = sqlx::query_scalar(
        "SELECT fields FROM custom_validation_rules WHERE org_id = $1 AND service = $2 AND action = $3",
    )
    .bind(org_id)
    .bind(service)
    .bind(action)
    .fetch_optional(&state.pg)
    .await?;
    if let Some(fields) = custom {
        return Ok(Some(EffectiveValidationRule { fields }));
    }
    if service == "custom" {
        return Ok(None);
    }
    let route_key = format!("{service}.{action}");
    Ok(state
        .service_routes
        .get(&route_key)
        .filter(|r| r.validation.is_object() && !r.validation.as_object().unwrap().is_empty())
        .map(|r| EffectiveValidationRule {
            fields: r.validation.clone(),
        }))
}

pub struct EffectiveDedupRule {
    pub fields: Vec<String>,
    /// Semantic/Entity-Level Idempotency Keys: an optional rule-specific
    /// dedup window (e.g. 15 minutes) instead of the 24h system default.
    pub ttl_seconds: Option<i64>,
    /// Whether to normalize each key field's value (trim/lowercase/
    /// numeric-coerce) before hashing, so trivially different-looking
    /// values for the same field still count as a duplicate.
    pub normalize: bool,
}

/// No static fallback — every action defaults to whole-payload-hash dedup
/// unless an org explicitly configures a field-based rule.
pub async fn get_effective_dedup_rule(
    pg: &PgPool,
    org_id: &str,
    service: &str,
    action: &str,
) -> Result<Option<EffectiveDedupRule>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        fields: Value,
        ttl_seconds: Option<i32>,
        normalize: bool,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT fields, ttl_seconds, normalize FROM custom_dedup_rules WHERE org_id = $1 AND service = $2 AND action = $3",
    )
    .bind(org_id)
    .bind(service)
    .bind(action)
    .fetch_optional(pg)
    .await?;
    Ok(row.map(|row| EffectiveDedupRule {
        ttl_seconds: row.ttl_seconds.map(i64::from),
        normalize: row.normalize,
        fields: row
            .fields
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    }))
}

/// Which hash function a request should dedupe on: idempotency key beats a
/// non-empty field rule beats whole-payload. Pulled out of the request
/// handlers so this decision is testable without an async handler, DB, or
/// Redis — a rule with empty `fields` (TTL-only) still falls through to
/// `Payload`, it just carries a custom TTL alongside it.
#[derive(Debug, PartialEq)]
pub enum DedupHashMode<'a> {
    IdempotencyKey(&'a str),
    Fields { fields: &'a [String], normalize: bool },
    Payload,
}

pub fn select_dedup_hash_mode<'a>(
    idempotency_key: Option<&'a str>,
    rule: Option<&'a EffectiveDedupRule>,
) -> DedupHashMode<'a> {
    if let Some(idem) = idempotency_key {
        DedupHashMode::IdempotencyKey(idem)
    } else if let Some(rule) = rule.filter(|r| !r.fields.is_empty()) {
        DedupHashMode::Fields { fields: &rule.fields, normalize: rule.normalize }
    } else {
        DedupHashMode::Payload
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(fields: &[&str], ttl_seconds: Option<i64>) -> EffectiveDedupRule {
        EffectiveDedupRule {
            fields: fields.iter().map(|s| s.to_string()).collect(),
            ttl_seconds,
            normalize: false,
        }
    }

    #[test]
    fn idempotency_key_wins_regardless_of_rule() {
        let r = rule(&["a"], Some(900));
        assert_eq!(select_dedup_hash_mode(Some("idem-1"), Some(&r)), DedupHashMode::IdempotencyKey("idem-1"));
        assert_eq!(select_dedup_hash_mode(Some("idem-1"), None), DedupHashMode::IdempotencyKey("idem-1"));
    }

    #[test]
    fn ttl_only_rule_falls_through_to_payload_hash() {
        let r = rule(&[], Some(900));
        assert_eq!(select_dedup_hash_mode(None, Some(&r)), DedupHashMode::Payload);
    }

    #[test]
    fn non_empty_fields_rule_selects_field_hash() {
        let r = rule(&["a", "b"], None);
        assert_eq!(
            select_dedup_hash_mode(None, Some(&r)),
            DedupHashMode::Fields { fields: &["a".to_string(), "b".to_string()], normalize: false }
        );
    }

    #[test]
    fn no_rule_selects_payload_hash() {
        assert_eq!(select_dedup_hash_mode(None, None), DedupHashMode::Payload);
    }
}

/// Tool Output Sanitization (Enterprise, opt-in per org) — default false, no
/// row means never toggled. Checked once per forwarded call in
/// `forward_action`; see `crates/api/src/ee/output_sanitization.rs` for the
/// GET/PUT dashboard pair that writes this table.
#[cfg(feature = "enterprise")]
pub async fn is_output_sanitization_enabled(pg: &PgPool, org_id: &str) -> Result<bool, sqlx::Error> {
    let enabled: Option<bool> = sqlx::query_scalar("SELECT enabled FROM org_output_sanitization WHERE org_id = $1")
        .bind(org_id)
        .fetch_optional(pg)
        .await?;
    Ok(enabled.unwrap_or(false))
}

/// Tool Result & Context Pruner — default false, no row means never
/// toggled. Community + Enterprise both get this (not cfg-gated), unlike
/// `is_output_sanitization_enabled`. Checked once per forwarded call in
/// `forward_action`; see `crates/api/src/pruning_settings.rs` for the
/// GET/PUT dashboard pair that writes this table.
pub async fn is_pruning_enabled(pg: &PgPool, org_id: &str) -> Result<bool, sqlx::Error> {
    let enabled: Option<bool> = sqlx::query_scalar("SELECT enabled FROM org_output_pruning WHERE org_id = $1")
        .bind(org_id)
        .fetch_optional(pg)
        .await?;
    Ok(enabled.unwrap_or(false))
}

#[derive(Debug, Clone, Default)]
pub struct Credential {
    pub api_key: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// User-supplied stored credential first (self-serve path), falling back
/// to an operator-set env var if nothing's been saved yet.
pub async fn get_credential(state: &SharedState, service: &str, org_id: &str) -> Option<Credential> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT encrypted_payload FROM service_credentials
         WHERE org_id=$1 AND service=$2 AND revoked_at IS NULL
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org_id)
    .bind(service)
    .fetch_optional(&state.pg)
    .await
    .ok()
    .flatten();

    if let Some(encrypted) = row {
        match state.cipher.decrypt(&encrypted) {
            Ok(plaintext) => match serde_json::from_str::<Value>(&plaintext) {
                Ok(v) => {
                    return Some(Credential {
                        api_key: v.get("api_key").and_then(Value::as_str).map(String::from),
                        username: v.get("username").and_then(Value::as_str).map(String::from),
                        password: v.get("password").and_then(Value::as_str).map(String::from),
                    })
                }
                Err(err) => tracing::error!(?err, service, org_id, "stored credential is not valid JSON"),
            },
            Err(err) => tracing::error!(?err, service, org_id, "failed to decrypt stored credential"),
        }
    }

    let env_var = format!("AGENTRAAS_KEY_{}_{}", service.to_uppercase(), org_id);
    let env_val = std::env::var(&env_var)
        .ok()
        .or_else(|| std::env::var(format!("AGENTRAAS_KEY_{}_DEFAULT", service.to_uppercase())).ok())?;

    if let Some((username, password)) = env_val.split_once(':') {
        Some(Credential {
            username: Some(username.to_string()),
            password: Some(password.to_string()),
            api_key: None,
        })
    } else {
        Some(Credential {
            api_key: Some(env_val),
            username: None,
            password: None,
        })
    }
}

pub struct ApiKeyVerification {
    pub ok: bool,
}

/// Backward-compatible by design: if nobody has ever run "Connect Agent"
/// for this org_id/agent_id, there's nothing to enforce against and the
/// request passes. Once at least one key exists for the pair, a valid
/// matching key becomes required.
/// Resolves the owning org from a raw API key alone, with no org_id/agent_id
/// supplied up front — unlike `verify_api_key` (used by webhook/SDK/MCP
/// `tools/call`, where the caller already states which org they're acting
/// as). Needed for MCP `tools/list`: it has no per-call arguments at all
/// (it's not a tool invocation), so the only signal available is whatever
/// key the client sends on every request to `/mcp`, same header
/// `tools/call` reads. Same hash+prefix scheme as `verify_api_key`.
pub async fn resolve_org_from_api_key(pg: &PgPool, provided_key: &str) -> Result<Option<String>, sqlx::Error> {
    if provided_key.is_empty() || provided_key == "anonymous" {
        return Ok(None);
    }
    let prefix: String = provided_key.chars().take(16).collect();
    let hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(provided_key.as_bytes());
        hex::encode(hasher.finalize())
    };
    sqlx::query_scalar("SELECT org_id FROM api_keys WHERE key_prefix=$1 AND key_hash=$2 AND revoked_at IS NULL")
        .bind(&prefix)
        .bind(&hash)
        .fetch_optional(pg)
        .await
}

/// Every org-specific validation-rule override in one query, keyed by
/// (service, action) — used by MCP `tools/list` to build a per-org schema
/// without one DB round trip per tool (there are ~29 curated tools).
pub async fn get_org_validation_overrides(
    pg: &PgPool,
    org_id: &str,
) -> Result<std::collections::HashMap<(String, String), Value>, sqlx::Error> {
    let rows: Vec<(String, String, Value)> =
        sqlx::query_as("SELECT service, action, fields FROM custom_validation_rules WHERE org_id = $1")
            .bind(org_id)
            .fetch_all(pg)
            .await?;
    Ok(rows.into_iter().map(|(service, action, fields)| ((service, action), fields)).collect())
}

pub async fn verify_api_key(
    pg: &PgPool,
    provided_key: &str,
    org_id: &str,
    agent_id: &str,
) -> Result<ApiKeyVerification, sqlx::Error> {
    // Zero-config convenience is scoped to the ORG, not the (org, agent_id)
    // pair — agent_id is a free-form string the caller supplies on every
    // request, so scoping the "no keys configured yet" bypass to it let
    // anyone with a real key for one agent skip auth entirely for any
    // other, never-used agent_id under the same org.
    let keys_exist: Option<i32> = sqlx::query_scalar("SELECT 1 FROM api_keys WHERE org_id=$1 AND revoked_at IS NULL LIMIT 1")
        .bind(org_id)
        .fetch_optional(pg)
        .await?;
    if keys_exist.is_none() {
        return Ok(ApiKeyVerification { ok: true });
    }
    if provided_key.is_empty() || provided_key == "anonymous" {
        return Ok(ApiKeyVerification { ok: false });
    }

    let prefix: String = provided_key.chars().take(16).collect();
    let hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(provided_key.as_bytes());
        hex::encode(hasher.finalize())
    };

    let id: Option<i32> = sqlx::query_scalar(
        "SELECT id FROM api_keys WHERE key_prefix=$1 AND key_hash=$2 AND org_id=$3 AND agent_id=$4 AND revoked_at IS NULL",
    )
    .bind(&prefix)
    .bind(&hash)
    .bind(org_id)
    .bind(agent_id)
    .fetch_optional(pg)
    .await?;

    let Some(id) = id else {
        return Ok(ApiKeyVerification { ok: false });
    };
    let _ = sqlx::query("UPDATE api_keys SET last_used_at = NOW() WHERE id=$1")
        .bind(id)
        .execute(pg)
        .await;
    Ok(ApiKeyVerification { ok: true })
}

pub async fn get_org_owner_plan(pg: &PgPool, org_id: &str) -> Result<String, sqlx::Error> {
    let plan: Option<String> = sqlx::query_scalar(
        "SELECT plan FROM users WHERE org_id = $1
         UNION SELECT u.plan FROM users u JOIN api_keys a ON a.user_id = u.id WHERE a.org_id = $1
         UNION SELECT u.plan FROM users u JOIN custom_actions c ON c.user_id = u.id WHERE c.org_id = $1
         UNION SELECT u.plan FROM users u JOIN service_credentials s ON s.user_id = u.id WHERE s.org_id = $1
         LIMIT 1",
    )
    .bind(org_id)
    .fetch_optional(pg)
    .await?;
    Ok(plan.unwrap_or_else(|| "free".to_string()))
}

pub async fn get_effective_rate_limit(state: &SharedState, org_id: &str) -> Result<u32, sqlx::Error> {
    let plan = get_org_owner_plan(&state.pg, org_id).await?;
    Ok(if plan == "agency" {
        state.agency_rate_limit_per_min
    } else {
        state.agent_rate_limit_per_min
    })
}

pub async fn get_effective_limit(state: &SharedState, org_id: &str) -> Result<i64, sqlx::Error> {
    let override_limit: Option<i32> =
        sqlx::query_scalar("SELECT monthly_limit FROM org_limit_overrides WHERE org_id = $1")
            .bind(org_id)
            .fetch_optional(&state.pg)
            .await?;
    if let Some(limit) = override_limit {
        return Ok(limit as i64);
    }
    let plan = get_org_owner_plan(&state.pg, org_id).await?;
    Ok(if plan == "agency" {
        state.agency_monthly_limit
    } else {
        state.cloud_monthly_limit
    })
}

pub struct UsageCheck {
    pub ok: bool,
    pub count: i64,
    pub limit: i64,
}

pub async fn check_usage_limit(state: &SharedState, org_id: &str) -> Result<UsageCheck, sqlx::Error> {
    if state.deployment_mode != "cloud" {
        return Ok(UsageCheck { ok: true, count: 0, limit: 0 });
    }

    let owner_exempt: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM users u WHERE (u.is_admin = true OR u.id BETWEEN 1 AND 9) AND (
           u.org_id = $1 OR u.id IN (
             SELECT user_id FROM api_keys WHERE org_id = $1
             UNION SELECT user_id FROM custom_actions WHERE org_id = $1
             UNION SELECT user_id FROM service_credentials WHERE org_id = $1
           )
         ) LIMIT 1",
    )
    .bind(org_id)
    .fetch_optional(&state.pg)
    .await?;
    if owner_exempt.is_some() {
        return Ok(UsageCheck { ok: true, count: 0, limit: 0 });
    }

    let limit = get_effective_limit(state, org_id).await?;
    let count = get_monthly_usage(state, org_id).await.unwrap_or(0);
    Ok(UsageCheck {
        ok: count < limit,
        count,
        limit,
    })
}

pub fn current_month_key() -> String {
    let now = chrono::Utc::now();
    format!("{}-{:02}", now.format("%Y"), now.format("%m"))
}

pub async fn increment_monthly_usage(state: &SharedState, org_id: &str) -> redis::RedisResult<i64> {
    let mut conn = state.redis.get_multiplexed_async_connection().await?;
    let key = format!("usage:{}:{}", org_id, current_month_key());
    let count: i64 = redis::cmd("INCR").arg(&key).query_async(&mut conn).await?;
    if count == 1 {
        let _: () = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(60 * 60 * 24 * 40)
            .query_async(&mut conn)
            .await?;
    }
    let _: Result<(), _> = redis::cmd("PUBLISH")
        .arg("usage:updates")
        .arg(serde_json::json!({ "org_id": org_id, "total": count }).to_string())
        .query_async(&mut conn)
        .await;
    Ok(count)
}

pub async fn get_monthly_usage(state: &SharedState, org_id: &str) -> redis::RedisResult<i64> {
    let mut conn = state.redis.get_multiplexed_async_connection().await?;
    let key = format!("usage:{}:{}", org_id, current_month_key());
    let val: Option<String> = redis::cmd("GET").arg(&key).query_async(&mut conn).await?;
    Ok(val.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Every org a user owns or belongs to, via any of the ways that gets
/// established. Scoped subset needed by Phase 2's agent-connect route
/// (tenant cap check); Phase 3/4 dashboard routes will need the same
/// query and can reuse this.
pub async fn get_user_org_ids(pg: &PgPool, user_id: i32) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT org_id FROM users WHERE id = $1 AND org_id IS NOT NULL
         UNION SELECT DISTINCT org_id FROM api_keys WHERE user_id = $1
         UNION SELECT DISTINCT org_id FROM custom_actions WHERE user_id = $1
         UNION SELECT DISTINCT org_id FROM service_credentials WHERE user_id = $1
         UNION SELECT DISTINCT org_id FROM custom_validation_rules WHERE created_by = $1
         UNION SELECT DISTINCT org_id FROM custom_dedup_rules WHERE created_by = $1
         UNION SELECT DISTINCT org_id FROM org_members WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pg)
    .await?;
    Ok(rows)
}

pub struct TenantCapCheck {
    pub ok: bool,
    pub limit: i64,
}

pub async fn check_agency_tenant_cap(
    state: &SharedState,
    user_id: i32,
    org_id: &str,
) -> Result<TenantCapCheck, sqlx::Error> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT plan, org_id FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.pg)
            .await?;
    let Some((plan, own_org_id)) = row else {
        return Ok(TenantCapCheck { ok: true, limit: 0 });
    };
    if plan != "agency" {
        return Ok(TenantCapCheck { ok: true, limit: 0 });
    }
    if own_org_id.as_deref() == Some(org_id) {
        return Ok(TenantCapCheck { ok: true, limit: 0 });
    }
    let client_tenant_ids: Vec<String> = get_user_org_ids(&state.pg, user_id)
        .await?
        .into_iter()
        .filter(|id| Some(id.as_str()) != own_org_id.as_deref())
        .collect();
    if client_tenant_ids.iter().any(|id| id == org_id) {
        return Ok(TenantCapCheck { ok: true, limit: 0 });
    }
    if client_tenant_ids.len() as i64 >= state.agency_max_client_tenants {
        return Ok(TenantCapCheck {
            ok: false,
            limit: state.agency_max_client_tenants,
        });
    }
    Ok(TenantCapCheck { ok: true, limit: 0 })
}

/// Write gate for org-scoped dashboard/agent-connect routes. An
/// `org_members` row (Enterprise RBAC, via an accepted invite) decides it
/// when present — 'auditor' is read-only, anything else can write. With
/// no membership row, only the org's own registered owner
/// (`users.org_id = org_id`, set at registration — see `auth/routes.rs`'s
/// register(), which already rejects claiming someone else's org_id) may
/// write to it. A user with neither relationship to `org_id` has no
/// business writing into it at all.
pub async fn check_org_write_permission(pg: &PgPool, user_id: i32, org_id: &str) -> Result<bool, sqlx::Error> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE user_id = $1 AND org_id = $2")
        .bind(user_id)
        .bind(org_id)
        .fetch_optional(pg)
        .await?;
    if let Some(role) = role {
        return Ok(role != "auditor");
    }
    let is_owner: Option<i32> = sqlx::query_scalar("SELECT 1 FROM users WHERE id = $1 AND org_id = $2")
        .bind(user_id)
        .bind(org_id)
        .fetch_optional(pg)
        .await?;
    Ok(is_owner.is_some())
}

/// Tier resolution — cloud reads `users.plan` for the org's owning user
/// directly (same "owner" model as `check_org_write_permission` above).
/// Self-host has no billing DB to query, so it reads the cached,
/// already-verified license tier instead (`state.license_tier`, kept
/// current by a background task started in `main.rs` — see
/// `agentraas_core::license`). Never errors: an org with no matching
/// row, or a stale/unrecognized plan string, resolves to
/// `Tier::Community` rather than blocking the caller, and so does a
/// self-host deployment with no (or an invalid/expired) license.
///
/// Not called from the Community binary yet — its callers today are all
/// inside `ee/`-gated code (still Cargo-feature-gated exactly as before;
/// only the *runtime* check moved from a single on/off switch to a
/// graduated tier). Phase 7's dashboard UI (showing an org's own tier)
/// will be its first Community-reachable caller.
#[allow(dead_code)]
pub async fn effective_tier(state: &SharedState, org_id: &str) -> agentraas_core::tier::Tier {
    if state.deployment_mode != "cloud" {
        return *state.license_tier.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM users WHERE org_id = $1")
        .bind(org_id)
        .fetch_optional(&state.pg)
        .await
        .ok()
        .flatten();
    plan.as_deref().map(agentraas_core::tier::Tier::from_plan_str).unwrap_or(agentraas_core::tier::Tier::Community)
}

/// Runtime replacement for `require_enterprise_mode` on features that
/// moved from pure Enterprise gating to a graduated tier (HITL → Pro+,
/// Inbound Webhooks → Agency+). These stay exactly as `ee/`-gated as
/// before — Community self-hosters (the public repo) never compile them
/// in either way, so nothing here needs to be reachable from the
/// Community binary. What changed is only the runtime check *within*
/// that already-gated code: the whole server's single `enterprise_mode`
/// switch can't express "Pro can, Community can't," so this checks the
/// calling org's actual tier instead (cloud: from the billing DB;
/// self-host: from the cached license). No license, or an invalid/
/// expired one, correctly gets 403 here — never a silent bypass.
#[allow(dead_code)]
pub async fn require_tier(state: &SharedState, org_id: &str, minimum: agentraas_core::tier::Tier) -> Result<(), ApiError> {
    if effective_tier(state, org_id).await < minimum {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("This feature requires the {minimum:?} plan or higher. Upgrade from the dashboard's Billing panel."),
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct ResolvedRoute {
    pub method: String,
    pub url: String,
    pub internal: bool,
    pub auth_type: String,
    pub auth_header: Option<String>,
    pub content_type: String,
    pub extra_headers: Option<Value>,
    pub fanout_urls: Vec<String>,
    pub credential_key: String,
    /// See `RawActionConfig::streaming` in `agentraas_core::config`. Custom
    /// actions have no config schema for this yet, so this is always
    /// `false` for them.
    pub streaming: bool,
}

/// Looks up a registered custom action, shaped like a `SERVICE_ROUTES`
/// entry — Dynamic Header & Secret Injection: any `secret: true` header is
/// decrypted now, so the forwarder's plain header merge just works, same
/// as a curated service's static config-driven `extraHeaders`.
pub async fn resolve_custom_route(
    state: &SharedState,
    org_id: &str,
    action_name: &str,
) -> Result<Option<ResolvedRoute>, sqlx::Error> {
    let pg = &state.pg;
    #[derive(sqlx::FromRow)]
    struct Row {
        method: String,
        target_url: String,
        auth_type: String,
        auth_header_name: Option<String>,
        content_type: String,
        extra_headers: Value,
        fanout_urls: Value,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT method, target_url, auth_type, auth_header_name, content_type, extra_headers, fanout_urls
         FROM custom_actions WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL LIMIT 1",
    )
    .bind(org_id)
    .bind(action_name)
    .fetch_optional(pg)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let mut extra_headers = serde_json::Map::new();
    if let Some(arr) = row.extra_headers.as_array() {
        for h in arr {
            let name = h.get("name").and_then(Value::as_str).unwrap_or_default();
            let is_secret = h.get("secret").and_then(Value::as_bool).unwrap_or(false);
            let raw_value = h.get("value").and_then(Value::as_str).unwrap_or_default();
            let value = if is_secret {
                match state.cipher.decrypt(raw_value) {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::error!(?err, org_id, action_name, header = name, "failed to decrypt custom-action secret header");
                        String::new()
                    }
                }
            } else {
                raw_value.to_string()
            };
            extra_headers.insert(name.to_string(), Value::String(value));
        }
    }

    let (auth_type, auth_header) = if row.auth_type == "header" {
        ("custom-header".to_string(), row.auth_header_name)
    } else if row.auth_type == "bearer" {
        ("bearer".to_string(), Some("Authorization".to_string()))
    } else {
        (row.auth_type, None)
    };

    Ok(Some(ResolvedRoute {
        method: row.method,
        url: row.target_url,
        internal: false,
        auth_type,
        auth_header,
        content_type: row.content_type,
        extra_headers: Some(Value::Object(extra_headers)),
        fanout_urls: row
            .fanout_urls
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        credential_key: format!("custom:{action_name}"),
        streaming: false,
    }))
}

/// A registered third-party MCP server's tool — distinct from `ResolvedRoute`
/// because the request/response shape is JSON-RPC (`forward::
/// forward_mcp_tool_call`), not the REST shape `ResolvedRoute` implies.
pub struct ResolvedMcpRoute {
    pub target_url: String,
    pub auth_type: String,
    pub auth_header: Option<String>,
    pub credential_key: String,
    pub remote_tool_name: String,
}

/// Splits a local tool name shaped `<server_name>.<remote_tool_name>` and
/// looks up the matching registered `custom_mcp_servers` row — the MCP
/// Custom Actions counterpart to `resolve_custom_route` above. Returns
/// `Ok(None)` (not an error) for anything that isn't `name.tool` shaped or
/// doesn't match a registered server, so callers can just fall through to
/// "tool not found" the same way they already do for curated/HTTP-custom
/// misses.
pub async fn resolve_mcp_tool_route(pg: &sqlx::PgPool, org_id: &str, tool_name: &str) -> Result<Option<ResolvedMcpRoute>, sqlx::Error> {
    let Some((server_name, remote_tool_name)) = tool_name.split_once('.') else {
        return Ok(None);
    };
    #[derive(sqlx::FromRow)]
    struct Row {
        target_url: String,
        auth_type: String,
        auth_header_name: Option<String>,
    }
    let row = sqlx::query_as::<_, Row>(
        "SELECT target_url, auth_type, auth_header_name FROM custom_mcp_servers WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL LIMIT 1",
    )
    .bind(org_id)
    .bind(server_name)
    .fetch_optional(pg)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let (auth_type, auth_header) = if row.auth_type == "header" {
        ("custom-header".to_string(), row.auth_header_name)
    } else if row.auth_type == "bearer" {
        ("bearer".to_string(), Some("Authorization".to_string()))
    } else {
        (row.auth_type, None)
    };

    Ok(Some(ResolvedMcpRoute {
        target_url: row.target_url,
        auth_type,
        auth_header,
        credential_key: format!("mcp:{server_name}"),
        remote_tool_name: remote_tool_name.to_string(),
    }))
}

/// Resolves `service`+`action` to a `ResolvedRoute`, exactly like
/// `handle_request`'s own routing branch — shared with the Dead Letter
/// Queue replay path, which needs the identical lookup outside the normal
/// dedup/validation/circuit-breaker pipeline.
pub async fn resolve_route(
    state: &SharedState,
    service: &str,
    action: &str,
    org_id: &str,
) -> Result<Option<ResolvedRoute>, sqlx::Error> {
    if service == "custom" {
        return resolve_custom_route(state, org_id, action).await;
    }
    let route_key = format!("{service}.{action}");
    Ok(state.service_routes.get(&route_key).map(|r| ResolvedRoute {
        method: r.method.clone(),
        url: r.url.clone(),
        internal: r.internal,
        auth_type: r.auth_type.clone(),
        auth_header: r.auth_header.clone(),
        content_type: r.content_type.clone(),
        extra_headers: r.extra_headers.clone(),
        fanout_urls: Vec::new(),
        credential_key: service.to_string(),
        streaming: r.streaming,
    }))
}

/// DLP redaction (`src/ee/dlp` equivalent) is Enterprise-only; the
/// Community edition never compiles `agentraas_core::dlp` in at all, so
/// this always returns `None` there — same end result as Node's
/// `ENTERPRISE_MODE && rawPayload` check when `src/ee/dlp` isn't present.
#[cfg(feature = "enterprise")]
fn redact_preview(enterprise_mode: bool, raw_payload: Option<&Value>) -> Option<String> {
    if enterprise_mode {
        raw_payload.map(|p| agentraas_core::dlp::redact_pii(p).to_string())
    } else {
        None
    }
}
#[cfg(not(feature = "enterprise"))]
fn redact_preview(_enterprise_mode: bool, _raw_payload: Option<&Value>) -> Option<String> {
    None
}

/// `enterprise_mode`+`raw_payload` mirror Node's `logAudit`'s optional
/// trailing `rawPayload` param: only call sites that explicitly pass a
/// payload (and only when Enterprise DLP is on) get a redacted preview
/// stored — every other call site behaves exactly as before this column
/// existed.
#[allow(clippy::too_many_arguments)]
pub async fn log_audit(
    pg: &PgPool,
    req_id: &str,
    api_key: &str,
    org_id: &str,
    agent_id: &str,
    service: &str,
    action: &str,
    status: &str,
    error_type: Option<&str>,
    duration_ms: i64,
    payload_hash: Option<&str>,
    enterprise_mode: bool,
    raw_payload: Option<&Value>,
    run_id: Option<&str>,
    step_id: Option<&str>,
) {
    let masked_key = mask_api_key_for_audit(api_key);
    let redacted_preview = redact_preview(enterprise_mode, raw_payload);
    if let Err(err) = sqlx::query(
        "INSERT INTO audit_log (req_id,api_key,org_id,agent_id,service,action,status,error_type,duration_ms,payload_hash,redacted_payload_preview,run_id,step_id,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,NOW())",
    )
    .bind(req_id)
    .bind(&masked_key)
    .bind(org_id)
    .bind(agent_id)
    .bind(service)
    .bind(action)
    .bind(status)
    .bind(error_type)
    .bind(duration_ms)
    .bind(payload_hash)
    .bind(redacted_preview)
    .bind(run_id)
    .bind(step_id)
    .execute(pg)
    .await
    {
        tracing::error!(?err, "audit log failed");
    }
}

/// Only for genuine upstream failures (the target API itself returned an
/// error) — never for client-side rejections (validation, usage limit, an
/// already-open circuit) that a blind replay wouldn't fix. Best-effort:
/// never let a DLQ write failure change the response the caller already
/// got, matching Node's `.catch(...)`.
#[allow(clippy::too_many_arguments)]
pub async fn write_dead_letter_queue(
    state: &SharedState,
    req_id: &str,
    org_id: &str,
    agent_id: &str,
    service: &str,
    action: &str,
    payload: &Value,
    error_message: &str,
) {
    let encrypted_payload = state.cipher.encrypt(&payload.to_string());
    if let Err(err) = sqlx::query(
        "INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(req_id)
    .bind(org_id)
    .bind(agent_id)
    .bind(service)
    .bind(action)
    .bind(&encrypted_payload)
    .bind(error_message)
    .execute(&state.pg)
    .await
    {
        tracing::warn!(?err, req_id, "dead-letter queue write failed");
    }
}

fn mask_api_key_for_audit(api_key: &str) -> String {
    if api_key.is_empty() || api_key == "anonymous" {
        return "anonymous".to_string();
    }
    if api_key.len() > 8 {
        format!("{}…", &api_key[..8])
    } else {
        "••••".to_string()
    }
}

/// Upstream services report errors in different shapes.
pub fn extract_upstream_error_message(response_data: &Value) -> Option<String> {
    if let Some(s) = response_data.get("error").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    response_data
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(String::from)
}
