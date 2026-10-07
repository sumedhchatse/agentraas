//! The MCP JSON-RPC entry point (`tools/list`, `tools/call`). A tool call
//! runs through the same pipeline as webhook/SDK calls
//! (`agent::handle_request` with `Source::Mcp`); this file only resolves tool
//! names and shapes JSON-RPC answers.

use agentraas_core::circuit_breaker;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::agent::db::{get_org_validation_overrides, resolve_custom_route, resolve_mcp_tool_route, resolve_org_from_api_key, ResolvedRoute};
use crate::agent::{handle_request, RequestIdentity, Response as AgentResponse, Source, Target};
use crate::auth::is_valid_identifier;
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
        if name == "*" {
            continue; // whole-payload rule (e.g. no_secrets), not a property
        }
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
                "idempotency_key": { "type": "string", "description": "Optional — dedupe on this key instead of the exact payload bytes, so you control what counts as a retry of the same operation. Reusing the key with a different payload is rejected (not silently applied), matching Stripe-style idempotency keys." },
                "run_id": { "type": "string", "description": "Optional — a stable identifier for the current multi-step task. If the same run_id calls this same tool too many times in a row, the call is halted with a structured message instead of executing again, to catch an agent stuck in a loop." },
                "step_id": { "type": "string", "description": "Optional, used with run_id — a stable identifier for this specific step (e.g. \"fetch-invoice\"). If this exact run_id+step_id already completed, the saved result is returned immediately instead of executing again — lets a retried task automatically resume past whatever steps already succeeded." },
                "end_user_id": { "type": "string", "description": "Optional — scopes credential lookup to this specific end-user instead of the org's shared credential (On-Behalf-Of End-User Identity). No fallback: if no credential is connected for this exact end-user, the call fails rather than silently using a shared/admin key. Also scopes deduplication, so two end-users calling with identical-looking payloads never collide." },
                "resource_id": { "type": "string", "description": "Optional — the thing this call acts on (an order, an account). While a call naming a resource_id is in flight, another call naming the same one is refused instead of racing it." },
            },
            "required": ["payload"],
        },
    })
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

const MCP_TOOLS_CACHE_TTL_SECONDS: i64 = 300;

/// MCP Custom Actions — probes each of the org's registered third-party MCP
/// servers for its own `tools/list` and merges the results in as
/// `<server_name>.<remote_tool>`, so an agent sees them alongside AgentRaaS's
/// curated tools without a separate discovery step. Run in parallel, each
/// with its own short timeout — `tools/list` isn't `tools/call`'s hot path,
/// but it shouldn't hang the whole response on one slow/dead server either.
/// A server that times out or errors is silently skipped (logged), not
/// fatal to the rest of the list. Each server's result is Redis-cached for
/// 5 minutes (fast-follow noted when this shipped uncached — a registered
/// server's own tool list rarely changes minute to minute, so a short TTL
/// trades a little staleness for skipping the live probe on almost every
/// `tools/list` call).
async fn fetch_registered_mcp_tools(state: &SharedState, org_id: &str) -> Vec<Value> {
    #[derive(sqlx::FromRow)]
    struct Row {
        name: String,
        target_url: String,
        auth_type: String,
        auth_header_name: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT name, target_url, auth_type, auth_header_name FROM custom_mcp_servers WHERE org_id=$1 AND revoked_at IS NULL",
    )
    .bind(org_id)
    .fetch_all(&state.pg)
    .await
    .unwrap_or_default();
    if rows.is_empty() {
        return Vec::new();
    }

    let fetches = rows.into_iter().map(|row| async move {
        let cache_key = format!("mcp_tools_cache:{org_id}:{}", row.name);
        if let Ok(mut conn) = state.redis_conn_result() {
            let cached: Option<String> = redis::cmd("GET").arg(&cache_key).query_async(&mut conn).await.ok().flatten();
            if let Some(cached) = cached {
                if let Ok(tools) = serde_json::from_str::<Vec<Value>>(&cached) {
                    return tools;
                }
            }
        }

        let tools = probe_mcp_server_tools(state, org_id, &row.name, &row.target_url, &row.auth_type, row.auth_header_name.as_deref()).await;

        if let Ok(mut conn) = state.redis_conn_result() {
            if let Ok(serialized) = serde_json::to_string(&tools) {
                let _: Result<(), _> = redis::cmd("SET").arg(&cache_key).arg(serialized).arg("EX").arg(MCP_TOOLS_CACHE_TTL_SECONDS).query_async(&mut conn).await;
            }
        }
        tools
    });

    futures_util::future::join_all(fetches).await.into_iter().flatten().collect()
}

/// Probes one MCP server's `tools/list` and returns its tools renamed to
/// `<server_name>.<remote_tool>`. Shared by `fetch_registered_mcp_tools`
/// above (the live `tools/list` merge, cached) and `mcp_servers.rs`'s
/// registration endpoint (a one-off probe at save time, so the caller gets
/// immediate confirmation the URL actually speaks MCP and a count of what
/// it found — auto-discovered, not hand-typed).
pub(crate) async fn probe_mcp_server_tools(
    state: &SharedState,
    org_id: &str,
    server_name: &str,
    target_url: &str,
    auth_type: &str,
    auth_header_name: Option<&str>,
) -> Vec<Value> {
    let credential = crate::agent::db::get_credential(state, &format!("mcp:{server_name}"), org_id, None).await;
    if auth_type != "none" && credential.is_none() {
        return Vec::new();
    }
    let mut builder = state
        .http_client
        .post(target_url)
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(3));
    if let Some(cred) = &credential {
        match auth_type {
            "basic" => {
                let username = cred.username.clone().or_else(|| cred.api_key.clone()).unwrap_or_default();
                builder = builder.basic_auth(username, cred.password.clone());
            }
            "custom-header" => {
                if let Some(header_name) = auth_header_name {
                    builder = builder.header(header_name, cred.api_key.clone().or_else(|| cred.username.clone()).unwrap_or_default());
                }
            }
            _ => {
                let key = cred.api_key.clone().or_else(|| cred.username.clone()).unwrap_or_default();
                builder = builder.header("Authorization", format!("Bearer {key}"));
            }
        }
    }
    let rpc_request = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let Ok(response) = builder.json(&rpc_request).send().await else {
        tracing::warn!(server = server_name, "MCP Custom Action tools/list probe failed, skipping");
        return Vec::new();
    };
    let body: Value = response.json().await.unwrap_or(Value::Null);
    let Some(remote_tools) = body.get("result").and_then(|r| r.get("tools")).and_then(Value::as_array) else {
        return Vec::new();
    };
    remote_tools
        .iter()
        .filter_map(|t| {
            let remote_name = t.get("name").and_then(Value::as_str)?;
            let mut entry = t.clone();
            entry["name"] = json!(format!("{server_name}.{remote_name}"));
            Some(entry)
        })
        .collect()
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
        let mut tools: Vec<Value> = Vec::new();
        let mut circuit_keys_by_index: Vec<String> = Vec::new();
        for (tool_name, (svc_name, act_name)) in state.tool_name_to_route.iter() {
            let fields = overrides
                .get(&(svc_name.clone(), act_name.clone()))
                .or_else(|| state.service_routes.get(&format!("{svc_name}.{act_name}")).map(|r| &r.validation))
                .unwrap_or(&empty);
            tools.push(build_tool_entry(tool_name, svc_name, act_name, fields));
            circuit_keys_by_index.push(svc_name.clone());
        }
        for mcp_tool in fetch_registered_mcp_tools(&state, &org_id).await {
            let server = mcp_tool.get("name").and_then(Value::as_str).and_then(|n| n.split_once('.')).map(|(s, _)| s.to_string()).unwrap_or_default();
            circuit_keys_by_index.push(format!("mcp:{server}"));
            tools.push(mcp_tool);
        }

        // Tool health — attach each tool's current circuit-breaker state so
        // an agent can check before calling instead of burning a turn on a
        // call it could've known would fail. Best-effort: a Redis hiccup
        // here just means every tool goes out unlabeled, not a broken list.
        if let Ok(mut conn) = state.redis_conn_result() {
            let unique_keys: Vec<String> = circuit_keys_by_index.iter().cloned().collect::<std::collections::HashSet<_>>().into_iter().collect();
            if let Ok((states_map, _)) = circuit_breaker::get_circuit_states_batch(&mut conn, &unique_keys).await {
                for (tool, key) in tools.iter_mut().zip(circuit_keys_by_index.iter()) {
                    let circuit_state = states_map.get(key).cloned().unwrap_or_else(|| "closed".to_string());
                    tool["x-agentraas-circuit-state"] = json!(circuit_state);
                }
            }
        }
        return (StatusCode::OK, Json(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } })));
    }

    if method == "tools/call" {
        return (StatusCode::OK, handle_tools_call(&state, &headers, &id, &params).await);
    }

    (StatusCode::OK, jsonrpc_error(&id, -32601, "Method not found"))
}

/// `tools/call`: resolve the tool name, then run the one shared request
/// pipeline (`agent::handle_request`, `Source::Mcp`) and wrap its answer as
/// a JSON-RPC result (`isError` for any non-2xx status).
async fn handle_tools_call(state: &SharedState, headers: &HeaderMap, id: &Value, params: &Value) -> Json<Value> {
    let tool_name = params.get("name").and_then(Value::as_str);
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    let arg = |k: &str| arguments.get(k).and_then(Value::as_str).map(String::from);
    let payload = arguments.get("payload").cloned().unwrap_or(json!({}));
    let org_id = arg("org_id").unwrap_or_else(|| "mcp".to_string());
    let agent_id = arg("agent_id").unwrap_or_else(|| "mcp-agent".to_string());
    let api_key = headers
        .get("x-agentraas-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("anonymous")
        .to_string();

    let Some(tool_name) = tool_name else {
        return jsonrpc_error(id, -32602, "Invalid params: \"name\" is required and must be a string.");
    };
    // Checked again in the pipeline; here so a bad value gets the JSON-RPC
    // invalid-params code before any lookup uses it.
    if !is_valid_identifier(&org_id) || !is_valid_identifier(&agent_id) {
        return jsonrpc_error(id, -32602, "Invalid params: org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only.");
    }

    // Curated service tool, then a custom action, then a tool on a
    // registered MCP server ("<server_name>.<remote_tool>", a real query,
    // so tried last).
    let resolved = if let Some((svc, act)) = state.tool_name_to_route.get(tool_name) {
        state.service_routes.get(&format!("{svc}.{act}")).map(|r| {
            let route = ResolvedRoute {
                method: r.method.clone(),
                url: r.url.clone(),
                internal: r.internal,
                auth_type: r.auth_type.clone(),
                auth_header: r.auth_header.clone(),
                content_type: r.content_type.clone(),
                extra_headers: r.extra_headers.clone(),
                fanout_urls: Vec::new(),
                credential_key: svc.clone(),
                streaming: r.streaming,
            };
            (svc.clone(), act.clone(), Target::Http(route))
        })
    } else {
        None
    };
    let resolved = match resolved {
        Some(r) => Some(r),
        None => match resolve_custom_route(state, &org_id, tool_name).await {
            Ok(Some(r)) => Some(("custom".to_string(), tool_name.to_string(), Target::Http(r))),
            _ => match resolve_mcp_tool_route(&state.pg, &org_id, tool_name).await {
                Ok(Some(r)) => Some((format!("mcp:{}", r.credential_key.trim_start_matches("mcp:")), r.remote_tool_name.clone(), Target::Mcp(r))),
                _ => None,
            },
        },
    };
    let Some((service, action, target)) = resolved else {
        return jsonrpc_error(id, -32601, format!("Tool not found: {tool_name}"));
    };

    let identity = RequestIdentity {
        org_id,
        agent_id,
        api_key,
        service,
        action,
        payload,
        idempotency_key: arg("idempotency_key"),
        run_id: arg("run_id"),
        step_id: arg("step_id"),
        end_user_id: arg("end_user_id"),
        resource_id: arg("resource_id"),
        target: Some(target),
    };
    match handle_request(state, Source::Mcp, identity).await {
        AgentResponse::Json(status, Json(body)) => jsonrpc_result(id, body, !status.is_success()),
        // Source::Mcp never streams (agent::handle_request buffers it).
        AgentResponse::Stream(_) => jsonrpc_result(id, json!({ "error": "An internal error occurred." }), true),
    }
}
