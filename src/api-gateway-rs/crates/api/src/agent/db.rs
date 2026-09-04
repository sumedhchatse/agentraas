//! DB-backed helpers `handle_request` depends on — mirrors the standalone
//! functions in `server.js` of the same name (getEffectiveValidationRule,
//! getEffectiveDedupRule, getCredential, verifyApiKey, getEffectiveRateLimit,
//! checkUsageLimit, incrementMonthlyUsage, getOrgOwnerPlan, getUserOrgIds,
//! checkAgencyTenantCap, checkOrgWritePermission, resolveCustomRoute,
//! logAudit).

use serde_json::Value;
use sqlx::PgPool;

use crate::state::SharedState;

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
}

/// No static fallback — every action defaults to whole-payload-hash dedup
/// unless an org explicitly configures a field-based rule.
pub async fn get_effective_dedup_rule(
    pg: &PgPool,
    org_id: &str,
    service: &str,
    action: &str,
) -> Result<Option<EffectiveDedupRule>, sqlx::Error> {
    let fields: Option<Value> = sqlx::query_scalar(
        "SELECT fields FROM custom_dedup_rules WHERE org_id = $1 AND service = $2 AND action = $3",
    )
    .bind(org_id)
    .bind(service)
    .bind(action)
    .fetch_optional(pg)
    .await?;
    Ok(fields.map(|f| EffectiveDedupRule {
        fields: f
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default(),
    }))
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
pub async fn verify_api_key(
    pg: &PgPool,
    provided_key: &str,
    org_id: &str,
    agent_id: &str,
) -> Result<ApiKeyVerification, sqlx::Error> {
    let keys_exist: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM api_keys WHERE org_id=$1 AND agent_id=$2 AND revoked_at IS NULL LIMIT 1",
    )
    .bind(org_id)
    .bind(agent_id)
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

/// Enterprise RBAC write gate: an 'auditor' `org_members` row means
/// read-only for that org. No membership row at all (Community-tier, the
/// overwhelming majority of orgs) is permissive, unchanged.
pub async fn check_org_write_permission(pg: &PgPool, user_id: i32, org_id: &str) -> Result<bool, sqlx::Error> {
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE user_id=$1 AND org_id=$2")
        .bind(user_id)
        .bind(org_id)
        .fetch_optional(pg)
        .await?;
    Ok(role.as_deref() != Some("auditor"))
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
) {
    let masked_key = mask_api_key_for_audit(api_key);
    let redacted_preview = redact_preview(enterprise_mode, raw_payload);
    if let Err(err) = sqlx::query(
        "INSERT INTO audit_log (req_id,api_key,org_id,agent_id,service,action,status,error_type,duration_ms,payload_hash,redacted_payload_preview,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,NOW())",
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
