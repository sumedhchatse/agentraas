pub mod db;
pub mod forward;

use agentraas_core::{circuit_breaker, dedup};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

use db::{
    check_agency_tenant_cap, check_org_write_permission, check_usage_limit,
    get_effective_dedup_rule, get_effective_rate_limit, get_effective_validation_rule,
    increment_monthly_usage, log_audit, resolve_custom_route, verify_api_key, ResolvedRoute,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/v1/webhook/:org_id/:agent_id", post(webhook_handler))
        .route("/v1/sdk/:service/:action", post(sdk_handler))
        .route("/api/v1/agents/connect", post(connect_agent))
        .route("/api/v1/agents/keys", get(list_keys))
        .route("/api/v1/agents/keys/:id", delete(revoke_key))
        .route("/api/v1/agents/keys/:id/regenerate", post(regenerate_key))
        .route("/internal/mockpay", post(internal_mockpay))
}

/// Internal mock payment processor — `config/services.json`'s `mockpay`
/// entry points at `http://localhost:3000/internal/mockpay`, which resolves
/// to whichever server is handling the request (each container has its own
/// network namespace), so this route has to exist here too, byte-identical
/// to Node's, not just in server.js.
async fn internal_mockpay(Json(body): Json<Value>) -> Response {
    let amount = body.get("amount").cloned();
    let fail = body.get("fail").and_then(Value::as_bool);
    // fail:true -> always fails. fail:false -> never fails (deterministic,
    // used by automated tests). fail omitted -> ~10% random failure.
    let should_fail = match fail {
        Some(true) => true,
        Some(false) => false,
        None => rand::random::<f64>() < 0.1,
    };
    if should_fail {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "MockPay temporarily unavailable", "code": "mock_error" })),
        );
    }
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    (
        StatusCode::OK,
        Json(json!({
            "id": format!("mockpay_{}", hex::encode(buf)),
            // Pass the caller's amount through untouched (same type/shape
            // it arrived as), matching Node's `amount||0` — no float
            // coercion, so `100` stays `100`, not `100.0`.
            "amount": amount.unwrap_or(json!(0)),
            "status": "completed",
            "processor": "MockPay",
            "timestamp": crate::util::iso_now(),
        })),
    )
}

fn generate_request_id() -> String {
    let mut buf = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut buf);
    format!("req_{}", hex::encode(buf))
}

// ─── shared handle_request ───

enum Source {
    Webhook,
    Sdk,
}

struct RequestIdentity {
    org_id: String,
    agent_id: String,
    api_key: String,
    service: String,
    action: String,
    payload: Value,
    idempotency_key: Option<String>,
}

async fn webhook_handler(
    State(state): State<SharedState>,
    Path((org_id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let api_key = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("anonymous")
        .to_string();
    let idempotency_key = header_value(&headers, "x-agentraas-idempotency-key");

    let service = body.get("service").and_then(Value::as_str).unwrap_or_default().to_string();
    let action = body.get("action").and_then(Value::as_str).unwrap_or_default().to_string();
    let payload = body.get("payload").cloned().unwrap_or(json!({}));

    handle_request(
        &state,
        Source::Webhook,
        RequestIdentity {
            org_id,
            agent_id,
            api_key,
            service,
            action,
            payload,
            idempotency_key,
        },
    )
    .await
}

async fn sdk_handler(
    State(state): State<SharedState>,
    Path((service, action)): Path<(String, String)>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Response {
    let api_key = header_value(&headers, "x-agentraas-key").unwrap_or_else(|| "anonymous".to_string());
    let org_id = header_value(&headers, "x-agentraas-org").unwrap_or_else(|| "sdk".to_string());
    let agent_id = header_value(&headers, "x-agentraas-agent").unwrap_or_else(|| "sdk-agent".to_string());
    let idempotency_key = header_value(&headers, "x-agentraas-idempotency-key");

    handle_request(
        &state,
        Source::Sdk,
        RequestIdentity {
            org_id,
            agent_id,
            api_key,
            service,
            action,
            payload,
            idempotency_key,
        },
    )
    .await
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(String::from)
}

type Response = (StatusCode, Json<Value>);

fn err_response(status: StatusCode, req_id: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into(), "reqId": req_id })))
}

/// Replays a Pause & Buffer (Enterprise maintenance mode) queued webhook
/// item through the exact same `handle_request` pipeline a live request
/// goes through — dedup/validation/circuit-breaker/forward/audit behave
/// identically, including exactly-once (a buffered request whose dedup
/// hash was somehow already completed is just a no-op cache hit here).
#[cfg(feature = "enterprise")]
pub async fn replay_webhook(
    state: &SharedState,
    org_id: String,
    agent_id: String,
    api_key: String,
    service: String,
    action: String,
    payload: Value,
) -> Response {
    handle_request(
        state,
        Source::Webhook,
        RequestIdentity { org_id, agent_id, api_key, service, action, payload, idempotency_key: None },
    )
    .await
}

async fn handle_request(
    state: &SharedState,
    #[cfg_attr(not(feature = "enterprise"), allow(unused_variables))] source: Source,
    identity: RequestIdentity,
) -> Response {
    let req_id = generate_request_id();
    let RequestIdentity {
        org_id,
        agent_id,
        api_key,
        service,
        action,
        payload,
        idempotency_key,
    } = identity;

    if service.is_empty() || action.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, &req_id, "Missing service or action");
    }
    let route_key = format!("{service}.{action}");

    let resolved_route = if service == "custom" {
        match resolve_custom_route(state, &org_id, &action).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return err_response(
                    StatusCode::BAD_REQUEST,
                    &req_id,
                    format!("No custom action named \"{action}\" registered for this org. Register it from the dashboard's Custom Actions panel."),
                )
            }
            Err(err) => {
                tracing::error!(?err, "resolve_custom_route failed");
                return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
            }
        }
    } else {
        match state.service_routes.get(&route_key) {
            Some(r) => ResolvedRoute {
                method: r.method.clone(),
                url: r.url.clone(),
                internal: r.internal,
                auth_type: r.auth_type.clone(),
                auth_header: r.auth_header.clone(),
                content_type: r.content_type.clone(),
                extra_headers: r.extra_headers.clone(),
                fanout_urls: Vec::new(),
                credential_key: service.clone(),
            },
            None => {
                return err_response(StatusCode::BAD_REQUEST, &req_id, format!("Unknown service.action: {route_key}"))
            }
        }
    };

    match verify_api_key(&state.pg, &api_key, &org_id, &agent_id).await {
        Ok(v) if !v.ok => {
            return err_response(
                StatusCode::UNAUTHORIZED,
                &req_id,
                "Invalid or missing API key for this agent. Generate one from the dashboard's Connect Agent panel.",
            )
        }
        Err(err) => {
            tracing::error!(?err, "verify_api_key failed");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
        }
        _ => {}
    }
    // Pause & Buffer (Enterprise) — while maintenance mode is on, incoming
    // webhooks are queued instead of forwarded, so upstream callers see a
    // clean 202 instead of failures during a known maintenance window.
    // SDK/MCP traffic isn't buffered (an agent is waiting synchronously for
    // a result) — this only applies to the fire-and-forget webhook path.
    #[cfg(feature = "enterprise")]
    if matches!(source, Source::Webhook) && state.enterprise_mode {
        match crate::ee::maintenance::is_paused(state).await {
            Ok(true) => {
                crate::ee::maintenance::enqueue(state, &org_id, &agent_id, &api_key, &service, &action, &payload).await;
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "buffered": true,
                        "reqId": req_id,
                        "message": "AgentRaaS is in maintenance mode — this request has been queued and will be processed automatically once maintenance ends.",
                    })),
                );
            }
            Ok(false) => {}
            Err(err) => {
                tracing::error!(?err, "maintenance-queue paused check failed");
            }
        }
    }

    let rate_limit_identity = if api_key != "anonymous" {
        api_key.clone()
    } else {
        format!("{org_id}:{agent_id}")
    };
    let effective_limit = match get_effective_rate_limit(state, &org_id).await {
        Ok(l) => l,
        Err(err) => {
            tracing::error!(?err, "get_effective_rate_limit failed");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
        }
    };
    let within_limit = {
        let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await else {
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
        };
        let bucket_key = format!("ratelimit:agent:{rate_limit_identity}");
        state
            .token_bucket
            .try_consume(&mut conn, &bucket_key, effective_limit as f64, effective_limit as f64 / 60.0, 1.0)
            .await
            .map(|r| r.allowed)
            .unwrap_or(true)
    };
    if !within_limit {
        return err_response(
            StatusCode::TOO_MANY_REQUESTS,
            &req_id,
            "Rate limit exceeded for this agent. Slow down and try again shortly.",
        );
    }

    let start = std::time::Instant::now();
    let payload_digest = dedup::hash_only(&payload);

    let dedup_field_rule = if idempotency_key.is_some() {
        None
    } else {
        get_effective_dedup_rule(&state.pg, &org_id, &service, &action).await.unwrap_or(None)
    };
    let dedup_hash = if let Some(idem) = &idempotency_key {
        dedup::hash_idempotency_key(&api_key, &service, &action, idem)
    } else if let Some(rule) = &dedup_field_rule {
        dedup::hash_field_values(&api_key, &service, &action, &payload, &rule.fields)
    } else {
        dedup::hash_payload(&api_key, &service, &action, &payload)
    };

    let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await else {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
    };
    let claim = match dedup::claim_dedup_slot(&mut conn, &dedup_hash).await {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(?err, "claim_dedup_slot failed");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
        }
    };

    if !claim.claimed {
        let existing = dedup::read_dedup_slot(&mut conn, &claim.key).await.ok().flatten();
        let is_pending = existing
            .as_ref()
            .and_then(|v| v.get("pending"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(existing) = existing else {
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
            return err_response(StatusCode::CONFLICT, &req_id, "An identical request is already being processed. Retry shortly.");
        };
        if is_pending {
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
            return err_response(StatusCode::CONFLICT, &req_id, "An identical request is already being processed. Retry shortly.");
        }

        if let Some(idem) = &idempotency_key {
            let existing_digest = existing.get("__payloadDigest").and_then(Value::as_str);
            if let Some(existing_digest) = existing_digest {
                if existing_digest != payload_digest {
                    let _ = idem;
                    log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("idempotency_key_reused"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
                    return err_response(StatusCode::UNPROCESSABLE_ENTITY, &req_id, "This Idempotency-Key was already used with a different payload. Use a new key for a different request.");
                }
            }
        }

        log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "deduplicated", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
        let mut cached = existing;
        if let Value::Object(ref mut map) = cached {
            map.remove("__payloadDigest");
            map.insert("cached".to_string(), Value::Bool(true));
            map.insert("reqId".to_string(), Value::String(req_id.clone()));
        }
        return (StatusCode::OK, Json(cached));
    }

    // ─── claimed: do the real work ───

    if let Ok(Some(rule)) = get_effective_validation_rule(state, &org_id, &service, &action).await {
        if let Some(validation_error) = agentraas_core::validator::validate_fields(&payload, &rule.fields) {
            let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("validation_failed"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
            return err_response(StatusCode::UNPROCESSABLE_ENTITY, &req_id, validation_error);
        }
    }

    let circuit_key = if resolved_route.credential_key.is_empty() {
        service.clone()
    } else {
        resolved_route.credential_key.clone()
    };
    match circuit_breaker::get_circuit_state(&mut conn, &circuit_key).await {
        Ok((state_str, transition)) => {
            if let Some(t) = transition {
                forward::log_circuit_transition(state, t).await;
            }
            if state_str == "open" {
                let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
                log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("circuit_open"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
                {
                    let state = state.clone();
                    let org_id = org_id.clone();
                    let service = service.clone();
                    tokio::spawn(async move {
                        crate::notifications::notify_circuit_open(&state, &org_id, &service).await;
                    });
                }
                return err_response(StatusCode::SERVICE_UNAVAILABLE, &req_id, format!("Circuit breaker open for {service}. Try again later."));
            }
        }
        Err(err) => {
            tracing::error!(?err, "get_circuit_state failed");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
        }
    }

    let usage = match check_usage_limit(state, &org_id).await {
        Ok(u) => u,
        Err(err) => {
            tracing::error!(?err, "check_usage_limit failed");
            return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
        }
    };
    if !usage.ok {
        let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
        log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("usage_limit_exceeded"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
        return err_response(
            StatusCode::PAYMENT_REQUIRED,
            &req_id,
            format!("Monthly usage limit reached ({}/{} actions this month). Contact support@agentraas.io to upgrade.", usage.count, usage.limit),
        );
    }

    match forward::forward_with_retry(state, &resolved_route, &service, &action, &org_id, &payload, &req_id, &circuit_key).await {
        Ok(mut result) => {
            if let Ok(mut c2) = state.redis.get_multiplexed_async_connection().await {
                if let Ok(Some(t)) = circuit_breaker::record_success(&mut c2, &circuit_key).await {
                    forward::log_circuit_transition(state, t).await;
                }
            }
            forward::broadcast_fanout(state, &resolved_route, &payload, &req_id);

            let mut stored = result.clone();
            if let (Some(idem), Value::Object(ref mut map)) = (&idempotency_key, &mut stored) {
                let _ = idem;
                map.insert("__payloadDigest".to_string(), Value::String(payload_digest.clone()));
            }
            let _ = dedup::complete_dedup_slot(&mut conn, &claim.key, &stored).await;
            let _ = increment_monthly_usage(state, &org_id).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "success", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), state.enterprise_mode, Some(&payload)).await;

            if let Value::Object(ref mut map) = result {
                map.insert("reqId".to_string(), Value::String(req_id.clone()));
            }
            (StatusCode::OK, Json(result))
        }
        Err(err) => {
            let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
            if !err.circuit_already_recorded {
                if let Ok(mut c2) = state.redis.get_multiplexed_async_connection().await {
                    if let Ok(Some(t)) = circuit_breaker::record_failure(&mut c2, &circuit_key).await {
                        forward::log_circuit_transition(state, t).await;
                    }
                }
            }
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "error", Some(&err.message), start.elapsed().as_millis() as i64, None, false, None).await;
            tracing::error!(req_id, error = %err.message, "request failed");

            let response_message = if err.upstream_status.is_some() {
                err.message.clone()
            } else {
                "An internal error occurred while processing this request.".to_string()
            };
            if err.upstream_status.is_some() {
                db::write_dead_letter_queue(state, &req_id, &org_id, &agent_id, &service, &action, &payload, &err.message).await;
            }
            let status = err
                .upstream_status
                .and_then(|s| StatusCode::from_u16(s).ok())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (
                status,
                Json(json!({ "error": response_message, "reqId": req_id, "agentraas_note": "Request blocked by AgentRaaS." })),
            )
        }
    }
}

// ─── agent key CRUD ───

#[derive(Deserialize)]
struct ConnectBody {
    org_id: Option<String>,
    agent_id: Option<String>,
    label: Option<String>,
}

fn generate_api_key() -> (String, String, String) {
    let mut buf = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut buf);
    let raw = format!("ar_live_{}", hex::encode(buf));
    let hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(raw.as_bytes());
        hex::encode(hasher.finalize())
    };
    let prefix: String = raw.chars().take(16).collect();
    (raw, hash, prefix)
}

async fn connect_agent(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<ConnectBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let org_id = body.org_id.unwrap_or_default();
    let agent_id = body.agent_id.unwrap_or_default();
    if !is_valid_identifier(&org_id) || !is_valid_identifier(&agent_id) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only.",
        ));
    }
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let tenant_cap = check_agency_tenant_cap(&state, user.sub, &org_id).await?;
    if !tenant_cap.ok {
        return Err(ApiError::new(
            StatusCode::PAYMENT_REQUIRED,
            format!("Agency plan is limited to {} client tenants. Contact support@agentraas.io to increase this.", tenant_cap.limit),
        ));
    }

    let (raw_key, key_hash, key_prefix) = generate_api_key();
    let label = body.label.unwrap_or_default().chars().take(255).collect::<String>();
    let label = if label.is_empty() { None } else { Some(label) };

    sqlx::query("INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(user.sub)
        .bind(&org_id)
        .bind(&agent_id)
        .bind(&label)
        .bind(&key_hash)
        .bind(&key_prefix)
        .execute(&state.pg)
        .await?;

    Ok(Json(json!({
        "api_key": raw_key,
        "webhook_url": format!("{}/v1/webhook/{}/{}", state.public_url, org_id, agent_id),
        "mcp_url": format!("{}/mcp", state.public_url),
        "org_id": org_id,
        "agent_id": agent_id,
    })))
}

async fn list_keys(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        agent_id: String,
        label: Option<String>,
        key_prefix: String,
        created_at: chrono::DateTime<chrono::Utc>,
        last_used_at: Option<chrono::DateTime<chrono::Utc>>,
        revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, agent_id, label, key_prefix, created_at, last_used_at, revoked_at
         FROM api_keys WHERE user_id = $1 ORDER BY created_at DESC",
    )
    .bind(user.sub)
    .fetch_all(&state.pg)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| {
                json!({
                    "id": r.id, "org_id": r.org_id, "agent_id": r.agent_id, "label": r.label,
                    "key_prefix": r.key_prefix, "created_at": r.created_at, "last_used_at": r.last_used_at,
                    "revoked_at": r.revoked_at,
                })
            })
            .collect(),
    ))
}

async fn revoke_key(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(id): Path<i32>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let updated: Option<i32> = sqlx::query_scalar(
        "UPDATE api_keys SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id",
    )
    .bind(id)
    .bind(user.sub)
    .fetch_optional(&state.pg)
    .await?;
    if updated.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Key not found."));
    }
    Ok(Json(json!({ "revoked": true })))
}

async fn regenerate_key(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(id): Path<i32>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let existing: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT org_id, agent_id, label FROM api_keys WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(user.sub)
    .fetch_optional(&state.pg)
    .await?;
    let Some((org_id, agent_id, label)) = existing else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Key not found."));
    };

    sqlx::query("UPDATE api_keys SET revoked_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(&state.pg)
        .await?;

    let (raw_key, key_hash, key_prefix) = generate_api_key();
    sqlx::query("INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(user.sub)
        .bind(&org_id)
        .bind(&agent_id)
        .bind(&label)
        .bind(&key_hash)
        .bind(&key_prefix)
        .execute(&state.pg)
        .await?;

    Ok(Json(json!({
        "api_key": raw_key,
        "webhook_url": format!("{}/v1/webhook/{}/{}", state.public_url, org_id, agent_id),
        "mcp_url": format!("{}/mcp", state.public_url),
        "org_id": org_id,
        "agent_id": agent_id,
    })))
}
