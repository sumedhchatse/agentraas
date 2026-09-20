//! CLI local dev tunnel (SPEC-TUNNEL.md) — an ngrok-style relay so a
//! developer can receive a real inbound webhook (Stripe, WhatsApp, GitHub,
//! etc.) on their `localhost` instance while building, without deploying
//! anywhere first. Open to every tier (this is for evaluation/dev, not a
//! paid feature) — abuse controls below are load-bearing because of that,
//! not optional polish.
//!
//! Simpler than the original README note assumed ("a separate hosted
//! relay service") — it isn't one. Axum's own WebSocket support plus an
//! in-memory registry in this same process is enough, since production is
//! a single instance today (see SPEC-TUNNEL.md §6 for the explicit,
//! deferred multi-instance note).
//!
//! Flow: CLI opens `GET /api/v1/tunnel/connect/:org_id/:agent_id` (agent
//! API key auth, same as every other agent connection) and holds it open.
//! An external caller hits the public `ANY /t/:tunnel_id/*rest` path; that
//! request gets framed as JSON and pushed down the held WebSocket; the CLI
//! forwards it to `localhost:<port>` and sends the response back the same
//! way, correlated by a per-request id.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;

use crate::agent::db::{get_user_org_ids, verify_api_key};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

const TUNNEL_TTL_SECONDS: u64 = 2 * 60 * 60;
const RESPONSE_TIMEOUT_SECONDS: u64 = 30;
/// Fixed-window request cap on relayed traffic through one tunnel — the
/// thing that stops this being usable as a general-purpose anonymous
/// proxy. Same windowed Redis INCR+EXPIRE shape spend_caps.rs already
/// uses, just a simpler fixed default instead of a configurable rule.
const RELAY_RATE_LIMIT_PER_MIN: i64 = 120;

pub struct TunnelHandle {
    pub org_id: String,
    to_cli: tokio::sync::mpsc::UnboundedSender<Message>,
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>,
    inspector_tx: tokio::sync::broadcast::Sender<Value>,
    /// Signals `handle_socket`'s select loop to stop and actually close
    /// the connection — removing an entry from the registry alone does
    /// NOT do this (a real bug caught in testing: the old task kept
    /// running, socket open, just unreachable via the registry, until its
    /// 2-hour expiry). `Mutex<Option<...>>` since a oneshot sender is
    /// consumed on send and this may be taken at most once.
    kill_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

pub type TunnelRegistry = Mutex<HashMap<String, Arc<TunnelHandle>>>;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/tunnel/connect/:org_id/:agent_id", get(connect))
        .route("/api/v1/tunnel/stream", get(stream))
        .route("/t/:tunnel_id/*rest", any(forward))
        // A tunnel opened at the bare public root (no sub-path) — same
        // handler, empty rest.
        .route("/t/:tunnel_id", any(forward_root))
}

fn generate_tunnel_id() -> String {
    let mut buf = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    format!("tn_{}", hex::encode(buf))
}

fn generate_correlation_id() -> String {
    let mut buf = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    format!("tr_{}", hex::encode(buf))
}

// ─── connect (CLI side) ───

async fn connect(
    State(state): State<SharedState>,
    Path((org_id, agent_id)): Path<(String, String)>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !is_valid_identifier(&org_id) || !is_valid_identifier(&agent_id) {
        return (StatusCode::BAD_REQUEST, "org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only.").into_response();
    }
    let api_key = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    match verify_api_key(&state.pg, &api_key, &org_id, &agent_id).await {
        Ok(v) if v.ok => {}
        Ok(_) => return (StatusCode::UNAUTHORIZED, "Invalid or missing API key for this agent.").into_response(),
        Err(err) => {
            tracing::error!(?err, "tunnel connect: verify_api_key failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "An internal error occurred.").into_response();
        }
    }

    // One active tunnel per org — opening a new one closes any existing
    // one for the same org (same "reuse or replace" shape as
    // re-registering an agent key).
    close_existing_tunnel_for_org(&state, &org_id);

    let tunnel_id = generate_tunnel_id();
    let public_url = format!("{}/t/{}", state.public_url, tunnel_id);

    ws.on_upgrade(move |socket| handle_socket(socket, state, tunnel_id, org_id, public_url))
}

fn close_existing_tunnel_for_org(state: &SharedState, org_id: &str) {
    let mut registry = state.tunnels.lock().unwrap();
    let existing_id = registry.iter().find(|(_, h)| h.org_id == org_id).map(|(id, _)| id.clone());
    if let Some(id) = existing_id {
        if let Some(handle) = registry.remove(&id) {
            if let Some(kill_tx) = handle.kill_tx.lock().unwrap().take() {
                let _ = kill_tx.send(());
            }
        }
    }
}

async fn handle_socket(socket: WebSocket, state: SharedState, tunnel_id: String, org_id: String, public_url: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (to_cli_tx, mut to_cli_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let (inspector_tx, _) = tokio::sync::broadcast::channel(32);
    let (kill_tx, mut kill_rx) = tokio::sync::oneshot::channel::<()>();

    let handle = Arc::new(TunnelHandle { org_id, to_cli: to_cli_tx, pending: Mutex::new(HashMap::new()), inspector_tx, kill_tx: Mutex::new(Some(kill_tx)) });
    state.tunnels.lock().unwrap().insert(tunnel_id.clone(), handle.clone());
    tracing::info!(tunnel_id, "tunnel connected");

    if ws_tx.send(Message::Text(json!({"type": "connected", "url": public_url}).to_string())).await.is_err() {
        state.tunnels.lock().unwrap().remove(&tunnel_id);
        return;
    }

    let forward_task = tokio::spawn(async move {
        while let Some(msg) = to_cli_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let expiry = tokio::time::sleep(Duration::from_secs(TUNNEL_TTL_SECONDS));
    tokio::pin!(expiry);

    loop {
        tokio::select! {
            _ = &mut expiry => break,
            _ = &mut kill_rx => break,
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(val) = serde_json::from_str::<Value>(&text) else { continue };
                        let Some(cid) = val.get("correlation_id").and_then(Value::as_str) else { continue };
                        let sender = handle.pending.lock().unwrap().remove(cid);
                        if let Some(sender) = sender {
                            let _ = sender.send(val);
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    // Dropping forward_task's JoinHandle via abort() drops its owned
    // ws_tx (closing that half); ws_rx drops when this function returns,
    // closing the other half - together this actually terminates the
    // WebSocket, not just removes it from the registry.
    forward_task.abort();
    state.tunnels.lock().unwrap().remove(&tunnel_id);
    tracing::info!(tunnel_id, "tunnel closed");
}

// ─── forward (public side) ───

async fn forward_root(state: State<SharedState>, path: Path<String>, method: axum::http::Method, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    forward(state, Path((path.0, String::new())), method, headers, body).await
}

async fn forward(
    State(state): State<SharedState>,
    Path((tunnel_id, rest)): Path<(String, String)>,
    method: axum::http::Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let handle = { state.tunnels.lock().unwrap().get(&tunnel_id).cloned() };
    let Some(handle) = handle else {
        return (StatusCode::NOT_FOUND, "Tunnel not found, expired, or the CLI disconnected.").into_response();
    };

    if !check_tunnel_rate_limit(&state, &tunnel_id).await {
        return (StatusCode::TOO_MANY_REQUESTS, "Too many requests through this tunnel — try again in a minute.").into_response();
    }

    let correlation_id = generate_correlation_id();
    let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
    handle.pending.lock().unwrap().insert(correlation_id.clone(), tx);

    let header_map: serde_json::Map<String, Value> =
        headers.iter().filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), json!(v)))).collect();
    let envelope = json!({
        "type": "request",
        "correlation_id": correlation_id,
        "method": method.to_string(),
        "path": format!("/{rest}"),
        "headers": header_map,
        "body_base64": base64::engine::general_purpose::STANDARD.encode(&body),
    });

    if handle.to_cli.send(Message::Text(envelope.to_string())).is_err() {
        handle.pending.lock().unwrap().remove(&correlation_id);
        return (StatusCode::BAD_GATEWAY, "Tunnel's CLI is disconnected.").into_response();
    }

    let response_val = match tokio::time::timeout(Duration::from_secs(RESPONSE_TIMEOUT_SECONDS), rx).await {
        Ok(Ok(v)) => v,
        _ => {
            handle.pending.lock().unwrap().remove(&correlation_id);
            return (StatusCode::GATEWAY_TIMEOUT, "No response from the local agent within 30s.").into_response();
        }
    };

    let status = response_val.get("status").and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()).unwrap_or(502);
    let resp_body = response_val
        .get("body_base64")
        .and_then(Value::as_str)
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .unwrap_or_default();
    let content_type = response_val.get("content_type").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string();

    let _ = handle.inspector_tx.send(json!({
        "method": method.to_string(), "path": format!("/{rest}"), "status": status,
        "at": chrono::Utc::now().to_rfc3339(),
    }));

    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    (status_code, [(axum::http::header::CONTENT_TYPE, content_type)], resp_body).into_response()
}

async fn check_tunnel_rate_limit(state: &SharedState, tunnel_id: &str) -> bool {
    let Ok(mut conn) = state.redis_conn_result() else { return true };
    let key = format!("tunnelrate:{tunnel_id}:{}", chrono::Utc::now().format("%Y-%m-%dT%H:%M"));
    let count: i64 = match redis::cmd("INCR").arg(&key).query_async(&mut conn).await {
        Ok(c) => c,
        Err(_) => return true,
    };
    if count == 1 {
        let _: Result<(), _> = redis::cmd("EXPIRE").arg(&key).arg(90).query_async(&mut conn).await;
    }
    count <= RELAY_RATE_LIMIT_PER_MIN
}

// ─── request inspector (dashboard SSE) ───
// ponytail: live-forward only, no replay-on-connect of past requests for
// this tunnel (SPEC-TUNNEL.md §3 mentioned an in-memory "last 50" buffer;
// simplified to a plain broadcast with no history, since a v1 dev tool
// opened right after firing a test webhook covers the real use case).
// Add a ring buffer per TunnelHandle if replay-on-connect turns out to
// matter in practice.

async fn stream(
    State(state): State<SharedState>,
    user: AuthUser,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_id = q.get("org_id").cloned().unwrap_or_default();
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if !org_ids.contains(&org_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Not your org."));
    }
    let handle = { state.tunnels.lock().unwrap().values().find(|h| h.org_id == org_id).cloned() };
    let Some(handle) = handle else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "No active tunnel for this org."));
    };
    let rx = handle.inspector_tx.subscribe();
    let stream = BroadcastStream::new(rx)
        .filter_map(|r| async move { r.ok() })
        .map(|val| Ok(Event::default().data(val.to_string())));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)).text("keepalive")))
}
