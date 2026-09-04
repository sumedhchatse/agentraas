//! Ports `forwardAction`/`forwardWithRetry`/`isRetryableError` from
//! `src/core/proxy/index.js` — the actual outbound call to a curated
//! service or custom action, with circuit-breaker-aware retry.

use std::time::Duration;

use agentraas_core::circuit_breaker;
use serde_json::{json, Value};

use super::db::{extract_upstream_error_message, get_credential, ResolvedRoute};
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

pub async fn forward_action(
    state: &SharedState,
    route: &ResolvedRoute,
    service_name: &str,
    action_name: &str,
    org_id: &str,
    payload: &Value,
    req_id: &str,
) -> Result<Value, ForwardError> {
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
