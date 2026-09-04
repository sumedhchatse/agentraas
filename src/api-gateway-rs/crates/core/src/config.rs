//! Loads `config/services.json` and builds the flat `service.action -> route`
//! map, mirroring `src/api-gateway/config-loader.js` exactly — including its
//! one deliberate subtlety: `authHeader` distinguishes "key absent from the
//! JSON" (default to `"Authorization"`) from "key present with value null"
//! (genuinely no auth header, e.g. zapier — the URL itself carries the
//! secret). A naive `Option<String>` can't tell those apart; see
//! `deserialize_present_option` below.

use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Deserializes a JSON field as `Some(Option<T>)` when the key is present
/// (`Some(None)` for an explicit `null`, `Some(Some(v))` for a value), so the
/// caller can distinguish "absent" (`None`) from "present but null" — the
/// same distinction Node gets for free from `hasOwnProperty`.
fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawServiceConfig {
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(
        rename = "authHeader",
        default,
        deserialize_with = "deserialize_present_option"
    )]
    pub auth_header: Option<Option<String>>,
    #[serde(rename = "authType")]
    pub auth_type: Option<String>,
    #[serde(rename = "contentType")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub internal: Option<bool>,
    #[serde(rename = "extraHeaders")]
    pub extra_headers: Option<serde_json::Value>,
    pub actions: HashMap<String, RawActionConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawActionConfig {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub validation: serde_json::Value,
}

/// One resolved `service.action` route — the Rust equivalent of a
/// `SERVICE_ROUTES[routeKey]` entry built by `buildServiceRoutes()`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceRoute {
    pub method: String,
    pub url: String,
    pub internal: bool,
    pub auth_type: String,
    /// `None` = no auth header should be added at all (explicit `null` in
    /// config); `Some(name)` = add this header.
    pub auth_header: Option<String>,
    pub content_type: String,
    pub extra_headers: Option<serde_json::Value>,
    pub validation: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config JSON: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("service {0}: no actions defined")]
    NoActions(String),
}

pub type ServicesConfig = HashMap<String, RawServiceConfig>;

/// Mirrors `loadConfig()` — reads `AGENTRAAS_CONFIG_PATH` or falls back to
/// `config/services.json` relative to the repo root, and applies the same
/// "every service needs actions" sanity check.
pub fn load_config(default_path: &Path) -> Result<ServicesConfig, ConfigError> {
    let path = std::env::var("AGENTRAAS_CONFIG_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| default_path.to_path_buf());

    if !path.exists() {
        return Err(ConfigError::NotFound(path.display().to_string()));
    }

    let raw = std::fs::read_to_string(&path)?;
    let config: ServicesConfig = serde_json::from_str(&raw)?;

    for (service_name, service_config) in &config {
        if service_config.actions.is_empty() {
            return Err(ConfigError::NoActions(service_name.clone()));
        }
    }

    Ok(config)
}

/// Mirrors `buildServiceRoutes()` — flattens `{service: {actions: {...}}}`
/// into a `"service.action" -> ServiceRoute` map.
pub fn build_service_routes(config: &ServicesConfig) -> HashMap<String, ServiceRoute> {
    let mut routes = HashMap::new();

    for (service_name, service_config) in config {
        for (action_name, action_config) in &service_config.actions {
            let route_key = format!("{service_name}.{action_name}");

            // Same precedence as the JS: key absent -> default to
            // "Authorization"; key present (even as null) -> use exactly
            // what was given.
            let auth_header = match &service_config.auth_header {
                None => Some("Authorization".to_string()),
                Some(inner) => inner.clone(),
            };

            routes.insert(
                route_key,
                ServiceRoute {
                    method: action_config.method.clone(),
                    url: format!("{}{}", service_config.base_url, action_config.path),
                    internal: service_config.internal.unwrap_or(false),
                    auth_type: service_config
                        .auth_type
                        .clone()
                        .unwrap_or_else(|| "bearer".to_string()),
                    auth_header,
                    content_type: service_config
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "application/json".to_string()),
                    extra_headers: service_config.extra_headers.clone(),
                    validation: action_config.validation.clone(),
                },
            );
        }
    }

    routes
}
