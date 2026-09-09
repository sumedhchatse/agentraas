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
    get_effective_validation_rule, get_org_validation_overrides, increment_monthly_usage, log_audit,
    resolve_custom_route, resolve_org_from_api_key, select_dedup_hash_mode, verify_api_key,
    DedupHashMode, ResolvedRoute,
};
use crate::auth::is_valid_identifier;
use crate::agent::forward::{forward_with_retry, log_circuit_transition};
use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new().route("/mcp", post(handle_mcp))
}

/// Translates a validation-rule-definition object (the shared shape used by
/// both `config/services.json`'s static `validation` blocks and
/// `custom_validation_rules.fields` — `{field: {type, required, min, max,
/// minLength, maxLength, format, enum}}`, see `validator::validate_fields`
/// for the enforced semantics this must stay in sync with) into a real JSON
/// Schema `payload` property, instead of the generic `{type: "object"}`
/// placeholder every tool previously advertised regardless of what it
/// actually accepts.
fn build_payload_schema(fields: &Value) -> Value {
    let Some(obj) = fields.as_object() else {
        return json!({ "type": "object", "description": "Request payload" });
    };
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, rules) in obj {
        let is_array = rules.get("type").and_then(Value::as_str) == Some("array");
        let mut prop = serde_json::Map::new();
        for key in ["type", "format", "enum"] {
            if let Some(v) = rules.get(key) {
                prop.insert(key.to_string(), v.clone());
            }
        }
        if let Some(v) = rules.get("min") {
            prop.insert("minimum".to_string(), v.clone());
        }
        if let Some(v) = rules.get("max") {
            prop.insert("maximum".to_string(), v.clone());
        }
        // `minLength` means item count on an array rule, character count on
        // a string one (see validate_fields) — JSON Schema has separate
        // keywords for each, `maxLength` is string-only in practice today.
        if let Some(v) = rules.get("minLength") {
            prop.insert(if is_array { "minItems" } else { "minLength" }.to_string(), v.clone());
        }
        if let Some(v) = rules.get("maxLength") {
            prop.insert("maxLength".to_string(), v.clone());
        }
        properties.insert(name.clone(), Value::Object(prop));
        if rules.get("required").and_then(Value::as_bool).unwrap_or(false) {
            required.push(json!(name));
        }
    }
    let mut schema = json!({
        "type": "object",
        "description": "Request payload",
        "properties": properties,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

/// Builds one MCP tool's advertised definition — shared by the curated
/// startup baseline (`main.rs`, using `config/services.json`'s static
/// validation) and `tools/list`'s per-org path above (using that org's
/// `custom_validation_rules` override where one exists), so both stay in
/// sync via the same code rather than two hand-maintained schema shapes.
pub fn build_tool_entry(tool_name: &str, svc_name: &str, act_name: &str, validation_fields: &Value) -> Value {
    json!({
        "name": tool_name,
        "description": format!("AgentRaaS-protected {svc_name} {act_name}"),
        "inputSchema": {
            "type": "object",
            "properties": {
                "payload": build_payload_schema(validation_fields),
                "org_id": { "type": "string", "description": "Organization ID" },
                "idempotency_key": { "type": "string", "description": "Optional — dedupe on this key instead of the exact payload bytes, so you control what counts as a retry of the same operation. Reusing the key with a genuinely different payload is rejected (not silently applied), matching Stripe-style idempotency keys." },
                "run_id": { "type": "string", "description": "Optional — a stable identifier for the current multi-step task. If the same run_id calls this same tool too many times in a row, the call is halted with a structured message instead of executing again, to catch an agent stuck in a loop." },
                "step_id": { "type": "string", "description": "Optional, used with run_id — a stable identifier for this specific step (e.g. \"fetch-invoice\"). If this exact run_id+step_id already completed, the saved result is returned immediately instead of executing again — lets a retried task automatically resume past whatever steps already succeeded." },
            },
            "required": ["payload"],
        },
    })
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
        let api_key = headers.get("x-agentraas-key").and_then(|v| v.to_str().ok()).unwrap_or("");
        let org_id = match resolve_org_from_api_key(&state.pg, api_key).await {
            Ok(org_id) => org_id,
            Err(err) => {
                tracing::error!(?err, "resolve_org_from_api_key failed, falling back to curated tools/list");
                None
            }
        };
        let Some(org_id) = org_id else {
            // No key, an invalid key, or a lookup error — same curated,
            // precomputed list every MCP client got before this feature,
            // zero extra DB work.
            return (StatusCode::OK, Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": state.mcp_tools_list } })));
        };
        let overrides = match get_org_validation_overrides(&state.pg, &org_id).await {
            Ok(overrides) => overrides,
            Err(err) => {
                tracing::error!(?err, org_id, "get_org_validation_overrides failed, falling back to curated tools/list");
                return (StatusCode::OK, Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": state.mcp_tools_list } })));
            }
        };
        let empty = Value::Object(Default::default());
        let tools: Vec<Value> = state
            .tool_name_to_route
            .iter()
            .map(|(tool_name, (svc_name, act_name))| {
                let fields = overrides
                    .get(&(svc_name.clone(), act_name.clone()))
                    .or_else(|| state.service_routes.get(&format!("{svc_name}.{act_name}")).map(|r| &r.validation))
                    .unwrap_or(&empty);
                build_tool_entry(tool_name, svc_name, act_name, fields)
            })
            .collect();
        return (StatusCode::OK, Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })));
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
    let run_id = arguments.get("run_id").and_then(Value::as_str).map(String::from);
    let step_id = arguments.get("step_id").and_then(Value::as_str).map(String::from);
    let api_key = headers
        .get("x-agentraas-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();
    let req_id = generate_request_id();

    let Some(tool_name) = tool_name else {
        return jsonrpc_error(id, -32602, "Invalid params: \"name\" is required and must be a string.");
    };
    // org_id/agent_id land in audit_log and get rendered in the dashboard —
    // same charset every other org_id-accepting route in this app already
    // enforces (see agent/mod.rs's handle_request), rather than letting
    // arbitrary strings (HTML, oversized values) reach it from here too.
    if !is_valid_identifier(&org_id) || !is_valid_identifier(&agent_id) {
        return jsonrpc_error(id, -32602, "Invalid params: org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only.");
    }

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
                // MCP tool calls don't support streaming passthrough (see
                // agent/mod.rs) — stringified into a single JSON-RPC "text"
                // field regardless — but keep this field honest rather than
                // silently forcing false and disagreeing with config.
                streaming: r.streaming,
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

    // State Checkpointing — see agent::mod's identical wiring (checked
    // first, before loop-detection: a checkpoint hit is a known-already-
    // done step, not a new attempt to count toward the loop budget).
    if let (Some(run_id), Some(step_id)) = (&run_id, &step_id) {
        let checkpoint_key = agentraas_core::checkpoint::step_key(run_id, step_id);
        if let Ok(Some(mut cached)) = agentraas_core::checkpoint::read_checkpoint(&mut conn, &checkpoint_key).await {
            if let Value::Object(ref mut map) = cached {
                map.insert("checkpointed".to_string(), Value::Bool(true));
                map.insert("reqId".to_string(), Value::String(req_id.clone()));
            }
            return jsonrpc_result(id, cached, false);
        }
    }

    // Agent Run Budgeting & Loop Detection — see agent::mod's identical
    // wiring for the webhook/SDK path; only engages when the MCP caller
    // supplies a run_id argument tagging a multi-step task.
    if let Some(run_id) = &run_id {
        match agentraas_core::agent_run::record_call_and_check(
            &mut conn,
            &org_id,
            &agent_id,
            run_id,
            &resolved_service_name,
            &resolved_action_name,
            state.agent_loop_max_repeats,
            state.agent_run_ttl_seconds,
        )
        .await
        {
            Ok(check) if check.tripped => {
                return jsonrpc_result(
                    id,
                    json!({
                        "error": "agent_circuit_open",
                        "message": format!(
                            "Tool execution halted: You have called {resolved_service_name}.{resolved_action_name} {count} times with no state change. Re-evaluate your strategy.",
                            count = check.count
                        ),
                        "reqId": req_id,
                    }),
                    true,
                );
            }
            Ok(_) => {}
            Err(err) => tracing::error!(?err, "agent loop-detection check failed"),
        }
    }

    let start = std::time::Instant::now();
    let payload_digest = dedup::hash_only(&payload);
    // See agent/mod.rs::handle_request for why this is looked up unconditionally.
    let dedup_field_rule = get_effective_dedup_rule(&state.pg, &org_id, &resolved_service_name, &resolved_action_name)
        .await
        .unwrap_or(None);
    let dedup_hash = match select_dedup_hash_mode(idempotency_key.as_deref(), dedup_field_rule.as_ref()) {
        DedupHashMode::IdempotencyKey(idem) => {
            dedup::hash_idempotency_key(&api_key, &resolved_service_name, &resolved_action_name, idem)
        }
        DedupHashMode::Fields { fields, normalize } => dedup::hash_field_values(
            &api_key, &resolved_service_name, &resolved_action_name, &payload, fields, normalize,
        ),
        DedupHashMode::Payload => dedup::hash_payload(&api_key, &resolved_service_name, &resolved_action_name, &payload),
    };
    let dedup_ttl_seconds = dedup_field_rule.as_ref().and_then(|r| r.ttl_seconds);

    let Ok(claim) = dedup::claim_dedup_slot_with_ttl(&mut conn, &dedup_hash, dedup_ttl_seconds).await else {
        return jsonrpc_result(id, json!({ "error": "An internal error occurred.", "reqId": req_id }), true);
    };

    if !claim.claimed {
        let existing = dedup::read_dedup_slot(&mut conn, &claim.key).await.ok().flatten();
        let is_pending = existing.as_ref().and_then(|v| v.get("pending")).and_then(Value::as_bool).unwrap_or(false);
        // A REST-side streaming call (agent/mod.rs) that completed leaves a
        // non-replayable sentinel in this same dedup keyspace, not the real
        // response body — reject rather than replaying the sentinel as if
        // it were a real tool result. See agent/mod.rs's own `is_streamed`
        // check for the matching REST-side guard.
        let is_streamed = existing.as_ref().and_then(|v| v.get("streamed")).and_then(Value::as_bool).unwrap_or(false);
        let Some(existing) = existing else {
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
            return jsonrpc_result(id, json!({ "error": "An identical request is already being processed. Retry shortly.", "reqId": req_id }), true);
        };
        if is_pending {
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("duplicate_in_progress"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
            return jsonrpc_result(id, json!({ "error": "An identical request is already being processed. Retry shortly.", "reqId": req_id }), true);
        }
        if is_streamed {
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("duplicate_of_streamed_response"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
            return jsonrpc_result(id, json!({ "error": "An identical request was already served as a streaming response and cannot be replayed. Wait for the dedup window to expire, or use a new idempotency_key.", "reqId": req_id }), true);
        }
        if let Some(existing_digest) = existing.get("__payloadDigest").and_then(Value::as_str) {
            if idempotency_key.is_some() && existing_digest != payload_digest {
                log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("idempotency_key_reused"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
                return jsonrpc_result(id, json!({ "error": "This idempotency_key was already used with a different payload. Use a new key for a different request.", "reqId": req_id }), true);
            }
        }
        log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "deduplicated", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
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
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("validation_failed"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
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
                log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("circuit_open"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
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
        log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "blocked", Some("usage_limit_exceeded"), start.elapsed().as_millis() as i64, Some(&dedup_hash), false, None, run_id.as_deref(), step_id.as_deref()).await;
        return jsonrpc_result(
            id,
            json!({ "error": format!("Monthly usage limit reached ({}/{} actions this month). Contact hello@agentraas.io to upgrade.", usage.count, usage.limit), "reqId": req_id }),
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
            let _ = dedup::complete_dedup_slot_with_ttl(&mut conn, &claim.key, &stored, dedup_ttl_seconds).await;
            if let (Some(run_id), Some(step_id)) = (&run_id, &step_id) {
                let checkpoint_key = agentraas_core::checkpoint::step_key(run_id, step_id);
                let _ = agentraas_core::checkpoint::write_checkpoint(&mut conn, &checkpoint_key, &stored, state.checkpoint_ttl_seconds).await;
            }
            let _ = increment_monthly_usage(state, &org_id).await;
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "success", None, start.elapsed().as_millis() as i64, Some(&dedup_hash), state.enterprise_mode, Some(&payload), run_id.as_deref(), step_id.as_deref()).await;

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
            log_audit(&state.pg, &req_id, &api_key, &org_id, "mcp-agent", &resolved_service_name, &resolved_action_name, "error", Some(&err.message), start.elapsed().as_millis() as i64, None, false, None, run_id.as_deref(), step_id.as_deref()).await;
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
