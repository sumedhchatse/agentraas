pub mod db;
pub mod forward;

use agentraas_core::{circuit_breaker, dedup};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

use db::{
    check_agency_tenant_cap, check_org_write_permission, check_usage_limit,
    get_effective_dedup_rule, get_effective_rate_limit, get_effective_validation_rule,
    get_user_org_ids, increment_monthly_usage, log_audit, resolve_custom_route,
    resolve_org_from_api_key, select_dedup_hash_mode, verify_api_key, DedupHashMode, ResolvedRoute,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/v1/webhook/:org_id/:agent_id", post(webhook_handler))
        .route("/v1/sdk/:service/:action", post(sdk_handler))
        .route("/api/v1/agents/connect", post(connect_agent))
        .route("/api/v1/agents/keys", get(list_keys))
        .route("/api/v1/agents/keys/:id", delete(revoke_key))
        .route("/api/v1/agents/keys/:id/regenerate", post(regenerate_key))
        .route("/api/v1/runs/:run_id", get(get_run_status))
        .route("/api/v1/demo/live-test", post(demo_live_test))
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
        )
            .into();
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
        .into()
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
    /// Agent Run Budgeting & Loop Detection — optional, additive. A caller
    /// that never sends this sees no behavior change.
    run_id: Option<String>,
    /// State Checkpointing — a stable identifier for this specific step
    /// within `run_id`'s task. Only takes effect when both are present.
    step_id: Option<String>,
    /// On-Behalf-Of End-User Identity — optional, additive. When present,
    /// scopes credential lookup to this specific end-user (no fallback to
    /// a shared org-wide credential) and the dedup hash, so a caller who
    /// never sends this sees no behavior change at all.
    end_user_id: Option<String>,
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
    let run_id = header_value(&headers, "x-agentraas-run-id");
    let step_id = header_value(&headers, "x-agentraas-step-id");

    let service = body.get("service").and_then(Value::as_str).unwrap_or_default().to_string();
    let action = body.get("action").and_then(Value::as_str).unwrap_or_default().to_string();
    let payload = body.get("payload").cloned().unwrap_or(json!({}));
    let end_user_id = body.get("end_user_id").and_then(Value::as_str).map(String::from);

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
            run_id,
            step_id,
            end_user_id,
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
    let run_id = header_value(&headers, "x-agentraas-run-id");
    let step_id = header_value(&headers, "x-agentraas-step-id");
    let end_user_id = header_value(&headers, "x-agentraas-end-user");

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
            run_id,
            step_id,
            end_user_id,
        },
    )
    .await
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(String::from)
}

/// Almost always a plain `(StatusCode, Json<Value>)`; `Stream` is the one
/// exception, used only by `handle_request`'s streaming-route success arm to
/// pass an upstream SSE/chunked response through live instead of buffering
/// it — see `forward.rs::forward_action_streaming`. Mirrors `ApiError`'s
/// `IntoResponse` impl in `state.rs`.
pub(crate) enum Response {
    Json(StatusCode, Json<Value>),
    Stream(axum::response::Response),
}

impl axum::response::IntoResponse for Response {
    fn into_response(self) -> axum::response::Response {
        match self {
            Response::Json(status, json) => (status, json).into_response(),
            Response::Stream(resp) => resp,
        }
    }
}

impl From<(StatusCode, Json<Value>)> for Response {
    fn from((status, json): (StatusCode, Json<Value>)) -> Self {
        Response::Json(status, json)
    }
}

impl Response {
    /// For callers that only care whether the request succeeded, not the
    /// body — the Pause & Buffer maintenance queue's replay path (nothing
    /// is waiting on a streamed body there, so a streaming action getting
    /// replayed just forwards the call and discards the stream, same as it
    /// would discard a normal JSON body). Only called from `ee::maintenance`,
    /// which is enterprise-only — the Community build never calls this.
    #[cfg_attr(not(feature = "enterprise"), allow(dead_code))]
    pub(crate) fn status_code(&self) -> StatusCode {
        match self {
            Response::Json(status, _) => *status,
            Response::Stream(resp) => resp.status(),
        }
    }
}

fn err_response(status: StatusCode, req_id: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into(), "reqId": req_id }))).into()
}

/// Shared by `handle_request` and (once approved) the HITL Gateway's
/// resume path — both need to turn a service/action into the same
/// `ResolvedRoute` the exact same way.
pub(crate) async fn resolve_route(state: &SharedState, org_id: &str, service: &str, action: &str, req_id: &str) -> Result<ResolvedRoute, Response> {
    let route_key = format!("{service}.{action}");
    if service == "custom" {
        match resolve_custom_route(state, org_id, action).await {
            Ok(Some(r)) => Ok(r),
            Ok(None) => Err(err_response(
                StatusCode::BAD_REQUEST,
                req_id,
                format!("No custom action named \"{action}\" registered for this org. Register it from the dashboard's Custom Actions panel."),
            )),
            Err(err) => {
                tracing::error!(?err, "resolve_custom_route failed");
                Err(err_response(StatusCode::INTERNAL_SERVER_ERROR, req_id, "An internal error occurred."))
            }
        }
    } else {
        match state.service_routes.get(&route_key) {
            Some(r) => Ok(ResolvedRoute {
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
            }),
            None => Err(err_response(StatusCode::BAD_REQUEST, req_id, format!("Unknown service.action: {route_key}"))),
        }
    }
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
        RequestIdentity { org_id, agent_id, api_key, service, action, payload, idempotency_key: None, run_id: None, step_id: None, end_user_id: None },
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
        run_id,
        step_id,
        end_user_id,
    } = identity;

    if service.is_empty() || action.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, &req_id, "Missing service or action");
    }
    // org_id/agent_id land in audit_log and get rendered in the dashboard's
    // Active Agents panel — reject anything outside the same charset every
    // other org_id-accepting route in this app already enforces, rather
    // than letting arbitrary strings (HTML, oversized values) reach it.
    if !is_valid_identifier(&org_id) || !is_valid_identifier(&agent_id) {
        return err_response(StatusCode::BAD_REQUEST, &req_id, "org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only.");
    }
    let resolved_route = match resolve_route(state, &org_id, &service, &action, &req_id).await {
        Ok(r) => r,
        Err(resp) => return resp,
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
                )
                    .into();
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

    // State Checkpointing — only engages when the caller supplies BOTH a
    // run_id (the task) and a step_id (a stable identifier for this
    // specific step, caller-assigned — see checkpoint.rs for why a
    // derived call-count ordinal can't do this safely). If this exact
    // run_id+step_id pair already has a completed result, serve it
    // immediately — this is checked first, before loop-detection, since a
    // checkpoint hit is a known-already-done step, not a new attempt to
    // count toward the loop budget. Unlike payload-hash dedup, this
    // doesn't require the retried payload to match.
    if let (Some(run_id), Some(step_id)) = (&run_id, &step_id) {
        let checkpoint_key = agentraas_core::checkpoint::step_key(run_id, step_id);
        if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
            if let Ok(Some(mut cached)) = agentraas_core::checkpoint::read_checkpoint(&mut conn, &checkpoint_key).await {
                if let Value::Object(ref mut map) = cached {
                    map.insert("checkpointed".to_string(), Value::Bool(true));
                    map.insert("reqId".to_string(), Value::String(req_id.clone()));
                }
                return (StatusCode::OK, Json(cached)).into();
            }
        }
    }

    // Agent Run Budgeting & Loop Detection — only engages when the caller
    // supplies a run_id tagging a multi-step task. Every call to this
    // service.action within that run counts, including ones that end up
    // dedup-cached below: this is about the agent's own repeated-invocation
    // pattern, not how AgentRaaS happened to answer it. A checkpoint hit
    // above already returned, so this only runs for genuinely new attempts.
    if let Some(run_id) = &run_id {
        if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
            match agentraas_core::agent_run::record_call_and_check(
                &mut conn,
                &org_id,
                &agent_id,
                run_id,
                &service,
                &action,
                state.agent_loop_max_repeats,
                state.agent_run_ttl_seconds,
            )
            .await
            {
                Ok(check) if check.tripped => {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({
                            "error": "agent_circuit_open",
                            "message": format!(
                                "Tool execution halted: You have called {service}.{action} {count} times with no state change. Re-evaluate your strategy.",
                                count = check.count
                            ),
                            "reqId": req_id,
                        })),
                    )
                        .into();
                }
                Ok(_) => {}
                Err(err) => tracing::error!(?err, "agent loop-detection check failed"),
            }
        }
    }

    let start = std::time::Instant::now();
    let payload_digest = dedup::hash_only(&payload);

    // Looked up regardless of idempotency-key mode: a rule's `ttl_seconds`
    // (endpoint-specific dedup window) applies no matter which hash mode is
    // active — only the (non-empty) `fields` list is specific to field-based
    // hashing. This lets an org set a custom TTL without opting into a
    // field-allow-list rule at all (a "TTL-only" rule has empty `fields`).
    let dedup_field_rule = get_effective_dedup_rule(&state.pg, &org_id, &service, &action).await.unwrap_or(None);
    let dedup_hash = match select_dedup_hash_mode(idempotency_key.as_deref(), dedup_field_rule.as_ref()) {
        DedupHashMode::IdempotencyKey(idem) => dedup::hash_idempotency_key(&api_key, &service, &action, idem, end_user_id.as_deref()),
        DedupHashMode::Fields { fields, normalize } => {
            dedup::hash_field_values(&api_key, &service, &action, &payload, fields, normalize, end_user_id.as_deref())
        }
        DedupHashMode::Payload => dedup::hash_payload(&api_key, &service, &action, &payload, end_user_id.as_deref()),
    };
    let dedup_ttl_seconds = dedup_field_rule.as_ref().and_then(|r| r.ttl_seconds);

    let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await else {
        return err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred.");
    };
    let claim = match dedup::claim_dedup_slot_with_ttl(&mut conn, &dedup_hash, dedup_ttl_seconds).await {
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
        // A streaming route's completed slot holds a non-replayable
        // sentinel (see the success arm below), not the actual response
        // body — a stream can't be replayed byte-for-byte without buffering
        // it, which is exactly what streaming passthrough exists to avoid.
        // Reject rather than silently handing back the sentinel as if it
        // were a real result.
        let is_streamed = existing
            .as_ref()
            .and_then(|v| v.get("streamed"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(existing) = existing else {
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
            return err_response(StatusCode::CONFLICT, &req_id, "An identical request is already being processed. Retry shortly.");
        };
        if is_pending {
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
            return err_response(StatusCode::CONFLICT, &req_id, "An identical request is already being processed. Retry shortly.");
        }
        if is_streamed {
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("duplicate_of_streamed_response"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
            return err_response(StatusCode::CONFLICT, &req_id, "An identical request was already served as a streaming response and cannot be replayed. Wait for the dedup window to expire, or use a new Idempotency-Key.");
        }

        if let Some(idem) = &idempotency_key {
            let existing_digest = existing.get("__payloadDigest").and_then(Value::as_str);
            if let Some(existing_digest) = existing_digest {
                if existing_digest != payload_digest {
                    let _ = idem;
                    log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("idempotency_key_reused"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
                    return err_response(StatusCode::UNPROCESSABLE_ENTITY, &req_id, "This Idempotency-Key was already used with a different payload. Use a new key for a different request.");
                }
            }
        }

        log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "deduplicated", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
        let mut cached = existing;
        if let Value::Object(ref mut map) = cached {
            map.remove("__payloadDigest");
            map.insert("cached".to_string(), Value::Bool(true));
            map.insert("reqId".to_string(), Value::String(req_id.clone()));
        }
        return (StatusCode::OK, Json(cached)).into();
    }

    // ─── claimed: do the real work ───

    if let Ok(Some(rule)) = get_effective_validation_rule(state, &org_id, &service, &action).await {
        if let Some(validation_error) = agentraas_core::validator::validate_fields(&payload, &rule.fields) {
            let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("validation_failed"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
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
                log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("circuit_open"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
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
        log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "blocked", Some("usage_limit_exceeded"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;
        return err_response(
            StatusCode::PAYMENT_REQUIRED,
            &req_id,
            format!("Monthly usage limit reached ({}/{} actions this month). Contact hello@agentraas.io to upgrade.", usage.count, usage.limit),
        );
    }

    // Stateful Human-in-the-Loop (HITL) Gateway — the last gate before an
    // action that actually costs money/does something irreversible
    // fires. Webhook-only (an SDK/MCP caller is waiting synchronously and
    // has no notion of "come back later"), same scope limit as Pause &
    // Buffer above. The dedup slot claimed above is deliberately left
    // pending on freeze — see `ee::hitl` module doc.
    //
    // Pro+ (not "enterprise_mode on", which was the old server-wide
    // switch, independent of any org's actual plan) — checked here, not
    // just at rule-creation time, so a rule created while on Pro stops
    // firing the moment the org drops back to Community, without needing
    // to delete the rule itself.
    #[cfg(feature = "enterprise")]
    if matches!(source, Source::Webhook) && db::effective_tier(state, &org_id).await >= agentraas_core::tier::Tier::Pro {
        match crate::ee::hitl::match_rule(&state.pg, &org_id, &service, &action, &payload).await {
            Ok(Some(rule)) => {
                return crate::ee::hitl::freeze_and_notify(
                    state, &req_id, &org_id, &agent_id, &api_key, &service, &action, &payload, &dedup_hash, dedup_ttl_seconds, rule,
                    run_id.as_deref(), step_id.as_deref(),
                )
                .await
                .into();
            }
            Ok(None) => {}
            Err(err) => tracing::error!(?err, "hitl match_rule failed"),
        }
    }

    if resolved_route.streaming {
        return match forward::forward_with_retry_streaming(state, &resolved_route, &service, &org_id, &payload, &req_id, &circuit_key, end_user_id.as_deref()).await {
            Ok(streaming) => {
                if let Ok(mut c2) = state.redis.get_multiplexed_async_connection().await {
                    if let Ok(Some(t)) = circuit_breaker::record_success(&mut c2, &circuit_key).await {
                        forward::log_circuit_transition(state, t).await;
                    }
                }
                forward::broadcast_fanout(state, &resolved_route, &payload, &req_id);
                let _ = increment_monthly_usage(state, &org_id).await;
                // Fired now, at confirmed-2xx-headers time — not when the
                // stream finishes. This is what any reverse proxy does: a
                // client disconnect or upstream drop mid-stream is invisible
                // to it too. Checkpoint write is skipped for the same
                // reason `complete_dedup_slot_with_ttl` below stores a
                // sentinel, not the body: there's no full response to
                // persist for either.
                log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "success", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), state.enterprise_mode, Some(&payload), run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;

                // ponytail: no automated non-buffering test — proving bytes
                // are forwarded incrementally (not buffered) needs a test
                // double that trickles chunks over real wall-clock time,
                // which this repo has no precedent for. Verify with a
                // manual `curl -N` against a real/simulated chunked
                // upstream before shipping. Add a timed-chunk test double
                // if this regresses more than once.
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(16);
                let claim_key = claim.key.clone();
                let spawn_state = state.clone();
                let spawn_circuit_key = circuit_key.clone();
                let spawn_req_id = req_id.clone();
                let spawn_api_key = api_key.clone();
                let spawn_org_id = org_id.clone();
                let spawn_agent_id = agent_id.clone();
                let spawn_service = service.clone();
                let spawn_action = action.clone();
                let spawn_run_id = run_id.clone();
                let spawn_step_id = step_id.clone();
                let spawn_end_user_id = end_user_id.clone();
                let mut upstream = streaming.response;
                tokio::spawn(async move {
                    let mut had_error = false;
                    loop {
                        match upstream.chunk().await {
                            Ok(Some(chunk)) => {
                                if tx.send(Ok(chunk)).await.is_err() {
                                    break; // client disconnected
                                }
                            }
                            Ok(None) => break, // clean end of stream
                            Err(err) => {
                                had_error = true;
                                let _ = tx.send(Err(std::io::Error::other(err.to_string()))).await;
                                break;
                            }
                        }
                    }
                    let Ok(mut conn) = spawn_state.redis.get_multiplexed_async_connection().await else { return };
                    if had_error {
                        let _ = dedup::release_dedup_slot(&mut conn, &claim_key).await;
                        if let Ok(Some(t)) = circuit_breaker::record_failure(&mut conn, &spawn_circuit_key).await {
                            forward::log_circuit_transition(&spawn_state, t).await;
                        }
                        log_audit(&spawn_state.pg, &spawn_req_id, &spawn_api_key, &spawn_org_id, &spawn_agent_id, &spawn_service, &spawn_action, "error", Some("stream interrupted mid-response"), 0, None, false, None, spawn_run_id.as_deref(), spawn_step_id.as_deref(), spawn_end_user_id.as_deref()).await;
                    } else {
                        // Sentinel, not the real body — a stream can't be
                        // dedup-replayed byte-for-byte without buffering it,
                        // which is exactly what streaming exists to avoid.
                        // A duplicate call is rejected instead of replayed;
                        // see the `is_streamed` check above.
                        let sentinel = json!({ "pending": false, "streamed": true });
                        let _ = dedup::complete_dedup_slot_with_ttl(&mut conn, &claim_key, &sentinel, dedup_ttl_seconds).await;
                    }
                });

                let mut builder = axum::response::Response::builder().status(streaming.status);
                if let Some(ct) = &streaming.content_type {
                    builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
                }
                builder = builder.header("X-AgentRaaS-ReqId", &req_id);
                let body = Body::from_stream(ReceiverStream::new(rx));
                match builder.body(body) {
                    Ok(resp) => Response::Stream(resp),
                    Err(_) => err_response(StatusCode::INTERNAL_SERVER_ERROR, &req_id, "An internal error occurred."),
                }
            }
            Err(err) => forward_error_response(state, &mut conn, &claim.key, &circuit_key, err, &req_id, &api_key, &org_id, &agent_id, &service, &action, &payload, start, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await,
        };
    }

    match forward::forward_with_retry(state, &resolved_route, &service, &action, &org_id, &payload, &req_id, &circuit_key, end_user_id.as_deref()).await {
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
            let _ = dedup::complete_dedup_slot_with_ttl(&mut conn, &claim.key, &stored, dedup_ttl_seconds).await;
            if let (Some(run_id), Some(step_id)) = (&run_id, &step_id) {
                let checkpoint_key = agentraas_core::checkpoint::step_key(run_id, step_id);
                let _ = agentraas_core::checkpoint::write_checkpoint(&mut conn, &checkpoint_key, &stored, state.checkpoint_ttl_seconds).await;
            }
            let _ = increment_monthly_usage(state, &org_id).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, &agent_id, &service, &action, "success", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), state.enterprise_mode, Some(&payload), run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await;

            if let Value::Object(ref mut map) = result {
                map.insert("reqId".to_string(), Value::String(req_id.clone()));
            }
            (StatusCode::OK, Json(result)).into()
        }
        Err(err) => forward_error_response(state, &mut conn, &claim.key, &circuit_key, err, &req_id, &api_key, &org_id, &agent_id, &service, &action, &payload, start, run_id.as_deref(), step_id.as_deref(), end_user_id.as_deref()).await,
    }
}

/// Shared by both the streaming and non-streaming success/failure arms of
/// `handle_request` — a `ForwardError` is handled identically either way
/// (nothing has been sent to the caller yet by the time either path sees
/// one, so there's no streaming-specific case to special-case here).
#[allow(clippy::too_many_arguments)]
async fn forward_error_response(
    state: &SharedState,
    conn: &mut redis::aio::MultiplexedConnection,
    claim_key: &str,
    circuit_key: &str,
    err: forward::ForwardError,
    req_id: &str,
    api_key: &str,
    org_id: &str,
    agent_id: &str,
    service: &str,
    action: &str,
    payload: &Value,
    start: std::time::Instant,
    run_id: Option<&str>,
    step_id: Option<&str>,
    end_user_id: Option<&str>,
) -> Response {
    let _ = dedup::release_dedup_slot(conn, claim_key).await;
    if !err.circuit_already_recorded {
        if let Ok(mut c2) = state.redis.get_multiplexed_async_connection().await {
            if let Ok(Some(t)) = circuit_breaker::record_failure(&mut c2, circuit_key).await {
                forward::log_circuit_transition(state, t).await;
            }
        }
    }
    log_audit(&state.pg, req_id, api_key, org_id, agent_id, service, action, "error", Some(&err.message), start.elapsed().as_millis() as i64, None, false, None, run_id, step_id, end_user_id).await;
    tracing::error!(req_id, error = %err.message, "request failed");

    let response_message = if err.upstream_status.is_some() {
        err.message.clone()
    } else {
        "An internal error occurred while processing this request.".to_string()
    };
    if err.upstream_status.is_some() {
        db::write_dead_letter_queue(state, req_id, org_id, agent_id, service, action, payload, &err.message).await;
    }
    let status = err
        .upstream_status
        .and_then(|s| StatusCode::from_u16(s).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(json!({ "error": response_message, "reqId": req_id, "agentraas_note": "Request blocked by AgentRaaS." })),
    )
        .into()
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
            format!("Agency plan is limited to {} client tenants. Contact hello@agentraas.io to increase this.", tenant_cap.limit),
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

/// Fires a real burst of identical requests through the exact same
/// dedup/forward/audit pipeline every agent call goes through — against
/// the built-in `mockpay` sandbox service, so it costs nothing and hits no
/// real credentials — then reports how many actually executed vs. were
/// caught as duplicates. This is the in-console version of the
/// concurrency test on the homepage: a claim the user watches happen in
/// their own Recent Activity feed, not a canned animation.
///
/// Requests are sent one-then-seven rather than all eight at once: a truly
/// simultaneous burst would mostly race the first request mid-flight and
/// come back `blocked` ("duplicate_in_progress"), which is also correct
/// dedup behavior but not the clean "1 executed, 7 caught" shape this is
/// meant to demonstrate. Waiting for the first call to land guarantees the
/// rest hit the completed-result cache path (`status = 'deduplicated'`).
async fn demo_live_test(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let mut org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    let org_id = org_ids.pop().unwrap_or_else(|| format!("demo_{}", user.sub));
    let agent_id = "live-test-agent";

    // One active "Live test" key at a time per user/org — revoke any
    // previous one before minting a fresh secret, so repeated clicks don't
    // pile up rows in the Connect Agent key list.
    sqlx::query("UPDATE api_keys SET revoked_at = NOW() WHERE user_id = $1 AND org_id = $2 AND agent_id = $3 AND revoked_at IS NULL")
        .bind(user.sub)
        .bind(&org_id)
        .bind(agent_id)
        .execute(&state.pg)
        .await?;
    let (raw_key, key_hash, key_prefix) = generate_api_key();
    sqlx::query("INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(user.sub)
        .bind(&org_id)
        .bind(agent_id)
        .bind("Live test")
        .bind(&key_hash)
        .bind(&key_prefix)
        .execute(&state.pg)
        .await?;

    let mut idem_bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut idem_bytes);
    let idempotency_key = Some(format!("livetest_{}", hex::encode(idem_bytes)));
    let payload = json!({ "amount": 4999, "fail": false });

    fn tally(resp: Response, executed: &mut i64, deduplicated: &mut i64, errored: &mut i64) {
        match resp {
            Response::Json(StatusCode::OK, Json(body)) => {
                if body.get("cached").and_then(Value::as_bool).unwrap_or(false) {
                    *deduplicated += 1;
                } else {
                    *executed += 1;
                }
            }
            _ => *errored += 1,
        }
    }

    let mut executed = 0i64;
    let mut deduplicated = 0i64;
    let mut errored = 0i64;

    let first = handle_request(
        &state,
        Source::Sdk,
        RequestIdentity {
            org_id: org_id.clone(),
            agent_id: agent_id.to_string(),
            api_key: raw_key.clone(),
            service: "mockpay".to_string(),
            action: "payment.create".to_string(),
            payload: payload.clone(),
            idempotency_key: idempotency_key.clone(),
            run_id: None,
            step_id: None,
            end_user_id: None,
        },
    )
    .await;
    tally(first, &mut executed, &mut deduplicated, &mut errored);

    const BURST: i64 = 8;
    let mut handles = Vec::with_capacity((BURST - 1) as usize);
    for _ in 1..BURST {
        let state = state.clone();
        let identity = RequestIdentity {
            org_id: org_id.clone(),
            agent_id: agent_id.to_string(),
            api_key: raw_key.clone(),
            service: "mockpay".to_string(),
            action: "payment.create".to_string(),
            payload: payload.clone(),
            idempotency_key: idempotency_key.clone(),
            run_id: None,
            step_id: None,
            end_user_id: None,
        };
        handles.push(tokio::spawn(async move { handle_request(&state, Source::Sdk, identity).await }));
    }
    for h in handles {
        match h.await {
            Ok(resp) => tally(resp, &mut executed, &mut deduplicated, &mut errored),
            Err(_) => errored += 1,
        }
    }

    Ok(Json(json!({
        "sent": BURST,
        "executed": executed,
        "deduplicated": deduplicated,
        "errored": errored,
        "org_id": org_id,
        "agent_id": agent_id,
    })))
}

/// Lets an agent (or a human debugging one) ask "what have I already
/// completed in this run" instead of blindly replaying every step and
/// relying on dedup/checkpoint hits alone — useful once a run has more
/// steps than fit comfortably in the agent's own retry logic. Scoped by
/// the same `x-agentraas-key` org resolution `tools/list` already uses;
/// a run_id from another org is invisible, not just unauthorized, since
/// the query itself is org-scoped rather than checked-then-rejected.
async fn get_run_status(State(state): State<SharedState>, headers: HeaderMap, Path(run_id): Path<String>) -> Result<Json<Value>, ApiError> {
    let api_key = headers.get("x-agentraas-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    let Some(org_id) = resolve_org_from_api_key(&state.pg, api_key).await.ok().flatten() else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "A valid x-agentraas-key header is required."));
    };

    #[derive(sqlx::FromRow)]
    struct StepRow {
        step_id: Option<String>,
        service: String,
        action: String,
        status: String,
        error_type: Option<String>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, StepRow>(
        "SELECT step_id, service, action, status, error_type, (created_at AT TIME ZONE 'UTC') as created_at
         FROM audit_log WHERE run_id = $1 AND org_id = $2 ORDER BY created_at ASC",
    )
    .bind(&run_id)
    .bind(&org_id)
    .fetch_all(&state.pg)
    .await?;

    if rows.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "No steps found for this run_id."));
    }

    Ok(Json(json!({
        "run_id": run_id,
        "steps": rows.into_iter().map(|r| json!({
            "step_id": r.step_id, "service": r.service, "action": r.action,
            "status": r.status, "error_type": r.error_type, "created_at": r.created_at,
        })).collect::<Vec<_>>(),
    })))
}
