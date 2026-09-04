//! Remaining Phase 6 long-tail routes: public webhook-audit tool, demo
//! seed data, Paddle billing (checkout-info + webhook), and org branding.
//! Mirrors the corresponding sections of `server.js`.

use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::Mac;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::get_user_org_ids;
use crate::auth::{check_dashboard_rate_limit, check_login_rate_limit, AuthUser};
use crate::state::{ApiError, SharedState};
use crate::util::validate_target_url;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/tools/webhook-audit", post(webhook_audit_tool))
        .route("/api/v1/demo/seed", post(demo_seed))
        .route("/api/v1/billing/checkout-info", get(billing_checkout_info))
        .route("/api/v1/webhooks/paddle", post(paddle_webhook))
        .route("/api/v1/org-branding/:org_id", get(get_org_branding).put(put_org_branding))
}

// ─── Public webhook-audit tool (unauthenticated lead magnet) ───

#[derive(Deserialize)]
struct AuditBody {
    url: Option<String>,
}

#[derive(serde::Serialize)]
struct FireResult {
    attempt: u8,
    status: Option<u16>,
    latency_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn fire_one(client: &reqwest::Client, url: &str, payload: &Value, attempt: u8) -> FireResult {
    let start = std::time::Instant::now();
    match client.post(url).json(payload).timeout(Duration::from_secs(8)).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(500).collect();
            FireResult { attempt, status: Some(status), latency_ms: start.elapsed().as_millis(), body_snippet: Some(snippet), error: None }
        }
        Err(err) => FireResult { attempt, status: None, latency_ms: start.elapsed().as_millis(), body_snippet: None, error: Some(err.to_string()) },
    }
}

/// Fires 3 identical POSTs at a caller-supplied URL and reports whether
/// the responses look idempotent — an HTTP-level heuristic, not proof.
/// Same SSRF guard as custom-action registration, plus an IP rate limit
/// (reusing the login limiter), since this is unauthenticated and takes a
/// caller-supplied URL.
async fn webhook_audit_tool(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AuditBody>,
) -> Result<Json<Value>, ApiError> {
    let url = body.url.unwrap_or_default();
    if url.is_empty() || url.chars().count() > 2000 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "A webhook URL is required."));
    }
    let under_limit = check_login_rate_limit(&state.redis, &addr.ip().to_string(), "webhook-audit").await?;
    if !under_limit {
        return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many audits from this IP. Try again in 15 minutes."));
    }
    if let Some(err) = validate_target_url(&url).await {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    let mut test_id_bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut test_id_bytes);
    let test_id = hex::encode(test_id_bytes);
    let payload = json!({
        "agentraas_webhook_audit": true,
        "note": "Free idempotency test from AgentRaaS (agentraas.io/webhook-audit) — 3 identical requests fired in parallel to check whether this endpoint deduplicates retries. Safe to ignore or discard.",
        "test_id": test_id,
        "amount": 100,
        "currency": "usd",
    });

    let (r1, r2, r3) = tokio::join!(
        fire_one(&state.http_client, &url, &payload, 1),
        fire_one(&state.http_client, &url, &payload, 2),
        fire_one(&state.http_client, &url, &payload, 3),
    );
    let results = [r1, r2, r3];

    let any_errored = results.iter().any(|r| r.status.is_none());
    let all_succeeded = results.iter().all(|r| r.status.is_some_and(|s| s < 500));
    let bodies: Vec<String> = results.iter().map(|r| r.error.as_ref().map(|e| format!("__error:{e}")).unwrap_or_else(|| r.body_snippet.clone().unwrap_or_default())).collect();
    let all_identical = bodies.iter().all(|b| b == &bodies[0]);

    let (verdict, verdict_label) = if any_errored {
        ("inconclusive", "One or more requests didn't complete (network error or timeout) — try again, or double-check the URL.")
    } else if !all_succeeded {
        ("inconclusive", "The endpoint returned a server error, so duplicate-processing risk could not be determined from this run.")
    } else if all_identical {
        ("likely_safe", "All 3 identical requests got back the exact same response — consistent with idempotent handling.")
    } else {
        ("vulnerable", "The 3 identical requests got back 3 different responses — consistent with 3 separate records/charges being created.")
    };

    Ok(Json(json!({
        "url": url, "test_id": test_id, "verdict": verdict, "verdict_label": verdict_label, "results": results,
        "disclaimer": "This is an HTTP-level heuristic based only on the responses your endpoint sent back — we cannot see your database. \"Likely safe\" is not a guarantee; \"vulnerable\" is strong evidence, not certainty.",
    })))
}

// ─── Demo seed data ───

async fn demo_seed(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_admin(&state, user.sub).await?;

    let mut org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        let demo_org_id = format!("demo_{}", user.sub);
        let mut key_bytes = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut key_bytes);
        let dummy_key = format!("ar_demo_{}", hex::encode(key_bytes));
        let dummy_key_hash = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(dummy_key.as_bytes()))
        };
        sqlx::query("INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(user.sub)
            .bind(&demo_org_id)
            .bind("demo_agent")
            .bind("Demo data")
            .bind(&dummy_key_hash)
            .bind(&dummy_key[..16])
            .execute(&state.pg)
            .await?;
        org_ids = vec![demo_org_id];
    }

    let services: Vec<(String, Vec<String>)> = state
        .service_routes
        .keys()
        .filter_map(|k| k.split_once('.'))
        .fold(std::collections::HashMap::<String, Vec<String>>::new(), |mut acc, (svc, action)| {
            acc.entry(svc.to_string()).or_default().push(action.to_string());
            acc
        })
        .into_iter()
        .collect();
    const AGENTS: &[&str] = &["agent_invoice", "agent_booking", "agent_crm", "agent_payment", "agent_sms", "agent_whatsapp"];
    const STATUSES: &[&str] = &["success", "success", "success", "deduplicated", "blocked", "error"];

    // `rand::thread_rng()` is deliberately `!Send` (thread-local) — holding
    // it across the `.await` points in the loop below would make this
    // handler's future non-`Send`, which axum's `Handler` bound rejects.
    // `StdRng` is `Send`.
    let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::from_entropy();
    for i in 0..30 {
        let (svc, actions) = &services[rand::Rng::gen_range(&mut rng, 0..services.len())];
        let action = &actions[rand::Rng::gen_range(&mut rng, 0..actions.len())];
        let org = &org_ids[rand::Rng::gen_range(&mut rng, 0..org_ids.len())];
        let agent = AGENTS[rand::Rng::gen_range(&mut rng, 0..AGENTS.len())];
        let status = STATUSES[rand::Rng::gen_range(&mut rng, 0..STATUSES.len())];
        let error_type = if status == "blocked" { Some("validation_failed") } else if status == "error" { Some("upstream_timeout") } else { None };
        let hours_ago = rand::Rng::gen_range(&mut rng, 0..24);
        let duration_ms = rand::Rng::gen_range(&mut rng, 10..210);
        let mut req_id_bytes = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rng, &mut req_id_bytes);

        sqlx::query(
            "INSERT INTO audit_log (req_id,api_key,org_id,agent_id,service,action,status,error_type,duration_ms,payload_hash,created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, NOW() - ($11 || ' hours')::interval)",
        )
        .bind(format!("req_{}", hex::encode(req_id_bytes)))
        .bind("ak_demo")
        .bind(org)
        .bind(agent)
        .bind(svc)
        .bind(action)
        .bind(status)
        .bind(error_type)
        .bind(duration_ms as i64)
        .bind(format!("hash_{i}"))
        .bind(hours_ago.to_string())
        .execute(&state.pg)
        .await?;
    }

    let demo_org = &org_ids[0];

    sqlx::query(
        "INSERT INTO circuit_breaker_events (service, from_state, to_state, occurred_at) VALUES
         ('stripe', 'closed', 'open', NOW() - INTERVAL '20 hours'),
         ('stripe', 'open', 'half-open', NOW() - INTERVAL '19 hours 30 minutes'),
         ('stripe', 'half-open', 'closed', NOW() - INTERVAL '19 hours 25 minutes')",
    )
    .execute(&state.pg)
    .await?;

    let mut dlq1_bytes = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rng, &mut dlq1_bytes);
    sqlx::query(
        "INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7, NOW() - INTERVAL '3 hours')",
    )
    .bind(format!("req_{}", hex::encode(dlq1_bytes)))
    .bind(demo_org)
    .bind("agent_payment")
    .bind("stripe")
    .bind("charge.create")
    .bind(state.cipher.encrypt(&json!({ "amount": 4999, "currency": "usd", "customer": "cus_demo123" }).to_string()))
    .bind("Upstream returned 503: Service temporarily unavailable")
    .execute(&state.pg)
    .await?;

    let mut dlq2_bytes = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rng, &mut dlq2_bytes);
    sqlx::query(
        "INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7, NOW() - INTERVAL '1 hour')",
    )
    .bind(format!("req_{}", hex::encode(dlq2_bytes)))
    .bind(demo_org)
    .bind("agent_whatsapp")
    .bind("whatsapp")
    .bind("message.send")
    .bind(state.cipher.encrypt(&json!({ "to": "+14155552671", "body": "Your order has shipped!" }).to_string()))
    .bind("Upstream returned 500: Internal Server Error")
    .execute(&state.pg)
    .await?;

    sqlx::query(
        "INSERT INTO custom_actions (user_id, org_id, name, method, target_url, auth_type, content_type)
         VALUES ($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT (org_id, name) WHERE revoked_at IS NULL DO NOTHING",
    )
    .bind(user.sub)
    .bind(demo_org)
    .bind("internal-order-webhook")
    .bind("POST")
    .bind("https://httpbin.org/post")
    .bind("none")
    .bind("application/json")
    .execute(&state.pg)
    .await?;

    sqlx::query("INSERT INTO health_check_settings (org_id, service, enabled_by) VALUES ($1,$2,$3) ON CONFLICT (org_id, service) DO NOTHING")
        .bind(demo_org)
        .bind("mockpay")
        .bind(user.sub)
        .execute(&state.pg)
        .await?;

    Ok(Json(json!({ "seeded": 30, "circuit_events": 3, "dlq_entries": 2, "custom_actions": 1, "health_checks_enabled": 1 })))
}

async fn require_admin(state: &SharedState, user_id: i32) -> Result<(), ApiError> {
    let is_admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1").bind(user_id).fetch_optional(&state.pg).await?;
    if is_admin != Some(true) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Admin access required."));
    }
    Ok(())
}

// ─── Paddle billing (agency-tier self-serve upgrade) ───
// NOTE (same caveat as the Node original): exact field names/scheme
// below have not been verified against a live Paddle sandbox — this
// deployment has no PADDLE_* env vars configured, so both routes 503
// "not configured" here, same as Node's own fallback when unconfigured.

async fn billing_checkout_info(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let client_token = std::env::var("PADDLE_CLIENT_TOKEN").ok();
    let price_id = std::env::var("PADDLE_AGENCY_PRICE_ID").ok();
    let (Some(client_token), Some(price_id)) = (client_token, price_id) else {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Billing is not configured on this deployment."));
    };
    let row: Option<(String, String)> = sqlx::query_as("SELECT email, plan FROM users WHERE id = $1").bind(user.sub).fetch_optional(&state.pg).await?;
    let Some((email, current_plan)) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "User not found."));
    };
    let environment = if std::env::var("PADDLE_ENVIRONMENT").as_deref() == Ok("production") { "production" } else { "sandbox" };
    Ok(Json(json!({
        "client_token": client_token, "environment": environment, "price_id": price_id,
        "email": email, "current_plan": current_plan,
        "custom_data": { "user_id": user.sub, "plan": "agency" },
    })))
}

/// Paddle's webhook signature scheme: `Paddle-Signature: ts=<unix-seconds>;h1=<hex>`,
/// HMAC-SHA256 over `"{ts}:{rawBody}"`, hex-encoded — ported from the
/// `@paddle/paddle-node-sdk`'s own `WebhooksValidator` (5-second tolerance,
/// unusually tight but that's genuinely what the SDK enforces).
fn verify_paddle_signature(raw_body: &str, signature_header: &str, secret: &str) -> bool {
    let mut ts: Option<i64> = None;
    let mut h1: Option<&str> = None;
    for part in signature_header.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            match k {
                "ts" => ts = v.parse().ok(),
                "h1" => h1 = Some(v),
                _ => {}
            }
        }
    }
    let (Some(ts), Some(h1)) = (ts, h1) else { return false };
    let now_ms = chrono::Utc::now().timestamp_millis();
    if now_ms > (ts + 5) * 1000 {
        return false;
    }
    let payload = format!("{ts}:{raw_body}");
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    hmac::Mac::update(&mut mac, payload.as_bytes());
    let expected = hex::encode(hmac::Mac::finalize(mac).into_bytes());
    agentraas_core::timing_safe_equal_strings(h1, &expected)
}

async fn paddle_webhook(State(state): State<SharedState>, headers: HeaderMap, raw_body: axum::body::Bytes) -> Result<Json<Value>, ApiError> {
    let (Ok(api_key), Ok(webhook_secret)) = (std::env::var("PADDLE_API_KEY"), std::env::var("PADDLE_WEBHOOK_SECRET")) else {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Billing is not configured on this deployment."));
    };
    let _ = api_key;
    let Some(signature) = headers.get("paddle-signature").and_then(|v| v.to_str().ok()) else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Missing Paddle-Signature header."));
    };
    let raw_body_str = String::from_utf8_lossy(&raw_body).into_owned();
    if !verify_paddle_signature(&raw_body_str, signature, &webhook_secret) {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid signature."));
    }
    let event: Value = serde_json::from_str(&raw_body_str).map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Could not parse webhook event."))?;

    let event_id = event.get("event_id").or_else(|| event.get("eventId")).and_then(Value::as_str).unwrap_or_default().to_string();
    let event_type = event.get("event_type").or_else(|| event.get("eventType")).and_then(Value::as_str).unwrap_or_default().to_string();
    let sub = event.get("data").cloned().unwrap_or(Value::Null);

    let already: Option<i32> = sqlx::query_scalar("SELECT 1 FROM processed_webhook_events WHERE event_id = $1").bind(&event_id).fetch_optional(&state.pg).await?;
    if already.is_some() {
        return Ok(Json(json!({ "received": true })));
    }

    let custom_data = sub.get("customData").or_else(|| sub.get("custom_data"));
    let user_id = custom_data.and_then(|c| c.get("user_id")).and_then(Value::as_i64);
    let target_plan = custom_data.and_then(|c| c.get("plan")).and_then(Value::as_str).unwrap_or("agency").to_string();

    if ["subscription.created", "subscription.activated", "subscription.updated"].contains(&event_type.as_str()) {
        if let Some(user_id) = user_id {
            let status = sub.get("status").and_then(Value::as_str).unwrap_or_default().to_string();
            let period_end = sub
                .get("currentBillingPeriod")
                .or_else(|| sub.get("current_billing_period"))
                .and_then(|p| p.get("endsAt").or_else(|| p.get("ends_at")))
                .and_then(Value::as_str);
            let sub_id = sub.get("id").and_then(Value::as_str).unwrap_or_default();
            let customer_id = sub.get("customerId").or_else(|| sub.get("customer_id")).and_then(Value::as_str);

            sqlx::query(
                "INSERT INTO subscriptions (user_id, paddle_subscription_id, paddle_customer_id, status, current_period_end)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (paddle_subscription_id) DO UPDATE SET status = $4, current_period_end = $5, updated_at = NOW()",
            )
            .bind(user_id)
            .bind(sub_id)
            .bind(customer_id)
            .bind(&status)
            .bind(period_end)
            .execute(&state.pg)
            .await?;

            let plan = if ["active", "trialing"].contains(&status.as_str()) { target_plan.as_str() } else { "free" };
            sqlx::query("UPDATE users SET plan = $1 WHERE id = $2").bind(plan).bind(user_id).execute(&state.pg).await?;
        }
    } else if event_type == "subscription.canceled" && sub.is_object() {
        let sub_id = sub.get("id").and_then(Value::as_str).unwrap_or_default();
        sqlx::query("UPDATE subscriptions SET status = 'canceled', updated_at = NOW() WHERE paddle_subscription_id = $1").bind(sub_id).execute(&state.pg).await?;
        if let Some(user_id) = user_id {
            sqlx::query("UPDATE users SET plan = 'free' WHERE id = $1").bind(user_id).execute(&state.pg).await?;
        }
    }

    sqlx::query("INSERT INTO processed_webhook_events (event_id, source) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(&event_id)
        .bind("paddle")
        .execute(&state.pg)
        .await?;
    Ok(Json(json!({ "received": true })))
}

// ─── Org branding (Agency-tier white-label) ───

async fn get_org_branding(State(state): State<SharedState>, Path(org_id): Path<String>) -> Result<Json<Value>, ApiError> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as("SELECT display_name, logo_url FROM org_branding WHERE org_id = $1").bind(&org_id).fetch_optional(&state.pg).await?;
    match row {
        Some((display_name, logo_url)) => Ok(Json(json!({ "display_name": display_name, "logo_url": logo_url }))),
        None => Ok(Json(json!({ "display_name": null, "logo_url": null }))),
    }
}

#[derive(Deserialize)]
struct BrandingBody {
    display_name: Option<String>,
    logo_url: Option<Option<String>>,
}

async fn put_org_branding(State(state): State<SharedState>, user: AuthUser, Path(org_id): Path<String>, Json(body): Json<BrandingBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let owned_org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !owned_org_ids.iter().any(|id| id == &org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "You do not own this org."));
    }
    let plan: Option<String> = sqlx::query_scalar("SELECT plan FROM users WHERE id = $1").bind(user.sub).fetch_optional(&state.pg).await?;
    if plan.as_deref() != Some("agency") {
        return Err(ApiError::new(StatusCode::PAYMENT_REQUIRED, "White-label branding requires the Agency plan."));
    }
    if let Some(name) = &body.display_name {
        if name.chars().count() > 255 {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "display_name must be a string up to 255 characters."));
        }
    }
    let logo_url = body.logo_url.flatten();
    if let Some(url) = &logo_url {
        if let Some(err) = validate_target_url(url).await {
            return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("logo_url: {err}")));
        }
    }

    sqlx::query(
        "INSERT INTO org_branding (org_id, display_name, logo_url, updated_at) VALUES ($1, $2, $3, NOW())
         ON CONFLICT (org_id) DO UPDATE SET display_name = EXCLUDED.display_name, logo_url = EXCLUDED.logo_url, updated_at = NOW()",
    )
    .bind(&org_id)
    .bind(&body.display_name)
    .bind(&logo_url)
    .execute(&state.pg)
    .await?;
    Ok(Json(json!({ "updated": true })))
}

