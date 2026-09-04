mod agent;
mod auth;
mod credentials;
mod custom_actions;
mod dashboard;
mod dlq;
#[cfg(feature = "enterprise")]
mod ee;
mod email;
mod health_checks;
mod long_tail;
mod mcp;
mod notifications;
mod pages;
mod rules;
mod self_host;
mod state;
mod util;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

use agentraas_core::config::{build_service_routes, load_config};
use email::Mailer;
use state::{AppState, SharedState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| anyhow::anyhow!("DATABASE_URL must be set"))?;
    let redis_url =
        std::env::var("REDIS_URL").map_err(|_| anyhow::anyhow!("REDIS_URL must be set"))?;
    let jwt_secret =
        std::env::var("JWT_SECRET").map_err(|_| anyhow::anyhow!("JWT_SECRET must be set"))?;
    let credentials_key = std::env::var("CREDENTIALS_ENCRYPTION_KEY")
        .map_err(|_| anyhow::anyhow!("CREDENTIALS_ENCRYPTION_KEY must be set"))?;
    let cipher = agentraas_core::crypto::CredentialCipher::from_base64_key(&credentials_key)
        .map_err(|e| anyhow::anyhow!("invalid CREDENTIALS_ENCRYPTION_KEY: {e}"))?;

    let pg = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;

    let redis = redis::Client::open(redis_url)?;
    {
        let mut conn = redis.get_multiplexed_async_connection().await?;
        let _: String = redis::cmd("PING").query_async(&mut conn).await?;
    }

    let default_config_path = PathBuf::from("/config/services.json");
    let services_config = load_config(&default_config_path)
        .map_err(|e| anyhow::anyhow!("failed to load services config: {e}"))?;
    let service_routes = build_service_routes(&services_config);
    tracing::info!(count = service_routes.len(), "loaded service routes from config");

    // MCP tool naming: `${svcName}_${actName.replace(/\./g,'_')}` -> route,
    // mirroring src/core/mcp/index.js's TOOL_NAME_TO_ROUTE, built once at
    // startup here too (Node rebuilds tools/list's array on every call, but
    // the content never changes for a given config, so precomputing both is
    // an equivalent, safe optimization).
    let mut tool_name_to_route = std::collections::HashMap::new();
    let mut tools = Vec::new();
    for (svc_name, svc) in &services_config {
        for act_name in svc.actions.keys() {
            let tool_name = format!("{svc_name}_{}", act_name.replace('.', "_"));
            tool_name_to_route.insert(tool_name.clone(), (svc_name.clone(), act_name.clone()));
            tools.push(serde_json::json!({
                "name": tool_name,
                "description": format!("AgentRaaS-protected {svc_name} {act_name}"),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "payload": { "type": "object", "description": "Request payload" },
                        "org_id": { "type": "string", "description": "Organization ID" },
                        "idempotency_key": { "type": "string", "description": "Optional — dedupe on this key instead of the exact payload bytes, so you control what counts as a retry of the same operation. Reusing the key with a genuinely different payload is rejected (not silently applied), matching Stripe-style idempotency keys." },
                    },
                    "required": ["payload"],
                },
            }));
        }
    }
    let mcp_tools_list = serde_json::json!(tools);
    let curated_services: std::collections::HashSet<String> = services_config.keys().cloned().collect();

    let node_env = std::env::var("NODE_ENV").unwrap_or_else(|_| "development".to_string());

    let state: SharedState = Arc::new(AppState {
        pg,
        redis,
        service_routes,
        tool_name_to_route,
        mcp_tools_list,
        curated_services,
        jwt_secret,
        public_url: std::env::var("PUBLIC_URL")
            .unwrap_or_else(|_| "http://localhost:13001".to_string()),
        deployment_mode: std::env::var("DEPLOYMENT_MODE").unwrap_or_else(|_| "cloud".to_string()),
        is_production: node_env == "production",
        expose_dev_verify_url: std::env::var("EXPOSE_DEV_VERIFY_URL").as_deref() == Ok("true"),
        enterprise_mode: std::env::var("ENTERPRISE_MODE").as_deref() == Ok("true"),
        dashboard_rate_limit_per_min: std::env::var("DASHBOARD_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
        agent_rate_limit_per_min: std::env::var("AGENT_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
        agency_rate_limit_per_min: std::env::var("AGENCY_RATE_LIMIT_PER_MIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2000),
        cloud_monthly_limit: std::env::var("CLOUD_MONTHLY_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500),
        agency_monthly_limit: std::env::var("AGENCY_MONTHLY_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(50000),
        agency_max_client_tenants: std::env::var("AGENCY_MAX_CLIENT_TENANTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        proxy_retry_max_attempts: std::env::var("PROXY_RETRY_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3),
        proxy_retry_base_delay_ms: std::env::var("PROXY_RETRY_BASE_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
        mailer: Mailer::from_env(),
        token_bucket: agentraas_core::token_bucket::TokenBucket::new(),
        http_client: reqwest::Client::builder()
            .build()
            .expect("building the shared HTTP client should never fail"),
        cipher,
    });

    health_checks::spawn_health_check_loop(state.clone());
    tokio::task::spawn_blocking(self_host::build_snapshot);

    let app = Router::new()
        .route("/health", get(health))
        .merge(auth::routes::router())
        .merge(agent::router())
        .merge(mcp::router())
        .merge(rules::router())
        .merge(credentials::router())
        .merge(custom_actions::router())
        .merge(notifications::router())
        .merge(health_checks::router())
        .merge(dlq::router())
        .merge(dashboard::router())
        .merge(pages::router())
        .merge(self_host::router())
        .merge(long_tail::router());
    #[cfg(feature = "enterprise")]
    let app = app.merge(ee::sso::router()).merge(ee::maintenance::router()).merge(ee::inbound_webhooks::router());
    let app = app.with_state(state);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "ar-api-rs listening");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

async fn health(State(state): State<SharedState>) -> impl IntoResponse {
    let db_ok = sqlx::query("SELECT 1").execute(&state.pg).await.is_ok();
    let redis_ok = match state.redis.get_multiplexed_async_connection().await {
        Ok(mut conn) => redis::cmd("PING")
            .query_async::<_, String>(&mut conn)
            .await
            .is_ok(),
        Err(_) => false,
    };

    let status = if db_ok && redis_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(json!({ "ok": db_ok && redis_ok, "postgres": db_ok, "redis": redis_ok })),
    )
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
