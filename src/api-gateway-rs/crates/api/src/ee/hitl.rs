//! Stateful Human-in-the-Loop (HITL) Gateway (Enterprise) — freezes a
//! webhook call matching a configured rule, posts an interactive Slack
//! approval card, and only forwards for real on approval. See
//! `handle_request` in `crate::agent::mod` for the freeze point and
//! `infra/migrations/036_hitl_gateway.sql` for the schema.
//!
//! The dedup slot the caller already claimed is deliberately left pending
//! on freeze (never completed/released here) — a concurrent identical
//! retry naturally gets the existing 409 "already being processed"
//! response. Approval completes that same slot with the real result;
//! denial releases it.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use agentraas_core::hmac_verify::timing_safe_equal_strings;
use agentraas_core::{dedup, hmac_verify};

use crate::agent::db::{check_org_write_permission, current_month_key, get_user_org_ids, log_audit, require_tier};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

const DEFAULT_MONTHLY_LIMIT: i64 = 10;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/hitl-rules", post(create_rule).get(list_rules))
        .route("/api/v1/hitl-rules/:id", axum::routing::delete(delete_rule))
        .route("/api/v1/hitl-slack-config", post(put_slack_config).get(get_slack_config))
        .route("/api/v1/hitl-escalation-config", post(put_escalation_config).get(get_escalation_config))
        .route("/v1/hitl/:req_id", get(get_status))
        .route("/v1/hitl/interactions/:org_id", post(interaction))
}

// ─── rule matching (called from handle_request) ───

pub struct MatchedRule;

/// A request matches if ANY rule row for this org+service+action either
/// has no field/operator/threshold (always require approval), or the
/// named payload field numerically satisfies the comparison.
pub async fn match_rule(pg: &sqlx::PgPool, org_id: &str, service: &str, action: &str, payload: &Value) -> Result<Option<MatchedRule>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        field: Option<String>,
        operator: Option<String>,
        threshold: Option<f64>,
    }
    let rows: Vec<Row> = sqlx::query_as("SELECT field, operator, threshold FROM hitl_rules WHERE org_id = $1 AND service = $2 AND action = $3")
        .bind(org_id)
        .bind(service)
        .bind(action)
        .fetch_all(pg)
        .await?;

    for row in rows {
        let (Some(field), Some(operator), Some(threshold)) = (&row.field, &row.operator, row.threshold) else {
            return Ok(Some(MatchedRule));
        };
        let Some(value) = payload.get(field).and_then(Value::as_f64) else {
            continue;
        };
        let matched = match operator.as_str() {
            "gt" => value > threshold,
            "gte" => value >= threshold,
            "lt" => value < threshold,
            "lte" => value <= threshold,
            "eq" => value == threshold,
            _ => false,
        };
        if matched {
            return Ok(Some(MatchedRule));
        }
    }
    Ok(None)
}

fn hitl_usage_key(org_id: &str) -> String {
    format!("hitl_usage:{org_id}:{}", current_month_key())
}

async fn monthly_limit(pg: &sqlx::PgPool, org_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT monthly_limit FROM org_hitl_limit_overrides WHERE org_id = $1")
        .bind(org_id)
        .fetch_optional(pg)
        .await
        .ok()
        .flatten()
        .unwrap_or(DEFAULT_MONTHLY_LIMIT)
}

/// Freezes the request: quota check, inserts the pending row, posts the
/// Slack approval card (if configured), and returns 202 immediately.
/// Fails closed on quota exhaustion or Slack misconfiguration — nothing
/// executes without an explicit approval.
#[allow(clippy::too_many_arguments)]
pub async fn freeze_and_notify(
    state: &SharedState,
    req_id: &str,
    org_id: &str,
    agent_id: &str,
    api_key: &str,
    service: &str,
    action: &str,
    payload: &Value,
    dedup_hash: &str,
    dedup_ttl_seconds: Option<i64>,
    _rule: MatchedRule,
    run_id: Option<&str>,
    step_id: Option<&str>,
) -> (StatusCode, Json<Value>) {
    let limit = monthly_limit(&state.pg, org_id).await;
    let usage_key = hitl_usage_key(org_id);
    let count: i64 = match state.redis_conn_result() {
        Ok(mut conn) => redis::cmd("INCR").arg(&usage_key).query_async(&mut conn).await.unwrap_or(0),
        Err(_) => 0,
    };
    if count == 1 {
        if let Ok(mut conn) = state.redis_conn_result() {
            let _: Result<(), _> = redis::cmd("EXPIRE").arg(&usage_key).arg(60 * 60 * 24 * 31).query_async(&mut conn).await;
        }
    }
    if count > limit {
        log_audit(&state.pg, req_id, api_key, org_id, agent_id, service, action, "blocked", Some("hitl_quota_exceeded"), 0, Some(dedup_hash), state.enterprise_mode, None, run_id, step_id, None).await;
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({ "error": "hitl_quota_exceeded", "message": format!("Monthly HITL approval limit reached ({count}/{limit}). Contact hello@agentraas.io if you need more."), "reqId": req_id })),
        );
    }

    if let Err(err) = sqlx::query(
        "INSERT INTO hitl_requests (req_id, org_id, agent_id, api_key, service, action, payload, dedup_hash, dedup_ttl_seconds, status, run_id, step_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending', $10, $11)",
    )
    .bind(req_id)
    .bind(org_id)
    .bind(agent_id)
    .bind(crate::agent::db::stored_key_ref(api_key))
    .bind(service)
    .bind(action)
    .bind(payload)
    .bind(dedup_hash)
    .bind(dedup_ttl_seconds)
    .bind(run_id)
    .bind(step_id)
    .execute(&state.pg)
    .await
    {
        tracing::error!(?err, "hitl: failed to insert pending request");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "An internal error occurred." })));
    }

    // Waiting on a human can take hours: drop the forward lease so a retry
    // doesn't read this as a claimer that died mid-call.
    if let Ok(mut conn) = state.redis_conn_result() {
        let _ = dedup::hold_for_approval(&mut conn, &format!("dedup:{dedup_hash}")).await;
    }
    post_slack_card(state, org_id, req_id, service, action, payload, run_id).await;

    log_audit(&state.pg, req_id, api_key, org_id, agent_id, service, action, "pending_approval", None, 0, Some(dedup_hash), state.enterprise_mode, None, run_id, step_id, None).await;
    (
        StatusCode::ACCEPTED,
        Json(json!({ "pending_approval": true, "reqId": req_id, "status_url": format!("{}/v1/hitl/{}", state.public_url, req_id) })),
    )
}

async fn slack_config(pg: &sqlx::PgPool, org_id: &str) -> Option<(String, String, String)> {
    #[derive(sqlx::FromRow)]
    struct Row {
        encrypted_bot_token: String,
        encrypted_signing_secret: String,
        default_channel: String,
    }
    let row: Row = sqlx::query_as("SELECT encrypted_bot_token, encrypted_signing_secret, default_channel FROM hitl_slack_config WHERE org_id = $1")
        .bind(org_id)
        .fetch_optional(pg)
        .await
        .ok()??;
    Some((row.encrypted_bot_token, row.encrypted_signing_secret, row.default_channel))
}

/// Posts the Approve/Deny card. Best-effort: if the org has no Slack
/// config yet, the request just sits pending with no way to approve it in
/// this MVP (documented limitation, not the golden path) — still fails
/// closed since nothing executes either way.
/// A step under approval that's part of a multi-step run is otherwise
/// shown with zero context about what already happened in that run — an
/// approver can't tell "this is step 4, steps 1-3 already succeeded"
/// from "this is a one-off call" without checking the dashboard
/// separately. Best-effort: an empty string (no prior steps, or the
/// lookup itself fails) just means no run-context line gets added.
async fn run_context_line(pg: &sqlx::PgPool, run_id: &str, current_req_id: &str) -> String {
    #[derive(sqlx::FromRow)]
    struct StepRow {
        step_id: Option<String>,
        status: String,
    }
    let rows: Vec<StepRow> = sqlx::query_as(
        "SELECT step_id, status FROM audit_log WHERE run_id = $1 AND req_id != $2 ORDER BY created_at ASC LIMIT 20",
    )
    .bind(run_id)
    .bind(current_req_id)
    .fetch_all(pg)
    .await
    .unwrap_or_default();
    if rows.is_empty() {
        return String::new();
    }
    let steps: Vec<String> = rows
        .iter()
        .map(|r| {
            let icon = if r.status == "success" { "✅" } else { "⚠️" };
            format!("{icon} {}", r.step_id.as_deref().unwrap_or("(unnamed step)"))
        })
        .collect();
    format!("\n_Run `{run_id}` so far:_ {}", steps.join(", "))
}

async fn post_slack_card(state: &SharedState, org_id: &str, req_id: &str, service: &str, action: &str, payload: &Value, run_id: Option<&str>) {
    let Some((encrypted_bot_token, _, channel)) = slack_config(&state.pg, org_id).await else {
        return;
    };
    let Ok(bot_token) = state.cipher.decrypt(&encrypted_bot_token) else {
        return;
    };
    let run_context = match run_id {
        Some(rid) => run_context_line(&state.pg, rid, req_id).await,
        None => String::new(),
    };
    let text = format!("*HITL approval requested*\n`{service}.{action}` for org `{org_id}`\n```{}```{run_context}", serde_json::to_string_pretty(payload).unwrap_or_default());
    let body = json!({
        "channel": channel,
        "text": text,
        "blocks": [
            { "type": "section", "text": { "type": "mrkdwn", "text": text } },
            {
                "type": "actions",
                "elements": [
                    { "type": "button", "text": { "type": "plain_text", "text": "Approve" }, "style": "primary", "action_id": "hitl_approve", "value": req_id },
                    { "type": "button", "text": { "type": "plain_text", "text": "Deny" }, "style": "danger", "action_id": "hitl_deny", "value": req_id },
                ],
            },
        ],
    });
    let sent = state
        .http_client
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(&bot_token)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
    let Ok(resp) = sent else {
        tracing::error!(org_id, req_id, "hitl: slack chat.postMessage request failed");
        return;
    };
    if let Ok(parsed) = resp.json::<Value>().await {
        let ts = parsed.get("ts").and_then(Value::as_str);
        let _ = sqlx::query("UPDATE hitl_requests SET slack_channel = $1, slack_message_ts = $2 WHERE req_id = $3")
            .bind(&channel)
            .bind(ts)
            .bind(req_id)
            .execute(&state.pg)
            .await;
    }
}

// ─── SLA tracking + auto-escalation ───

const ESCALATION_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(sqlx::FromRow)]
struct EscalationCandidate {
    req_id: String,
    org_id: String,
    service: String,
    action: String,
    payload: Value,
    escalation_channel: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// Pending requests past their org's configured SLA that haven't already
/// been escalated. A join, not two queries — `hitl_escalation_config` has
/// no row for an org that never opted in, so those orgs' requests simply
/// never match (default-off, same "no row" convention as
/// `org_hitl_limit_overrides`).
async fn find_escalation_candidates(pg: &sqlx::PgPool) -> Vec<EscalationCandidate> {
    sqlx::query_as::<_, EscalationCandidate>(
        "SELECT r.req_id, r.org_id, r.service, r.action, r.payload, c.escalation_channel, r.created_at
         FROM hitl_requests r
         JOIN hitl_escalation_config c ON c.org_id = r.org_id
         WHERE r.status = 'pending' AND r.escalated_at IS NULL
           AND r.created_at < NOW() - (c.sla_minutes || ' minutes')::interval",
    )
    .fetch_all(pg)
    .await
    .unwrap_or_default()
}

/// Second notification for the same req_id — reuses the same
/// `hitl_approve`/`hitl_deny` action_ids as the original card, so
/// `interaction` (which looks a request up by req_id + org_id, not by
/// which message the click came from) needs no changes to handle a click
/// from this card instead of the original.
async fn post_escalation_card(state: &SharedState, org_id: &str, channel: &str, req_id: &str, service: &str, action: &str, payload: &Value, minutes_overdue: i64) {
    let Some((encrypted_bot_token, _, _)) = slack_config(&state.pg, org_id).await else {
        return;
    };
    let Ok(bot_token) = state.cipher.decrypt(&encrypted_bot_token) else {
        return;
    };
    let text = format!(
        "🚨 *HITL approval SLA breached*\n`{service}.{action}` for org `{org_id}` has been pending {minutes_overdue}+ minutes with no response.\n```{}```",
        serde_json::to_string_pretty(payload).unwrap_or_default()
    );
    let body = json!({
        "channel": channel,
        "text": text,
        "blocks": [
            { "type": "section", "text": { "type": "mrkdwn", "text": text } },
            {
                "type": "actions",
                "elements": [
                    { "type": "button", "text": { "type": "plain_text", "text": "Approve" }, "style": "primary", "action_id": "hitl_approve", "value": req_id },
                    { "type": "button", "text": { "type": "plain_text", "text": "Deny" }, "style": "danger", "action_id": "hitl_deny", "value": req_id },
                ],
            },
        ],
    });
    let sent = state.http_client.post("https://slack.com/api/chat.postMessage").bearer_auth(&bot_token).json(&body).timeout(std::time::Duration::from_secs(10)).send().await;
    if sent.is_err() {
        tracing::error!(org_id, req_id, "hitl: escalation slack chat.postMessage request failed");
    }
}

async fn run_hitl_escalation_check(state: &SharedState) {
    for c in find_escalation_candidates(&state.pg).await {
        let minutes_overdue = (chrono::Utc::now() - c.created_at).num_minutes();
        post_escalation_card(state, &c.org_id, &c.escalation_channel, &c.req_id, &c.service, &c.action, &c.payload, minutes_overdue).await;
        // Marked escalated regardless of whether the Slack post actually
        // succeeded — a fixed-interval loop retrying the same stuck
        // request forever on persistent Slack failure would just spam the
        // channel once it recovers; one escalation attempt per breach is
        // the deliberate v1 behavior (see plan doc for the explicit
        // no-auto-deny scope cut this pairs with).
        let _ = sqlx::query("UPDATE hitl_requests SET escalated_at = NOW() WHERE req_id = $1").bind(&c.req_id).execute(&state.pg).await;
    }
}

/// Background loop — spawned once at startup, mirrors
/// `health_checks::spawn_health_check_loop`.
pub fn spawn_hitl_escalation_loop(state: SharedState) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ESCALATION_CHECK_INTERVAL);
        loop {
            interval.tick().await;
            run_hitl_escalation_check(&state).await;
        }
    });
}

// ─── status poll ───

async fn get_status(State(state): State<SharedState>, Path(req_id): Path<String>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    // Same bearer key that made the original (now-frozen) request — req_id
    // alone isn't a secret (it's returned in the freeze response, logged,
    // and visible in the Slack message to the whole approval channel), so
    // it can't be the only thing standing between an outsider and the
    // result of another org's approved action.
    let provided_key = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    #[derive(sqlx::FromRow)]
    struct Row {
        api_key: String,
        status: String,
        result: Option<Value>,
        error_message: Option<String>,
    }
    let row: Option<Row> = sqlx::query_as("SELECT api_key, status, result, error_message FROM hitl_requests WHERE req_id = $1")
        .bind(&req_id)
        .fetch_optional(&state.pg)
        .await?;
    let Some(row) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not found."));
    };
    // Hash both sides to a fixed-length digest before the constant-time
    // compare — `timing_safe_equal_strings` itself short-circuits on a
    // length mismatch, which would otherwise leak whether a guessed
    // key's length happens to match the real one.
    let sha256_hex = |s: &str| {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(s.as_bytes()))
    };
    // The row holds `stored_key_ref` (prefix + SHA-256), never the raw key.
    let stored_hash = crate::agent::db::stored_key_hash(&row.api_key).map(str::to_string).unwrap_or_else(|| sha256_hex(&row.api_key));
    if provided_key.is_empty() || !timing_safe_equal_strings(&sha256_hex(provided_key), &stored_hash) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not found."));
    }
    Ok(Json(json!({ "reqId": req_id, "status": row.status, "result": row.result, "error": row.error_message })))
}

// ─── Slack interaction receiver ───

/// Slack's `interactive_components` payload arrives as
/// `application/x-www-form-urlencoded` with a single `payload` field
/// holding the actual JSON.
async fn interaction(State(state): State<SharedState>, Path(org_id): Path<String>, headers: HeaderMap, raw_body: Bytes) -> Result<Json<Value>, ApiError> {
    let Some((_, encrypted_signing_secret, _)) = slack_config(&state.pg, &org_id).await else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not found."));
    };
    let signing_secret = state.cipher.decrypt(&encrypted_signing_secret).map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred."))?;

    let raw_body_str = String::from_utf8_lossy(&raw_body).into_owned();
    let header_val = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let result = hmac_verify::verify_slack(&raw_body_str, header_val("x-slack-signature"), header_val("x-slack-request-timestamp"), &signing_secret, 300);
    if !result.valid {
        tracing::warn!(org_id, reason = result.reason.as_deref().unwrap_or("signature mismatch"), "hitl: slack interaction signature verification failed");
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid signature."));
    }

    let form = url::form_urlencoded::parse(raw_body.as_ref()).into_owned().collect::<std::collections::HashMap<String, String>>();
    let Some(payload_json) = form.get("payload").and_then(|p| serde_json::from_str::<Value>(p).ok()) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Malformed interaction payload."));
    };
    let response_url = payload_json.get("response_url").and_then(Value::as_str).map(str::to_string);
    let action = payload_json.get("actions").and_then(|a| a.get(0));
    let (action_id, req_id) = match action {
        Some(a) => (a.get("action_id").and_then(Value::as_str).unwrap_or(""), a.get("value").and_then(Value::as_str).unwrap_or("")),
        None => ("", ""),
    };
    if req_id.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Missing request id in interaction payload."));
    }
    let resolved_by = payload_json.get("user").and_then(|u| u.get("username")).and_then(Value::as_str).unwrap_or("slack_user").to_string();

    match action_id {
        "hitl_approve" => {
            let state = state.clone();
            let req_id = req_id.to_string();
            // Slack requires an ack within 3s; the real forward can take
            // longer (retries, upstream latency), so it runs in the
            // background and updates the message via response_url after.
            tokio::spawn(async move {
                approve(&state, &req_id, &org_id, resolved_by, response_url).await;
            });
            Ok(Json(json!({ "text": "Approving…" })))
        }
        "hitl_deny" => {
            deny(&state, req_id, &org_id, resolved_by, response_url).await;
            Ok(Json(json!({ "text": "Denied." })))
        }
        other => Err(ApiError::new(StatusCode::BAD_REQUEST, format!("Unknown action_id: {other}"))),
    }
}

#[derive(sqlx::FromRow)]
struct PendingRow {
    org_id: String,
    agent_id: String,
    api_key: String,
    service: String,
    action: String,
    payload: Value,
    dedup_hash: String,
    dedup_ttl_seconds: Option<i64>,
    status: String,
    run_id: Option<String>,
    step_id: Option<String>,
}

/// Read-only lookup, scoped to the org whose Slack signature was just
/// verified — never by `req_id` alone, so a validly-signed interaction
/// from one org's own Slack app can never even see another org's request.
/// Used only to give a precise "not found" vs "already resolved" message;
/// the actual authorization + concurrency gate is `claim_pending` below.
async fn load_pending(pg: &sqlx::PgPool, req_id: &str, org_id: &str) -> Option<PendingRow> {
    sqlx::query_as::<_, PendingRow>(
        "SELECT org_id, agent_id, api_key, service, action, payload, dedup_hash, dedup_ttl_seconds, status, run_id, step_id FROM hitl_requests WHERE req_id = $1 AND org_id = $2",
    )
    .bind(req_id)
    .bind(org_id)
    .fetch_optional(pg)
    .await
    .ok()
    .flatten()
}

/// Atomically transitions a pending row to `claimed_status`, scoped to the
/// verified org. This is the real concurrency gate: of two simultaneous
/// Approve clicks (a genuine double-click, or a replayed/forged
/// interaction racing the first), only one `UPDATE ... WHERE status =
/// 'pending'` can match, so the underlying action is never forwarded
/// twice. Also closes the same org-scoping gap as `load_pending`.
async fn claim_pending(pg: &sqlx::PgPool, req_id: &str, org_id: &str, claimed_status: &str) -> Option<PendingRow> {
    sqlx::query_as::<_, PendingRow>(
        "UPDATE hitl_requests SET status = $1 WHERE req_id = $2 AND org_id = $3 AND status = 'pending'
         RETURNING org_id, agent_id, api_key, service, action, payload, dedup_hash, dedup_ttl_seconds, status, run_id, step_id",
    )
    .bind(claimed_status)
    .bind(req_id)
    .bind(org_id)
    .fetch_optional(pg)
    .await
    .ok()
    .flatten()
}

async fn patch_slack_message(state: &SharedState, response_url: Option<String>, text: &str) {
    let Some(url) = response_url else { return };
    // The payload is signature-checked, but with the org's own signing
    // secret: an org admin could forge one pointing anywhere. Slack's
    // response_url is always on hooks.slack.com.
    if !is_slack_response_url(&url) {
        tracing::warn!("ignoring a Slack response_url that isn't https://hooks.slack.com/");
        return;
    }
    let _ = state.http_client.post(&url).json(&json!({ "text": text, "replace_original": true })).timeout(std::time::Duration::from_secs(10)).send().await;
}

async fn approve(state: &SharedState, req_id: &str, org_id: &str, resolved_by: String, response_url: Option<String>) {
    let Some(seen) = load_pending(&state.pg, req_id, org_id).await else {
        patch_slack_message(state, response_url, "Request not found.").await;
        return;
    };
    if seen.status != "pending" {
        patch_slack_message(state, response_url, &format!("Already {}.", seen.status)).await;
        return;
    }
    let Some(row) = claim_pending(&state.pg, req_id, org_id, "approving").await else {
        patch_slack_message(state, response_url, "Already being processed.").await;
        return;
    };

    // The org may have revoked this key between freeze and approval —
    // don't let a stale key still execute just because a human clicked
    // Approve before the revocation propagated.
    match crate::agent::db::verify_stored_key_ref(&state.pg, &row.api_key, &row.org_id, &row.agent_id).await {
        Ok(true) => {}
        _ => {
            fail_request(state, req_id, &row, "The API key that made this request has since been revoked.").await;
            patch_slack_message(state, response_url, "Approval failed: the originating API key has since been revoked.").await;
            return;
        }
    }

    let resolved_route = match crate::agent::resolve_route(state, &row.org_id, &row.service, &row.action, req_id).await {
        Ok(r) => r,
        Err(_) => {
            fail_request(state, req_id, &row, "Route no longer resolves.").await;
            patch_slack_message(state, response_url, "Approval failed: route no longer resolves.").await;
            return;
        }
    };
    let circuit_key = if resolved_route.credential_key.is_empty() { row.service.clone() } else { resolved_route.credential_key.clone() };

    match crate::agent::forward::forward_with_retry(state, &resolved_route, &row.service, &row.action, &row.org_id, &row.payload, req_id, &circuit_key, None, Some(&dedup::upstream_idempotency_key(&row.dedup_hash))).await {
        Ok(result) => {
            if let Ok(mut conn) = state.redis_conn_result() {
                let _ = dedup::complete_dedup_slot_with_ttl(&mut conn, &format!("dedup:{}", row.dedup_hash), &result, row.dedup_ttl_seconds).await;
            }
            let _ = sqlx::query("UPDATE hitl_requests SET status = 'approved', result = $1, resolved_by = $2, resolved_at = NOW() WHERE req_id = $3")
                .bind(&result)
                .bind(&resolved_by)
                .bind(req_id)
                .execute(&state.pg)
                .await;
            crate::action_policies::remember_destinations(state, &row.org_id, &row.service, &row.action, &row.payload).await;
            log_audit(&state.pg, req_id, &row.api_key, &row.org_id, &row.agent_id, &row.service, &row.action, "success", None, 0, Some(&row.dedup_hash), state.enterprise_mode, None, row.run_id.as_deref(), row.step_id.as_deref(), None).await;
            patch_slack_message(state, response_url, &format!("✅ Approved by {resolved_by} and executed.")).await;
        }
        Err(err) => {
            if let Ok(mut conn) = state.redis_conn_result() {
                let key = format!("dedup:{}", row.dedup_hash);
                if err.outcome_unknown {
                    let _ = dedup::mark_dedup_slot_unknown(&mut conn, &key, req_id, row.dedup_ttl_seconds).await;
                } else {
                    let _ = dedup::release_dedup_slot(&mut conn, &key).await;
                }
            }
            fail_request(state, req_id, &row, &err.message).await;
            patch_slack_message(state, response_url, &format!("⚠️ Approved by {resolved_by}, but the forward failed: {}", err.message)).await;
        }
    }
}

async fn fail_request(state: &SharedState, req_id: &str, row: &PendingRow, error_message: &str) {
    let _ = sqlx::query("UPDATE hitl_requests SET status = 'failed', error_message = $1, resolved_at = NOW() WHERE req_id = $2")
        .bind(error_message)
        .bind(req_id)
        .execute(&state.pg)
        .await;
    log_audit(&state.pg, req_id, &row.api_key, &row.org_id, &row.agent_id, &row.service, &row.action, "blocked", Some("hitl_forward_failed"), 0, Some(&row.dedup_hash), state.enterprise_mode, None, row.run_id.as_deref(), row.step_id.as_deref(), None).await;
}

async fn deny(state: &SharedState, req_id: &str, org_id: &str, resolved_by: String, response_url: Option<String>) {
    let Some(seen) = load_pending(&state.pg, req_id, org_id).await else {
        patch_slack_message(state, response_url, "Request not found.").await;
        return;
    };
    if seen.status != "pending" {
        patch_slack_message(state, response_url, &format!("Already {}.", seen.status)).await;
        return;
    }
    let Some(row) = claim_pending(&state.pg, req_id, org_id, "denied").await else {
        patch_slack_message(state, response_url, "Already being processed.").await;
        return;
    };
    if let Ok(mut conn) = state.redis_conn_result() {
        let _ = dedup::release_dedup_slot(&mut conn, &format!("dedup:{}", row.dedup_hash)).await;
    }
    let _ = sqlx::query("UPDATE hitl_requests SET status = 'denied', resolved_by = $1, resolved_at = NOW() WHERE req_id = $2")
        .bind(&resolved_by)
        .bind(req_id)
        .execute(&state.pg)
        .await;
    log_audit(&state.pg, req_id, &row.api_key, &row.org_id, &row.agent_id, &row.service, &row.action, "blocked", Some("hitl_denied"), 0, Some(&row.dedup_hash), state.enterprise_mode, None, row.run_id.as_deref(), row.step_id.as_deref(), None).await;
    patch_slack_message(state, response_url, &format!("❌ Denied by {resolved_by}.")).await;
}

// ─── dashboard CRUD ───

#[derive(Deserialize)]
struct CreateRuleBody {
    org_id: String,
    service: String,
    action: String,
    field: Option<String>,
    operator: Option<String>,
    threshold: Option<f64>,
}

const OPERATORS: &[&str] = &["gt", "gte", "lt", "lte", "eq"];

async fn create_rule(State(state): State<SharedState>, user: AuthUser, Json(body): Json<CreateRuleBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    // HITL moved from Enterprise-only to Pro+ (was previously gated by
    // `require_enterprise_mode` — the whole server's on/off switch,
    // independent of the calling org's actual plan). Enterprise-mode
    // deployments (this binary always is, per SPEC.md §3) still require
    // the org to be at least Pro to use it.
    require_tier(&state, &body.org_id, agentraas_core::tier::Tier::Team).await?;
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if body.service.is_empty() || body.action.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "service and action are required."));
    }
    if let Some(op) = &body.operator {
        if !OPERATORS.contains(&op.as_str()) {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("operator must be one of: {}", OPERATORS.join(", "))));
        }
    }
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO hitl_rules (org_id, service, action, field, operator, threshold, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(&body.org_id)
    .bind(&body.service)
    .bind(&body.action)
    .bind(&body.field)
    .bind(&body.operator)
    .bind(body.threshold)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;
    Ok(Json(json!({ "id": id, "saved": true })))
}

#[derive(Deserialize)]
struct OrgQuery {
    org_id: String,
}

async fn list_rules(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    // No tier gate here (unlike create_rule/put_slack_config): this spans
    // every org the user belongs to, so there's no single org to check a
    // minimum tier against — and a downgraded org should still be able to
    // see/clean up rules it made while on Pro. Access itself stays scoped
    // by get_user_org_ids below, same as before.
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "rules": [] })));
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'org_id', org_id, 'service', service, 'action', action, 'field', field, 'operator', operator, 'threshold', threshold)
         FROM hitl_rules WHERE org_id = ANY($1) ORDER BY created_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "rules": rows })))
}

async fn delete_rule(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    // No tier gate — same reasoning as list_rules above.
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "HITL rule not found."));
    }
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM hitl_rules WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "HITL rule not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct SlackConfigBody {
    org_id: String,
    bot_token: String,
    signing_secret: String,
    default_channel: String,
}

async fn put_slack_config(State(state): State<SharedState>, user: AuthUser, Json(body): Json<SlackConfigBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    require_tier(&state, &body.org_id, agentraas_core::tier::Tier::Team).await?;
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if body.bot_token.len() < 8 || body.signing_secret.len() < 8 || body.default_channel.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "bot_token, signing_secret, and default_channel are all required."));
    }
    let encrypted_bot_token = state.cipher.encrypt(&body.bot_token);
    let encrypted_signing_secret = state.cipher.encrypt(&body.signing_secret);
    sqlx::query(
        "INSERT INTO hitl_slack_config (org_id, encrypted_bot_token, encrypted_signing_secret, default_channel) VALUES ($1, $2, $3, $4)
         ON CONFLICT (org_id) DO UPDATE SET encrypted_bot_token = EXCLUDED.encrypted_bot_token, encrypted_signing_secret = EXCLUDED.encrypted_signing_secret, default_channel = EXCLUDED.default_channel, updated_at = NOW()",
    )
    .bind(&body.org_id)
    .bind(&encrypted_bot_token)
    .bind(&encrypted_signing_secret)
    .bind(&body.default_channel)
    .execute(&state.pg)
    .await?;
    Ok(Json(json!({ "saved": true })))
}

async fn get_slack_config(State(state): State<SharedState>, user: AuthUser, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    // No tier gate — a read of config status, same reasoning as list_rules.
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !org_ids.contains(&q.org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    let channel: Option<String> = sqlx::query_scalar("SELECT default_channel FROM hitl_slack_config WHERE org_id = $1").bind(&q.org_id).fetch_optional(&state.pg).await?;
    Ok(Json(json!({ "configured": channel.is_some(), "default_channel": channel })))
}

#[derive(Deserialize)]
struct EscalationConfigBody {
    org_id: String,
    sla_minutes: Option<i32>,
    escalation_channel: String,
}

async fn put_escalation_config(State(state): State<SharedState>, user: AuthUser, Json(body): Json<EscalationConfigBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    require_tier(&state, &body.org_id, agentraas_core::tier::Tier::Team).await?;
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let sla_minutes = body.sla_minutes.unwrap_or(60);
    if !(1..=10080).contains(&sla_minutes) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "sla_minutes must be between 1 and 10080 (one week)."));
    }
    if body.escalation_channel.is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "escalation_channel is required."));
    }
    sqlx::query(
        "INSERT INTO hitl_escalation_config (org_id, sla_minutes, escalation_channel) VALUES ($1, $2, $3)
         ON CONFLICT (org_id) DO UPDATE SET sla_minutes = EXCLUDED.sla_minutes, escalation_channel = EXCLUDED.escalation_channel, updated_at = NOW()",
    )
    .bind(&body.org_id)
    .bind(sla_minutes)
    .bind(&body.escalation_channel)
    .execute(&state.pg)
    .await?;
    Ok(Json(json!({ "saved": true })))
}

async fn get_escalation_config(State(state): State<SharedState>, user: AuthUser, Query(q): Query<OrgQuery>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !org_ids.contains(&q.org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not a member of this org."));
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        sla_minutes: i32,
        escalation_channel: String,
    }
    let row: Option<Row> = sqlx::query_as("SELECT sla_minutes, escalation_channel FROM hitl_escalation_config WHERE org_id = $1").bind(&q.org_id).fetch_optional(&state.pg).await?;
    Ok(Json(json!({
        "configured": row.is_some(),
        "sla_minutes": row.as_ref().map(|r| r.sla_minutes),
        "escalation_channel": row.as_ref().map(|r| r.escalation_channel.clone()),
    })))
}

fn is_slack_response_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("hooks.slack.com") && u.port().is_none())
}

#[cfg(test)]
mod response_url_tests {
    #[test]
    fn only_slack_hooks_are_called_back() {
        assert!(super::is_slack_response_url("https://hooks.slack.com/actions/T1/123/abc"));
        assert!(!super::is_slack_response_url("http://hooks.slack.com/actions/x"));
        assert!(!super::is_slack_response_url("https://hooks.slack.com.evil.test/x"));
        assert!(!super::is_slack_response_url("https://hooks.slack.com:8443/x"));
        assert!(!super::is_slack_response_url("https://169.254.169.254/latest/meta-data"));
    }
}
