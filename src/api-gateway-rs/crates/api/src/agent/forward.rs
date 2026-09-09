//! Ports `forwardAction`/`forwardWithRetry`/`isRetryableError` from
//! `src/core/proxy/index.js` — the actual outbound call to a curated
//! service or custom action, with circuit-breaker-aware retry.

use std::time::Duration;

use agentraas_core::circuit_breaker;
use serde_json::{json, Value};

use super::db::{extract_upstream_error_message, get_credential, ResolvedMcpRoute, ResolvedRoute};
use crate::state::SharedState;

pub struct ForwardError {
    pub message: String,
    /// Present only when the upstream itself responded (vs. a network/
    /// internal error) — mirrors Node's `err.response`.
    pub upstream_status: Option<u16>,
    pub upstream_body: Option<Value>,
    /// Set once `recordFailure` has already been called for this error, so
    /// the caller's own catch block doesn't double-count it — mirrors
    /// `err.circuitAlreadyRecorded`.
    pub circuit_already_recorded: bool,
}

fn is_retryable(err: &ForwardError) -> bool {
    match err.upstream_status {
        None => true, // network error, timeout, DNS failure
        Some(status) => status == 429 || (500..=599).contains(&status),
    }
}

/// Everything `forward_action` and `forward_action_streaming` share: SSRF
/// re-check, credential lookup, and building the outbound request up to
/// (but not including) `.send()` — so the two response-handling strategies
/// (buffer-and-parse vs. pass-through) don't have to duplicate the request
/// side.
async fn build_request(
    state: &SharedState,
    route: &ResolvedRoute,
    service_name: &str,
    org_id: &str,
    payload: &Value,
    req_id: &str,
) -> Result<reqwest::RequestBuilder, ForwardError> {
    // Custom actions and inbound-webhook destinations are validated for
    // SSRF (`validate_target_url`) once, at registration time — a
    // destination's DNS record can change afterward (a low-TTL rebind to
    // an internal/metadata IP), and a stored destination gets called
    // indefinitely, not just once. Re-checking here closes that standing
    // window instead of only protecting the moment of registration. Not
    // applied to curated services, whose URLs come from this app's own
    // static config, not user input.
    if service_name == "custom" {
        if let Some(err) = crate::util::validate_target_url(&route.url).await {
            return Err(ForwardError {
                message: format!("Target URL failed a safety re-check: {err}"),
                upstream_status: None,
                upstream_body: None,
                circuit_already_recorded: false,
            });
        }
    }

    let credential = get_credential(state, &route.credential_key, org_id).await;

    if !route.internal && route.auth_type != "none" && credential.is_none() {
        return Err(ForwardError {
            message: format!(
                "No credentials configured for {service_name}. Add them from the dashboard's Credentials panel."
            ),
            upstream_status: None,
            upstream_body: None,
            circuit_already_recorded: false,
        });
    }

    let url = substitute_env_placeholders(&route.url);

    let mut builder = state
        .http_client
        .request(
            route
                .method
                .parse()
                .unwrap_or(reqwest::Method::POST),
            &url,
        )
        .header("Content-Type", &route.content_type)
        .header("X-AgentRaaS-ReqId", req_id)
        .timeout(Duration::from_secs(30));

    if let Some(extra) = &route.extra_headers {
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    builder = builder.header(k, s);
                }
            }
        }
    }

    if let Some(cred) = &credential {
        match route.auth_type.as_str() {
            "basic" => {
                let username = cred.username.clone().or_else(|| cred.api_key.clone()).unwrap_or_default();
                let password = cred.password.clone().unwrap_or_default();
                builder = builder.basic_auth(username, Some(password));
            }
            "custom-header" => {
                if let Some(header_name) = &route.auth_header {
                    let key = cred.api_key.clone().or_else(|| cred.username.clone()).unwrap_or_default();
                    builder = builder.header(header_name, key);
                }
            }
            _ => {
                if let Some(header_name) = &route.auth_header {
                    let key = cred.api_key.clone().or_else(|| cred.username.clone()).unwrap_or_default();
                    let value = if header_name == "Authorization" {
                        format!("Bearer {key}")
                    } else {
                        key
                    };
                    builder = builder.header(header_name, value);
                }
            }
        }
    }

    let builder = if route.content_type == "application/x-www-form-urlencoded" {
        // Stripe-style services expect form-encoded bodies — flatten the
        // (already-validated) JSON payload into form fields, same as
        // axios's `application/x-www-form-urlencoded` content type would
        // when handed a plain object.
        let form: Vec<(String, String)> = payload
            .as_object()
            .map(|obj| {
                obj.iter()
                    .map(|(k, v)| (k.clone(), value_to_form_string(v)))
                    .collect()
            })
            .unwrap_or_default();
        builder.form(&form)
    } else {
        builder.json(payload)
    };

    Ok(builder)
}

pub async fn forward_action(
    state: &SharedState,
    route: &ResolvedRoute,
    service_name: &str,
    action_name: &str,
    org_id: &str,
    payload: &Value,
    req_id: &str,
) -> Result<Value, ForwardError> {
    let builder = build_request(state, route, service_name, org_id, payload, req_id).await?;

    let response = builder.send().await.map_err(|err| ForwardError {
        message: err.to_string(),
        upstream_status: None,
        upstream_body: None,
        circuit_already_recorded: false,
    })?;

    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);

    if status.as_u16() >= 400 {
        let message = extract_upstream_error_message(&body)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(ForwardError {
            message,
            upstream_status: Some(status.as_u16()),
            upstream_body: Some(body),
            circuit_already_recorded: false,
        });
    }

    // Slack's Web API always returns HTTP 200, even on failure.
    if service_name == "slack" && body.get("ok") == Some(&Value::Bool(false)) {
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Slack API returned ok:false")
            .to_string();
        return Err(ForwardError {
            message,
            upstream_status: Some(status.as_u16()),
            upstream_body: Some(body),
            circuit_already_recorded: false,
        });
    }

    let upstream_id = body
        .get("id")
        .or_else(|| body.get("object_id"))
        .or_else(|| body.get("sid"))
        .cloned()
        .unwrap_or(Value::Null);

    // Tool Output Sanitization (Enterprise, opt-in per org): applied only to
    // what's actually surfaced to the calling agent below, never to
    // `upstream_id` above — that's extracted from the real response for
    // internal tracking/audit and must stay accurate regardless of this
    // per-org toggle.
    #[cfg(feature = "enterprise")]
    let body = if state.enterprise_mode
        && super::db::is_output_sanitization_enabled(&state.pg, org_id).await.unwrap_or(false)
    {
        agentraas_core::output_sanitize::sanitize_output(&body)
    } else {
        body
    };

    // Tool Result & Context Pruner — Community + Enterprise both get this,
    // applied after sanitization (order doesn't matter for correctness,
    // just picking one) and only to what's surfaced to the agent below.
    let body = if super::db::is_pruning_enabled(&state.pg, org_id).await.unwrap_or(false) {
        agentraas_core::pruner::prune_output(&body)
    } else {
        body
    };

    Ok(json!({
        "service": service_name,
        "action": action_name,
        "forwarded": true,
        "upstream_status": status.as_u16(),
        "upstream_id": upstream_id,
        "upstream_response": body,
        "timestamp": crate::util::iso_now(),
    }))
}

/// MCP Custom Actions' forwarder — the JSON-RPC counterpart to
/// `forward_action` above. Builds a `tools/call` envelope against a
/// registered third-party MCP server and unwraps its response into the
/// same `Result<Value, ForwardError>` shape `forward_action` returns, so
/// `mcp.rs::handle_tools_call` can call this in place of
/// `forward_with_retry` without needing a second reliability pipeline —
/// the dedup claim, circuit-breaker check, and audit logging around this
/// call are all unchanged, they just don't know the difference.
///
/// v1 deliberately does a single attempt, no retry-with-backoff loop like
/// `forward_with_retry` has — still wrapped in circuit-breaker record_success/
/// record_failure by the caller either way. Add a retrying variant once this
/// is proven in real use; not worth duplicating the backoff loop for a v1.
pub async fn forward_mcp_tool_call(
    state: &SharedState,
    route: &ResolvedMcpRoute,
    org_id: &str,
    payload: &Value,
    req_id: &str,
) -> Result<Value, ForwardError> {
    if let Some(err) = crate::util::validate_target_url(&route.target_url).await {
        return Err(ForwardError {
            message: format!("Target URL failed a safety re-check: {err}"),
            upstream_status: None,
            upstream_body: None,
            circuit_already_recorded: false,
        });
    }

    let credential = get_credential(state, &route.credential_key, org_id).await;
    if route.auth_type != "none" && credential.is_none() {
        return Err(ForwardError {
            message: "No credentials configured for this MCP server. Add them from the dashboard's MCP Servers panel.".to_string(),
            upstream_status: None,
            upstream_body: None,
            circuit_already_recorded: false,
        });
    }

    let mut builder = state
        .http_client
        .post(&route.target_url)
        .header("Content-Type", "application/json")
        .header("X-AgentRaaS-ReqId", req_id)
        .timeout(Duration::from_secs(30));

    if let Some(cred) = &credential {
        match route.auth_type.as_str() {
            "basic" => {
                let username = cred.username.clone().or_else(|| cred.api_key.clone()).unwrap_or_default();
                let password = cred.password.clone().unwrap_or_default();
                builder = builder.basic_auth(username, Some(password));
            }
            "custom-header" => {
                if let Some(header_name) = &route.auth_header {
                    let key = cred.api_key.clone().or_else(|| cred.username.clone()).unwrap_or_default();
                    builder = builder.header(header_name, key);
                }
            }
            _ => {
                if let Some(header_name) = &route.auth_header {
                    let key = cred.api_key.clone().or_else(|| cred.username.clone()).unwrap_or_default();
                    let value = if header_name == "Authorization" { format!("Bearer {key}") } else { key };
                    builder = builder.header(header_name, value);
                }
            }
        }
    }

    let rpc_request = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "tools/call",
        "params": { "name": route.remote_tool_name, "arguments": payload },
    });

    let response = builder.json(&rpc_request).send().await.map_err(|err| ForwardError {
        message: err.to_string(),
        upstream_status: None,
        upstream_body: None,
        circuit_already_recorded: false,
    })?;

    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);

    if status.as_u16() >= 400 {
        let message = extract_upstream_error_message(&body).unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(ForwardError { message, upstream_status: Some(status.as_u16()), upstream_body: Some(body), circuit_already_recorded: false });
    }
    // JSON-RPC-level error — a 200 with an "error" field, distinct from a
    // transport-level 4xx/5xx above.
    if body.get("error").is_some() {
        let message = extract_upstream_error_message(&body).unwrap_or_else(|| "MCP server returned a JSON-RPC error".to_string());
        return Err(ForwardError { message, upstream_status: Some(status.as_u16()), upstream_body: Some(body), circuit_already_recorded: false });
    }
    let result = body.get("result").cloned().unwrap_or(Value::Null);
    // MCP's own convention: a successful JSON-RPC envelope whose result
    // still carries isError:true means the TOOL failed, not the transport —
    // same "genuine failure" treatment forward_action gives Slack's
    // ok:false (which is also always HTTP 200).
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("MCP tool call failed")
            .to_string();
        return Err(ForwardError { message, upstream_status: Some(status.as_u16()), upstream_body: Some(body), circuit_already_recorded: false });
    }

    Ok(json!({
        "service": format!("mcp:{}", route.credential_key.trim_start_matches("mcp:")),
        "action": route.remote_tool_name,
        "forwarded": true,
        "upstream_status": status.as_u16(),
        "mcp_result": result,
        "timestamp": crate::util::iso_now(),
    }))
}

/// A streaming upstream response: status/headers are already available, but
/// the body is deliberately left unconsumed so the caller can pipe it
/// through live (`Response::bytes_stream()`/`chunk()`) instead of buffering
/// it the way `forward_action` does at its `response.json().await` line —
/// the one thing that makes SSE/chunked upstream responses (an LLM tool
/// streaming tokens, for example) currently unsupported.
pub struct StreamingForward {
    pub status: reqwest::StatusCode,
    pub content_type: Option<String>,
    pub response: reqwest::Response,
}

/// Streaming counterpart to `forward_action`. Shares SSRF re-check,
/// credential lookup, and request-building via `build_request`; diverges
/// only at response time: a 2xx response is handed back with its body
/// untouched, a 4xx/5xx response is buffered (small, error bodies only) so
/// error handling/retry behaves identically to the non-streaming path.
/// Deliberately skips: enterprise output sanitization, the pruner, and the
/// Slack `ok:false` check — all of those need the parsed body, which a
/// stream doesn't have; a streaming route is expected to be a raw
/// token/event feed, not a structured API response those checks apply to.
pub async fn forward_action_streaming(
    state: &SharedState,
    route: &ResolvedRoute,
    service_name: &str,
    org_id: &str,
    payload: &Value,
    req_id: &str,
) -> Result<StreamingForward, ForwardError> {
    let builder = build_request(state, route, service_name, org_id, payload, req_id).await?;

    let response = builder.send().await.map_err(|err| ForwardError {
        message: err.to_string(),
        upstream_status: None,
        upstream_body: None,
        circuit_already_recorded: false,
    })?;

    let status = response.status();

    if status.as_u16() >= 400 {
        let body: Value = response.json().await.unwrap_or(Value::Null);
        let message = extract_upstream_error_message(&body)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(ForwardError {
            message,
            upstream_status: Some(status.as_u16()),
            upstream_body: Some(body),
            circuit_already_recorded: false,
        });
    }

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    Ok(StreamingForward { status, content_type, response })
}

/// Streaming counterpart to `forward_with_retry`. Retry only ever applies to
/// the pre-stream `Err` case (a network error, or a buffered 4xx/5xx before
/// any bytes reached the client) — once a `StreamingForward` is returned
/// here, the caller is already committed to forwarding it, so there is no
/// "retry after the first byte" case to guard against separately; it falls
/// out of `Ok`/`Err` never being retried once `Ok`.
pub async fn forward_with_retry_streaming(
    state: &SharedState,
    route: &ResolvedRoute,
    service_name: &str,
    org_id: &str,
    payload: &Value,
    req_id: &str,
    circuit_key: &str,
) -> Result<StreamingForward, ForwardError> {
    let mut last_error = None;

    for attempt in 1..=state.proxy_retry_max_attempts {
        match forward_action_streaming(state, route, service_name, org_id, payload, req_id).await {
            Ok(result) => return Ok(result),
            Err(mut err) => {
                err.circuit_already_recorded = true;
                if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
                    if let Ok(Some(transition)) = circuit_breaker::record_failure(&mut conn, circuit_key).await {
                        log_circuit_transition(state, transition).await;
                    }
                }

                let retryable = is_retryable(&err);
                let is_last_attempt = attempt == state.proxy_retry_max_attempts;

                if is_last_attempt || !retryable {
                    last_error = Some(err);
                    break;
                }

                let circuit_open = {
                    if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
                        circuit_breaker::get_circuit_state(&mut conn, circuit_key)
                            .await
                            .map(|(s, _)| s == "open")
                            .unwrap_or(false)
                    } else {
                        false
                    }
                };
                if circuit_open {
                    last_error = Some(err);
                    break;
                }

                let delay_ms = state.proxy_retry_base_delay_ms * 2u64.pow(attempt - 1)
                    + (rand::random::<u32>() % 100) as u64;
                tracing::warn!(
                    req_id,
                    service = service_name,
                    attempt,
                    delay_ms,
                    error = %err.message,
                    "AgentRaaS: retrying transient upstream failure (streaming route)"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                last_error = Some(err);
            }
        }
    }

    Err(last_error.expect("loop always sets last_error before exiting without returning Ok"))
}

/// Ports `route.url.replace(/{(\w+)}/g, (match, key) => process.env[key] ||
/// match)` — e.g. Twilio's path has `{TWILIO_SID}` in it, filled in from an
/// env var of the same name at request time. Left as the literal `{KEY}`
/// text if the env var isn't set, matching Node's `|| match` fallback.
fn substitute_env_placeholders(url: &str) -> String {
    let mut result = String::with_capacity(url.len());
    let bytes = url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = url[i + 1..].find('}') {
                let key = &url[i + 1..i + 1 + end];
                if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    if let Ok(val) = std::env::var(key) {
                        result.push_str(&val);
                    } else {
                        result.push_str(&url[i..=i + 1 + end]);
                    }
                    i += end + 2;
                    continue;
                }
            }
        }
        let ch = url[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }
    result
}

fn value_to_form_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Retry-with-backoff, circuit-breaker-aware in both directions: every
/// failed attempt records a real failure, and a retry is abandoned the
/// moment the breaker trips open mid-sequence.
#[allow(clippy::too_many_arguments)]
pub async fn forward_with_retry(
    state: &SharedState,
    route: &ResolvedRoute,
    service_name: &str,
    action_name: &str,
    org_id: &str,
    payload: &Value,
    req_id: &str,
    circuit_key: &str,
) -> Result<Value, ForwardError> {
    let mut last_error = None;

    for attempt in 1..=state.proxy_retry_max_attempts {
        match forward_action(state, route, service_name, action_name, org_id, payload, req_id).await {
            Ok(mut result) => {
                if attempt > 1 {
                    result["retried"] = json!(attempt - 1);
                }
                return Ok(result);
            }
            Err(mut err) => {
                err.circuit_already_recorded = true;
                if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
                    if let Ok(Some(transition)) = circuit_breaker::record_failure(&mut conn, circuit_key).await {
                        log_circuit_transition(state, transition).await;
                    }
                }

                let retryable = is_retryable(&err);
                let is_last_attempt = attempt == state.proxy_retry_max_attempts;

                if is_last_attempt || !retryable {
                    last_error = Some(err);
                    break;
                }

                let circuit_open = {
                    if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
                        circuit_breaker::get_circuit_state(&mut conn, circuit_key)
                            .await
                            .map(|(s, _)| s == "open")
                            .unwrap_or(false)
                    } else {
                        false
                    }
                };
                if circuit_open {
                    last_error = Some(err);
                    break;
                }

                let delay_ms = state.proxy_retry_base_delay_ms * 2u64.pow(attempt - 1)
                    + (rand::random::<u32>() % 100) as u64;
                tracing::warn!(
                    req_id,
                    service = service_name,
                    action = action_name,
                    attempt,
                    delay_ms,
                    error = %err.message,
                    "AgentRaaS: retrying transient upstream failure"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                last_error = Some(err);
            }
        }
    }

    Err(last_error.expect("loop always sets last_error before exiting without returning Ok"))
}

/// Multi-Destination Fan-Out (event broadcasting) — best-effort copies of
/// the same payload to every configured `fanout_urls` destination, after
/// the primary target_url call has already succeeded. Never affects the
/// primary response, the dedup outcome, or the audit log status.
pub fn broadcast_fanout(state: &SharedState, route: &ResolvedRoute, payload: &Value, req_id: &str) {
    for url in &route.fanout_urls {
        let client = state.http_client.clone();
        let payload = payload.clone();
        let req_id = req_id.to_string();
        let url = url.clone();
        tokio::spawn(async move {
            let result = client
                .post(&url)
                .header("X-AgentRaaS-ReqId", &req_id)
                .header("X-AgentRaaS-Fanout", "true")
                .json(&payload)
                .timeout(Duration::from_secs(10))
                .send()
                .await;
            if let Err(err) = result {
                tracing::warn!(url, req_id, error = %err, "fan-out broadcast failed (best-effort, not retried)");
            }
        });
    }
}

/// Retry-with-backoff counterpart to `forward_mcp_tool_call`, same shape as
/// `forward_with_retry` above (fast-follow noted when MCP Custom Actions
/// first shipped as a single-attempt-only v1 — this replaces that).
pub async fn forward_mcp_with_retry(
    state: &SharedState,
    route: &ResolvedMcpRoute,
    org_id: &str,
    payload: &Value,
    req_id: &str,
    circuit_key: &str,
) -> Result<Value, ForwardError> {
    let mut last_error = None;

    for attempt in 1..=state.proxy_retry_max_attempts {
        match forward_mcp_tool_call(state, route, org_id, payload, req_id).await {
            Ok(mut result) => {
                if attempt > 1 {
                    result["retried"] = json!(attempt - 1);
                }
                return Ok(result);
            }
            Err(mut err) => {
                err.circuit_already_recorded = true;
                if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
                    if let Ok(Some(transition)) = circuit_breaker::record_failure(&mut conn, circuit_key).await {
                        log_circuit_transition(state, transition).await;
                    }
                }

                let retryable = is_retryable(&err);
                let is_last_attempt = attempt == state.proxy_retry_max_attempts;

                if is_last_attempt || !retryable {
                    last_error = Some(err);
                    break;
                }

                let circuit_open = {
                    if let Ok(mut conn) = state.redis.get_multiplexed_async_connection().await {
                        circuit_breaker::get_circuit_state(&mut conn, circuit_key)
                            .await
                            .map(|(s, _)| s == "open")
                            .unwrap_or(false)
                    } else {
                        false
                    }
                };
                if circuit_open {
                    last_error = Some(err);
                    break;
                }

                let delay_ms = state.proxy_retry_base_delay_ms * 2u64.pow(attempt - 1)
                    + (rand::random::<u32>() % 100) as u64;
                tracing::warn!(
                    req_id,
                    tool = route.remote_tool_name,
                    attempt,
                    delay_ms,
                    error = %err.message,
                    "AgentRaaS: retrying transient MCP upstream failure"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                last_error = Some(err);
            }
        }
    }

    Err(last_error.expect("loop always sets last_error before exiting without returning Ok"))
}

pub async fn log_circuit_transition(state: &SharedState, transition: circuit_breaker::Transition) {
    if let Err(err) = sqlx::query(
        "INSERT INTO circuit_breaker_events (service, from_state, to_state) VALUES ($1, $2, $3)",
    )
    .bind(&transition.service)
    .bind(&transition.from_state)
    .bind(&transition.to_state)
    .execute(&state.pg)
    .await
    {
        tracing::warn!(?err, service = %transition.service, "circuit transition log failed");
    }
}
