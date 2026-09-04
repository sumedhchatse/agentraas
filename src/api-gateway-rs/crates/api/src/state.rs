use std::collections::HashMap;
use std::sync::Arc;

use agentraas_core::config::ServiceRoute;

use crate::email::Mailer;

pub struct AppState {
    pub pg: sqlx::PgPool,
    pub redis: redis::Client,
    pub service_routes: HashMap<String, ServiceRoute>,
    /// MCP tool name (`service_action`) -> (service_name, action_name).
    pub tool_name_to_route: HashMap<String, (String, String)>,
    /// Top-level service names from `config/services.json` — used to
    /// validate a curated (non-"custom:") credential's `service` field.
    pub curated_services: std::collections::HashSet<String>,
    /// Precomputed `tools/list` result — static for the process lifetime,
    /// same content Node rebuilds on every call via `flatMap`.
    pub mcp_tools_list: serde_json::Value,

    pub jwt_secret: String,
    pub public_url: String,
    pub deployment_mode: String,
    /// Mirrors Node's `NODE_ENV === 'production'` gate on the session
    /// cookie's `Secure` flag.
    pub is_production: bool,
    pub expose_dev_verify_url: bool,
    /// "Is the Enterprise tier actually enabled on this deployment" — a
    /// different axis from `deployment_mode` (cloud vs self-hosted).
    /// Community-tier behavior is unaffected either way.
    pub enterprise_mode: bool,
    pub dashboard_rate_limit_per_min: u32,
    pub agent_rate_limit_per_min: u32,
    pub agency_rate_limit_per_min: u32,
    pub cloud_monthly_limit: i64,
    pub agency_monthly_limit: i64,
    pub agency_max_client_tenants: i64,
    pub proxy_retry_max_attempts: u32,
    pub proxy_retry_base_delay_ms: u64,

    pub mailer: Mailer,
    pub token_bucket: agentraas_core::token_bucket::TokenBucket,
    pub http_client: reqwest::Client,
    pub cipher: agentraas_core::crypto::CredentialCipher,
}

pub type SharedState = Arc<AppState>;

/// A uniform JSON `{"error": "..."}` response with a status code, matching
/// every Node route's error shape exactly (`reply.status(N).send({error})`).
pub struct ApiError {
    pub status: axum::http::StatusCode,
    pub message: String,
    /// Optional extra top-level field some Node routes add alongside
    /// `error` (e.g. `{"error": "...", "code": "EMAIL_NOT_VERIFIED"}`).
    pub code: Option<String>,
    /// Further ad-hoc top-level fields some Node error responses add
    /// (e.g. `{"error": "...", "replayed": false, "reqId": "..."}`).
    pub extra: Vec<(String, serde_json::Value)>,
}

impl ApiError {
    pub fn new(status: axum::http::StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            extra: Vec::new(),
        }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn with_extra(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.extra.push((key.into(), value));
        self
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let mut body = serde_json::json!({ "error": self.message });
        if let Some(code) = self.code {
            body["code"] = serde_json::Value::String(code);
        }
        for (key, value) in self.extra {
            body[key] = value;
        }
        (self.status, axum::Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!(?err, "database error");
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "An internal error occurred.",
        )
    }
}

/// "Is the Enterprise tier actually enabled on this deployment" gate —
/// mirrors Node's `requireEnterpriseMode`, which lives directly in
/// `server.js` (not `src/ee`) so it's present in both editions; it just
/// always 403s in Community since there's nothing behind it to enable.
/// Kept outside the `ee` module for the same reason: the two admin/audit
/// routes that use it (`dashboard.rs`) exist in both editions too.
pub fn require_enterprise_mode(state: &SharedState) -> Result<(), ApiError> {
    if !state.enterprise_mode {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "This is an Enterprise-tier feature. Set ENTERPRISE_MODE=true (see compose.ee.yaml) to enable it.",
        ));
    }
    Ok(())
}

impl From<redis::RedisError> for ApiError {
    fn from(err: redis::RedisError) -> Self {
        tracing::error!(?err, "redis error");
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "An internal error occurred.",
        )
    }
}
