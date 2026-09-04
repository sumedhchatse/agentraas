//! Ports `src/core/mcp/index.js`'s `handleMCP` — the MCP JSON-RPC entry
//! point (`tools/list`, `tools/call`). Reuses the exact same dedup/
//! validation/circuit-breaker/forward machinery as the webhook/SDK path
//! (`crate::agent::{db,forward}`), since MCP is just another entry point
//! into the same reliability layer — same as Node's own comment on this
//! file says.

use agentraas_core::{circuit_breaker, dedup, validator};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::agent::db::{
    check_usage_limit, get_effective_dedup_rule, get_effective_rate_limit,
    get_effective_validation_rule, increment_monthly_usage, log_audit, resolve_custom_route,
    verify_api_key, ResolvedRoute,
};
use crate::agent::forward::{forward_with_retry, log_circuit_transition};
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new().route("/mcp", post(handle_mcp))
}

fn generate_request_id() -> String {
    let mut buf = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    format!("req_{}", hex::encode(buf))
}

fn jsonrpc_result(id: &Value, content_json: Value, is_error: bool) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{ "type": "text", "text": content_json.to_string() }],
            "isError": is_error,
        }
    }))
}

fn jsonrpc_error(id: &Value, code: i32, message: impl Into<String>) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    }))
}

async fn handle_mcp(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let jsonrpc = body.get("jsonrpc").and_then(Value::as_str);
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let method = body.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = body.get("params").cloned().unwrap_or(Value::Null);

    if jsonrpc != Some("2.0") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "jsonrpc": "2.0", "error": { "code": -32600, "message": "Invalid Request" }, "id": id })),
        );
    }

    if method == "tools/list" {
        return (StatusCode::OK, Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": state.mcp_tools_list } })));
    }

    if method == "tools/call" {
        return (StatusCode::OK, handle_tools_call(&state, &headers, &id, &params).await);
    }

    (StatusCode::OK, jsonrpc_error(&id, -32601, "Method not found"))
}

async fn handle_tools_call(state: &SharedState, headers: &HeaderMap, id: &Value, params: &Value) -> Json<Value> {
    let tool_name = params.get("name").and_then(Value::as_str);
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let payload = arguments.get("payload").cloned().unwrap_or(json!({}));
    let org_id = arguments.get("org_id").and_then(Value::as_str).unwrap_or("mcp").to_string();
    let agent_id = arguments.get("agent_id").and_then(Value::as_str).unwrap_or("mcp-agent").to_string();
    let idempotency_key = arguments.get("idempotency_key").and_then(Value::as_str).map(String::from);
    let api_key = headers
        .get("x-agentraas-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();
    let req_id = generate_request_id();

    let Some(tool_name) = tool_name else {
        return jsonrpc_error(id, -32602, "Invalid params: \"name\" is required and must be a string.");
    };

    let mut resolved_service_name = String::new();
    let mut resolved_action_name = String::new();
    let mut resolved_route: Option<ResolvedRoute> = None;

    if let Some((svc, act)) = state.tool_name_to_route.get(tool_name) {
        resolved_service_name = svc.clone();
        resolved_action_name = act.clone();
        if let Some(r) = state.service_routes.get(&format!("{svc}.{act}")) {
            resolved_route = Some(ResolvedRoute {
                method: r.method.clone(),
                url: r.url.clone(),
                internal: r.internal,
                auth_type: r.auth_type.clone(),
                auth_header: r.auth_header.clone(),
                content_type: r.content_type.clone(),
                extra_headers: r.extra_headers.clone(),
                fanout_urls: Vec::new(),
                credential_key: svc.clone(),
            });
        }
    }
    if resolved_route.is_none() {
        if let Ok(Some(r)) = resolve_custom_route(state, &org_id, tool_name).await {
            resolved_service_name = "custom".to_string();
            resolved_action_name = tool_name.to_string();
            resolved_route = Some(r);
        }
    }
    let Some(resolved_route) = resolved_route else {
        return jsonrpc_error(id, -32601, format!("Tool not found: {tool_name}"));
    };

    match verify_api_key(&state.pg, &api_key, &org_id, &agent_id).await {
        Ok(v) if !v.ok => {
            return jsonrpc_result(id, json!({ "error": "Invalid or missing API key for this agent.", "reqId": req_id }), true)
        }
        Err(err) => {
            tracing::error!(?err, "verify_api_key failed");
            return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
        }
        _ => {}
    }

    let rate_limit_identity = if api_key != "anonymous" {
        api_key.clone()
    } else {
        format!("{org_id}:{agent_id}")
    };
    let Ok(effective_limit) = get_effective_rate_limit(state, &org_id).await else {
        return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
    };
    let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await else {
        return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
    };
    let bucket_key = format!("ratelimit:agent:{rate_limit_identity}");
    let within_limit = state
        .token_bucket
        .try_consume(&mut conn, &bucket_key, effective_limit as f64, effective_limit as f64 / 60.0, 1.0)
        .await
        .map(|r| r.allowed)
        .unwrap_or(true);
    if !within_limit {
        return jsonrpc_result(id, json!({ "error": "Rate limit exceeded for this agent.", "reqId": req_id }), true);
    }

    let start = std::time::Instant::now();
    let payload_digest = dedup::hash_only(&payload);
    let dedup_field_rule = if idempotency_key.is_some() {
        None
    } else {
        get_effective_dedup_rule(&state.pg, &org_id, &resolved_service_name, &resolved_action_name)
            .await
            .unwrap_or(None)
    };
    let dedup_hash = if let Some(idem) = &idempotency_key {
        dedup::hash_idempotency_key(&api_key, &resolved_service_name, &resolved_action_name, idem)
    } else if let Some(rule) = &dedup_field_rule {
        dedup::hash_field_values(&api_key, &resolved_service_name, &resolved_action_name, &payload, &rule.fields)
    } else {
        dedup::hash_payload(&api_key, &resolved_service_name, &resolved_action_name, &payload)
    };

    let Ok(claim) = dedup::claim_dedup_slot(&mut conn, &dedup_hash).await else {
        return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
    };

    if !claim.claimed {
        let existing = dedup::read_dedup_slot(&mut conn, &claim.key).await.ok().flatten();
        let is_pending = existing.as_ref().and_then(|v| v.get("pending")).and_then(Value::as_bool).unwrap_or(false);
        let Some(existing) = existing else {
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
            return jsonrpc_result(id, json!({ "error": "An identical request is already being processed. Retry shortly.", "reqId": req_id }), true);
        };
        if is_pending {
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
            return jsonrpc_result(id, json!({ "error": "An identical request is already being processed. Retry shortly.", "reqId": req_id }), true);
        }
        if let Some(existing_digest) = existing.get("__payloadDigest").and_then(Value::as_str) {
            if idempotency_key.is_some() && existing_digest != payload_digest {
                log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("idempotency_key_reused"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
                return jsonrpc_result(id, json!({ "error": "This idempotency_key was already used with a different payload. Use a new key for a different request.", "reqId": req_id }), true);
            }
        }
        log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "deduplicated", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
        let mut cached = existing;
        if let Value::Object(ref mut map) = cached {
            map.remove("__payloadDigest");
            map.insert("cached".to_string(), Value::Bool(true));
            map.insert("reqId".to_string(), Value::String(req_id.clone()));
        }
        return jsonrpc_result(id, cached, false);
    }

    // ─── claimed: do the real work ───

    if let Ok(Some(rule)) = get_effective_validation_rule(state, &org_id, &resolved_service_name, &resolved_action_name).await {
        if let Some(validation_error) = validator::validate_fields(&payload, &rule.fields) {
            let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("validation_failed"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
            return jsonrpc_result(id, json!({ "error": validation_error, "reqId": req_id }), true);
        }
    }

    let circuit_key = if resolved_route.credential_key.is_empty() {
        resolved_service_name.clone()
    } else {
        resolved_route.credential_key.clone()
    };
    match circuit_breaker::get_circuit_state(&mut conn, &circuit_key).await {
        Ok((state_str, transition)) => {
            if let Some(t) = transition {
                log_circuit_transition(state, t).await;
            }
            if state_str == "open" {
                let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
                log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("circuit_open"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
                return jsonrpc_result(id, json!({ "error": format!("Circuit breaker open for {resolved_service_name}"), "reqId": req_id }), true);
            }
        }
        Err(err) => {
            tracing::error!(?err, "get_circuit_state failed");
            return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
        }
    }

    let Ok(usage) = check_usage_limit(state, &org_id).await else {
        return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
    };
    if !usage.ok {
        let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
        log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("usage_limit_exceeded"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None).await;
        return jsonrpc_result(
            id,
            json!({ "error": format!("Monthly usage limit reached ({}/{} actions this month). Contact support@agentraas.io to upgrade.", usage.count, usage.limit), "reqId": req_id }),
            true,
        );
    }

    match forward_with_retry(state, &resolved_route, &resolved_service_name, &resolved_action_name, &org_id, &payload, &req_id, &circuit_key).await {
        Ok(mut result) => {
            if let Ok(mut c2) = state.redis.get_multiplexed_async_connection().await {
                if let Ok(Some(t)) = circuit_breaker::record_success(&mut c2, &circuit_key).await {
                    log_circuit_transition(state, t).await;
                }
            }
            let mut stored = result.clone();
            if let (Some(_), Value::Object(ref mut map)) = (&idempotency_key, &mut stored) {
                map.insert("__payloadDigest".to_string(), Value::String(payload_digest.clone()));
            }
            let _ = dedup::complete_dedup_slot(&mut conn, &claim.key, &stored).await;
            let _ = increment_monthly_usage(state, &org_id).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "success", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), state.enterprise_mode, Some(&payload)).await;

            if let Value::Object(ref mut map) = result {
                map.insert("reqId".to_string(), Value::String(req_id.clone()));
            }
            jsonrpc_result(id, result, false)
        }
        Err(err) => {
            let _ = dedup::release_dedup_slot(&mut conn, &claim.key).await;
            if !err.circuit_already_recorded {
                if let Ok(mut c2) = state.redis.get_multiplexed_async_connection().await {
                    if let Ok(Some(t)) = circuit_breaker::record_failure(&mut c2, &circuit_key).await {
                        log_circuit_transition(state, t).await;
                    }
                }
            }
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "error", Some(&err.message), start.elapsed().as_millis() as i64, None, false, None).await;
            tracing::error!(req_id, error = %err.message, "MCP request failed");
            let response_message = if err.upstream_status.is_some() {
                err.message.clone()
            } else {
                "An internal error occurred while processing this request.".to_string()
            };
            if err.upstream_status.is_some() {
                crate::agent::db::write_dead_letter_queue(state, &req_id, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, &payload, &err.message).await;
            }
            jsonrpc_result(id, json!({ "error": response_message, "reqId": req_id }), true)
        }
    }
}
