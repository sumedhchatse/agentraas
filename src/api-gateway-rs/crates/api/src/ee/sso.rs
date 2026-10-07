//! Enterprise SSO: `/api/v1/auth/sso/*` and `/api/v1/auth/invites/accept`.
//! OIDC only; SAML is out of scope.
//!
//! Hand-rolled OIDC client rather than a crate: discovery is a single GET
//! of `/.well-known/openid-configuration`, PKCE is SHA-256 + base64url,
//! and ID-token verification reuses `jsonwebtoken` (already a dependency
//! for session cookies) against the IdP's JWKS. Kept deliberately simple —
//! **RS256 only** (the default for every mainstream IdP: Okta, Azure AD,
//! Google Workspace) — an unsupported `alg` fails closed with a clear
//! error rather than silently accepting something weaker.
//!
//! `sso.test.js` covers config CRUD, membership/RBAC, invites and the two
//! claim-mapping functions; the login → callback round trip needs a real
//! IdP and isn't covered by a test.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use axum_extra::extract::cookie::{Cookie, SameSite};
use axum_extra::extract::CookieJar;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::require_tier;
use crate::auth::{check_dashboard_rate_limit, hash_password, is_valid_email, is_valid_password, session_cookie, sign_session, AuthUser};
use crate::state::{ApiError, SharedState};

use super::{require_enterprise_mode, require_org_admin, upsert_membership, ROLE_VALUES};

const SSO_FLOW_COOKIE: &str = "ar_sso_flow";
const INVITE_TTL_DAYS: i64 = 7;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/auth/sso/:org_id/configs", get(list_configs).post(create_config))
        .route("/api/v1/auth/sso/:org_id/configs/:config_id", put(update_config).delete(delete_config))
        .route("/api/v1/auth/sso/:org_id/login", get(login))
        .route("/api/v1/auth/sso/callback", get(callback))
        .route("/api/v1/auth/sso/:org_id/members", get(list_members))
        .route("/api/v1/auth/sso/:org_id/members/:user_id", put(update_member).delete(delete_member))
        .route("/api/v1/auth/sso/:org_id/invites", post(create_invite).get(list_invites))
        .route("/api/v1/auth/sso/:org_id/invites/:id", delete(delete_invite))
        .route("/api/v1/auth/invites/accept", post(accept_invite))
}

fn mask_secret(plaintext: &str) -> String {
    if plaintext.is_empty() {
        return "••••".to_string();
    }
    if plaintext.chars().count() > 8 {
        let chars: Vec<char> = plaintext.chars().collect();
        let first4: String = chars[..4].iter().collect();
        let last4: String = chars[chars.len() - 4..].iter().collect();
        format!("{first4}••••{last4}")
    } else {
        "••••".to_string()
    }
}

#[derive(Deserialize)]
struct SsoConfigBody {
    issuer_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    allowed_domains: Option<String>,
    default_role: Option<String>,
    enabled: Option<bool>,
}

fn validate_sso_config_body(body: &SsoConfigBody) -> Option<&'static str> {
    let Some(issuer_url) = &body.issuer_url else { return Some("issuer_url must be a valid https:// URL.") };
    let Ok(parsed) = url::Url::parse(issuer_url) else { return Some("issuer_url must be a valid https:// URL.") };
    if parsed.scheme() != "https" {
        return Some("issuer_url must be a valid https:// URL.");
    }
    if body.client_id.as_deref().unwrap_or("").is_empty() {
        return Some("client_id is required.");
    }
    if body.client_secret.as_deref().unwrap_or("").is_empty() {
        return Some("client_secret is required.");
    }
    if body.allowed_domains.as_deref().unwrap_or("").is_empty() {
        return Some("allowed_domains is required (comma-separated email domains).");
    }
    if let Some(role) = &body.default_role {
        if !ROLE_VALUES.contains(&role.as_str()) {
            return Some("default_role must be one of: admin, developer, auditor");
        }
    }
    None
}

async fn list_configs(State(state): State<SharedState>, user: AuthUser, Path(org_id): Path<String>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    require_enterprise_mode(&state)?;

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        issuer_url: String,
        client_id: String,
        encrypted_client_secret: String,
        allowed_domains: String,
        default_role: String,
        enabled: bool,
    }
    let rows: Vec<Row> = sqlx::query_as("SELECT id, org_id, issuer_url, client_id, encrypted_client_secret, allowed_domains, default_role, enabled FROM sso_configs WHERE org_id = $1 ORDER BY id ASC")
        .bind(&org_id)
        .fetch_all(&state.pg)
        .await?;
    let configs: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let secret = state.cipher.decrypt(&r.encrypted_client_secret).unwrap_or_default();
            json!({
                "id": r.id, "org_id": r.org_id, "issuer_url": r.issuer_url, "client_id": r.client_id,
                "client_secret_preview": mask_secret(&secret), "allowed_domains": r.allowed_domains,
                "default_role": r.default_role, "enabled": r.enabled,
            })
        })
        .collect();
    Ok(Json(json!({ "configs": configs })))
}

async fn create_config(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(body): Json<SsoConfigBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    require_enterprise_mode(&state)?;
    if let Some(err) = validate_sso_config_body(&body) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    sqlx::query("INSERT INTO orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING").bind(&org_id).execute(&state.pg).await?;

    let encrypted = state.cipher.encrypt(body.client_secret.as_deref().unwrap_or(""));
    let default_role = body.default_role.unwrap_or_else(|| "developer".to_string());
    let enabled = body.enabled != Some(false);
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO sso_configs (org_id, issuer_url, client_id, encrypted_client_secret, allowed_domains, default_role, enabled, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NOW()) RETURNING id",
    )
    .bind(&org_id)
    .bind(body.issuer_url.as_deref().unwrap_or_default())
    .bind(body.client_id.as_deref().unwrap_or_default())
    .bind(&encrypted)
    .bind(body.allowed_domains.as_deref().unwrap_or_default())
    .bind(&default_role)
    .bind(enabled)
    .fetch_one(&state.pg)
    .await?;

    Ok(Json(json!({ "created": true, "id": id })))
}

async fn update_config(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((org_id, config_id)): Path<(String, i32)>,
    Json(body): Json<SsoConfigBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    require_enterprise_mode(&state)?;

    let existing_org_id: Option<String> = sqlx::query_scalar("SELECT org_id FROM sso_configs WHERE id = $1").bind(config_id).fetch_optional(&state.pg).await?;
    if existing_org_id.as_deref() != Some(org_id.as_str()) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "No such SSO configuration for this org."));
    }
    if let Some(err) = validate_sso_config_body(&body) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    let encrypted = state.cipher.encrypt(body.client_secret.as_deref().unwrap_or(""));
    let default_role = body.default_role.unwrap_or_else(|| "developer".to_string());
    let enabled = body.enabled != Some(false);
    sqlx::query(
        "UPDATE sso_configs SET issuer_url = $2, client_id = $3, encrypted_client_secret = $4,
           allowed_domains = $5, default_role = $6, enabled = $7, updated_at = NOW()
         WHERE id = $1",
    )
    .bind(config_id)
    .bind(body.issuer_url.as_deref().unwrap_or_default())
    .bind(body.client_id.as_deref().unwrap_or_default())
    .bind(&encrypted)
    .bind(body.allowed_domains.as_deref().unwrap_or_default())
    .bind(&default_role)
    .bind(enabled)
    .execute(&state.pg)
    .await?;

    Ok(Json(json!({ "updated": true })))
}

async fn delete_config(State(state): State<SharedState>, user: AuthUser, Path((org_id, config_id)): Path<(String, i32)>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    require_enterprise_mode(&state)?;
    let existing_org_id: Option<String> = sqlx::query_scalar("SELECT org_id FROM sso_configs WHERE id = $1").bind(config_id).fetch_optional(&state.pg).await?;
    if existing_org_id.as_deref() != Some(org_id.as_str()) {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "No such SSO configuration for this org."));
    }
    sqlx::query("DELETE FROM sso_configs WHERE id = $1").bind(config_id).execute(&state.pg).await?;
    Ok(Json(json!({ "deleted": true })))
}

// ─── OIDC login / callback ───

struct DiscoveredConfig {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    issuer: String,
}

async fn discover(http_client: &reqwest::Client, issuer_url: &str) -> Result<DiscoveredConfig, String> {
    let url = format!("{}/.well-known/openid-configuration", issuer_url.trim_end_matches('/'));
    // The issuer is typed in by an org admin, and the discovery document it
    // returns names the other URLs the server will call: all of them go
    // through the same SSRF guard as every other outbound URL.
    if let Some(err) = crate::util::validate_target_url(&url).await {
        return Err(format!("issuer_url: {err}"));
    }
    let resp: Value = http_client.get(&url).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    let get = |k: &str| resp.get(k).and_then(Value::as_str).map(String::from);
    let discovered = DiscoveredConfig {
        authorization_endpoint: get("authorization_endpoint").ok_or("missing authorization_endpoint")?,
        token_endpoint: get("token_endpoint").ok_or("missing token_endpoint")?,
        jwks_uri: get("jwks_uri").ok_or("missing jwks_uri")?,
        issuer: get("issuer").ok_or("missing issuer")?,
    };
    for (name, u) in [("token_endpoint", &discovered.token_endpoint), ("jwks_uri", &discovered.jwks_uri)] {
        if let Some(err) = crate::util::validate_target_url(u).await {
            return Err(format!("{name}: {err}"));
        }
    }
    Ok(discovered)
}

fn random_urlsafe(len_bytes: usize) -> String {
    use base64::Engine;
    let mut buf = vec![0u8; len_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

struct ResolvedConfig {
    id: i32,
    org_id: String,
    issuer_url: String,
    client_id: String,
    client_secret: String,
    allowed_domains: String,
    default_role: String,
    enabled: bool,
}

async fn get_config_by_id(state: &SharedState, config_id: i32) -> Result<Option<ResolvedConfig>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        issuer_url: String,
        client_id: String,
        encrypted_client_secret: String,
        allowed_domains: String,
        default_role: String,
        enabled: bool,
    }
    let row: Option<Row> = sqlx::query_as("SELECT id, org_id, issuer_url, client_id, encrypted_client_secret, allowed_domains, default_role, enabled FROM sso_configs WHERE id = $1")
        .bind(config_id)
        .fetch_optional(&state.pg)
        .await?;
    Ok(row.map(|r| ResolvedConfig {
        id: r.id,
        org_id: r.org_id,
        issuer_url: r.issuer_url,
        client_id: r.client_id,
        client_secret: state.cipher.decrypt(&r.encrypted_client_secret).unwrap_or_default(),
        allowed_domains: r.allowed_domains,
        default_role: r.default_role,
        enabled: r.enabled,
    }))
}

/// Resolves which `sso_configs` row a login attempt should use. If the org
/// has exactly one enabled config, it's picked automatically — an org only
/// needs to start passing `config_id` once it has more than one IdP.
async fn find_login_config(state: &SharedState, org_id: &str, config_id: Option<&str>) -> Result<Option<ResolvedConfig>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct IdRow {
        id: i32,
    }
    let ids: Vec<IdRow> = sqlx::query_as("SELECT id FROM sso_configs WHERE org_id = $1 AND enabled = true ORDER BY id ASC").bind(org_id).fetch_all(&state.pg).await?;
    if let Some(cid) = config_id {
        let Ok(cid) = cid.parse::<i32>() else { return Ok(None) };
        if !ids.iter().any(|r| r.id == cid) {
            return Ok(None);
        }
        return get_config_by_id(state, cid).await;
    }
    if ids.len() == 1 {
        return get_config_by_id(state, ids[0].id).await;
    }
    Ok(None)
}

#[derive(Deserialize)]
struct LoginQuery {
    config_id: Option<String>,
}

async fn login(State(state): State<SharedState>, Path(org_id): Path<String>, Query(q): Query<LoginQuery>, jar: CookieJar) -> Result<(CookieJar, axum::response::Response), ApiError> {
    require_enterprise_mode(&state)?;
    let Some(config) = find_login_config(&state, &org_id, q.config_id.as_deref()).await? else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "SSO is not configured for this org, or more than one IdP exists and no config_id was specified."));
    };

    let discovered = match discover(&state.http_client, &config.issuer_url).await {
        Ok(d) => d,
        Err(err) => {
            tracing::error!(error = %err, org_id, "failed to start SSO login (IdP discovery/config error)");
            return Err(ApiError::new(StatusCode::BAD_GATEWAY, "Could not reach this org's identity provider. Contact your admin."));
        }
    };

    let code_verifier = random_urlsafe(32);
    let code_challenge = pkce_challenge(&code_verifier);
    let flow_state = random_urlsafe(16);
    let nonce = random_urlsafe(16);
    let redirect_uri = format!("{}/api/v1/auth/sso/callback", state.public_url);

    let mut auth_url = url::Url::parse(&discovered.authorization_endpoint).map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "Identity provider returned an invalid authorization endpoint."))?;
    auth_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", "openid email profile")
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &flow_state)
        .append_pair("nonce", &nonce);

    let flow_json = json!({
        "orgId": org_id, "configId": config.id, "state": flow_state,
        "codeVerifier": code_verifier, "nonce": nonce,
    })
    .to_string();
    let mut cookie = Cookie::new(SSO_FLOW_COOKIE, flow_json);
    cookie.set_http_only(true);
    cookie.set_secure(state.is_production);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::minutes(10));
    let jar = jar.add(cookie);

    Ok((jar, Redirect::to(auth_url.as_str()).into_response()))
}

#[derive(Deserialize)]
struct FlowCookieIn {
    #[serde(rename = "orgId")]
    org_id: String,
    #[serde(rename = "configId")]
    config_id: i32,
    state: String,
    #[serde(rename = "codeVerifier")]
    code_verifier: String,
    nonce: String,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Pure — matches a verified email's domain against an org's configured
/// `allowed_domains` (comma-separated). Case-insensitive.
fn match_org_by_email_domain(email: &str, allowed_domains_csv: &str) -> bool {
    let Some(domain) = email.split('@').nth(1) else { return false };
    let domain = domain.to_lowercase();
    allowed_domains_csv.split(',').map(|d| d.trim().to_lowercase()).any(|d| !d.is_empty() && d == domain)
}

/// Pure — best-effort mapping from ID token claims to admin/developer/
/// auditor. Group/role claim names aren't standardized across IdPs, so
/// this checks common claim names permissively (substring match).
fn map_claims_to_role(claims: &Value, default_role: &str) -> String {
    let mut candidates: Vec<String> = Vec::new();
    for key in ["groups", "roles"] {
        if let Some(arr) = claims.get(key).and_then(Value::as_array) {
            candidates.extend(arr.iter().filter_map(|v| v.as_str()).map(|s| s.to_lowercase()));
        }
    }
    for role in ROLE_VALUES {
        if candidates.iter().any(|c| c.contains(role)) {
            return role.to_string();
        }
    }
    default_role.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_org_by_email_domain_is_case_insensitive() {
        assert!(match_org_by_email_domain("alice@acme.com", "acme.com,acme.io"));
        assert!(match_org_by_email_domain("alice@ACME.COM", "acme.com,acme.io"));
        assert!(!match_org_by_email_domain("alice@evil.com", "acme.com,acme.io"));
        assert!(!match_org_by_email_domain("not-an-email", "acme.com"));
    }

    #[test]
    fn map_claims_to_role_reads_group_claim_and_falls_back_to_default() {
        assert_eq!(map_claims_to_role(&serde_json::json!({"groups": ["org-admins"]}), "developer"), "admin");
        assert_eq!(map_claims_to_role(&serde_json::json!({"roles": ["acme-auditor-role"]}), "developer"), "auditor");
        assert_eq!(map_claims_to_role(&serde_json::json!({"groups": ["some-other-group"]}), "developer"), "developer");
        assert_eq!(map_claims_to_role(&serde_json::json!({}), "auditor"), "auditor");
    }
}

async fn verify_id_token(http_client: &reqwest::Client, jwks_uri: &str, id_token: &str, issuer: &str, client_id: &str, expected_nonce: &str) -> Result<Value, String> {
    let header = jsonwebtoken::decode_header(id_token).map_err(|e| e.to_string())?;
    let alg = header.alg;
    if alg != jsonwebtoken::Algorithm::RS256 {
        return Err(format!("unsupported ID token algorithm: {alg:?} (only RS256 is supported)"));
    }
    let Some(kid) = header.kid else { return Err("ID token is missing a kid".to_string()) };

    let jwks: Value = http_client.get(jwks_uri).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
    let keys = jwks.get("keys").and_then(Value::as_array).ok_or("malformed JWKS response")?;
    let key = keys
        .iter()
        .find(|k| k.get("kid").and_then(Value::as_str) == Some(kid.as_str()))
        .ok_or("no matching JWKS key for this ID token's kid")?;
    let n = key.get("n").and_then(Value::as_str).ok_or("JWKS key missing n")?;
    let e = key.get("e").and_then(Value::as_str).ok_or("JWKS key missing e")?;
    let decoding_key = jsonwebtoken::DecodingKey::from_rsa_components(n, e).map_err(|e| e.to_string())?;

    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[client_id]);
    let data = jsonwebtoken::decode::<Value>(id_token, &decoding_key, &validation).map_err(|e| e.to_string())?;

    if data.claims.get("nonce").and_then(Value::as_str) != Some(expected_nonce) {
        return Err("nonce mismatch".to_string());
    }
    Ok(data.claims)
}

async fn callback(State(state): State<SharedState>, Query(q): Query<CallbackQuery>, jar: CookieJar) -> Result<(CookieJar, Json<Value>), ApiError> {
    require_enterprise_mode(&state)?;
    let raw_flow = jar.get(SSO_FLOW_COOKIE).map(|c| c.value().to_string());
    let mut clear_cookie = Cookie::new(SSO_FLOW_COOKIE, "");
    clear_cookie.set_path("/");
    clear_cookie.set_max_age(time::Duration::seconds(0));
    let jar = jar.add(clear_cookie);

    let Some(raw_flow) = raw_flow else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Missing or expired SSO login attempt. Try logging in again."));
    };
    let Ok(flow) = serde_json::from_str::<FlowCookieIn>(&raw_flow) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid SSO login attempt."));
    };
    if let Some(err) = &q.error {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, format!("SSO login failed: {err}")));
    }
    let (Some(code), Some(returned_state)) = (&q.code, &q.state) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Missing authorization code."));
    };
    if returned_state != &flow.state {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "SSO state mismatch — possible CSRF attempt."));
    }

    let Some(config) = get_config_by_id(&state, flow.config_id).await? else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "SSO is not configured or is disabled for this org."));
    };
    if !config.enabled || config.org_id != flow.org_id {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "SSO is not configured or is disabled for this org."));
    }

    let discovered = match discover(&state.http_client, &config.issuer_url).await {
        Ok(d) => d,
        Err(err) => {
            tracing::error!(error = %err, org_id = %flow.org_id, "SSO callback failed (discovery error)");
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "SSO login failed. Try again, or contact your admin."));
        }
    };

    let redirect_uri = format!("{}/api/v1/auth/sso/callback", state.public_url);
    let token_resp: Value = match state
        .http_client
        .post(&discovered.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", config.client_id.as_str()),
            ("client_secret", config.client_secret.as_str()),
            ("code_verifier", flow.code_verifier.as_str()),
        ])
        .send()
        .await
    {
        Ok(resp) => match resp.json().await {
            Ok(v) => v,
            Err(err) => {
                tracing::error!(?err, org_id = %flow.org_id, "SSO callback failed (malformed token response)");
                return Err(ApiError::new(StatusCode::UNAUTHORIZED, "SSO login failed. Try again, or contact your admin."));
            }
        },
        Err(err) => {
            tracing::error!(?err, org_id = %flow.org_id, "SSO callback failed (token exchange error)");
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "SSO login failed. Try again, or contact your admin."));
        }
    };
    let Some(id_token) = token_resp.get("id_token").and_then(Value::as_str) else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "SSO login failed. Try again, or contact your admin."));
    };

    let claims = match verify_id_token(&state.http_client, &discovered.jwks_uri, id_token, &discovered.issuer, &config.client_id, &flow.nonce).await {
        Ok(c) => c,
        Err(err) => {
            tracing::error!(error = %err, org_id = %flow.org_id, "SSO callback failed (ID token verification error)");
            return Err(ApiError::new(StatusCode::UNAUTHORIZED, "SSO login failed. Try again, or contact your admin."));
        }
    };

    let email_verified = claims.get("email_verified").and_then(Value::as_bool);
    let email = claims.get("email").and_then(Value::as_str);
    if email_verified == Some(false) || email.is_none() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Your identity provider did not return a verified email address."));
    }
    let email = email.unwrap();
    if !match_org_by_email_domain(email, &config.allowed_domains) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Your email domain is not allowed to sign in to this org."));
    }
    let role = map_claims_to_role(&claims, &config.default_role);

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: i32,
        email: String,
        org_id: Option<String>,
    }
    let existing: Option<UserRow> = sqlx::query_as("SELECT id, email, org_id FROM users WHERE email = $1").bind(email).fetch_optional(&state.pg).await?;
    let user = match existing {
        Some(u) => u,
        None => {
            let mut random_pw = [0u8; 24];
            rand::thread_rng().fill_bytes(&mut random_pw);
            let unusable_hash = hash_password(&hex::encode(random_pw)).await?;
            sqlx::query_as::<_, UserRow>("INSERT INTO users (email, password_hash, org_id, email_verified) VALUES ($1, $2, $3, true) RETURNING id, email, org_id")
                .bind(email)
                .bind(&unusable_hash)
                .bind(&flow.org_id)
                .fetch_one(&state.pg)
                .await?
        }
    };

    upsert_membership(&state, user.id, &flow.org_id, &role).await?;

    let token = sign_session(&state.jwt_secret, user.id, &user.email, user.org_id.as_deref());
    let jar = jar.add(session_cookie(&state, token));
    Ok((jar, Json(json!({ "redirect": format!("{}/dashboard", state.public_url) }))))
}

// ─── Org members ───

async fn list_members(State(state): State<SharedState>, user: AuthUser, Path(org_id): Path<String>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    // No tier gate — a downgraded org should still see its own members.
    // Same reasoning as HITL's list_rules.

    #[derive(sqlx::FromRow, serde::Serialize)]
    struct Row {
        user_id: i32,
        email: String,
        role: String,
        created_at: chrono::DateTime<chrono::Utc>,
        updated_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT u.id as user_id, u.email, m.role, m.created_at, m.updated_at
         FROM org_members m JOIN users u ON u.id = m.user_id
         WHERE m.org_id = $1 ORDER BY m.created_at ASC",
    )
    .bind(&org_id)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "members": rows })))
}

#[derive(Deserialize)]
struct UpdateMemberBody {
    role: Option<String>,
}

async fn update_member(
    State(state): State<SharedState>,
    user: AuthUser,
    Path((org_id, user_id)): Path<(String, i32)>,
    Json(body): Json<UpdateMemberBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    // No tier gate — a role change doesn't consume a seat.
    let role = body.role.unwrap_or_default();
    if !ROLE_VALUES.contains(&role.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("role must be one of: {}", ROLE_VALUES.join(", "))));
    }
    let updated: Option<i32> = sqlx::query_scalar("UPDATE org_members SET role = $3, updated_at = NOW() WHERE org_id = $1 AND user_id = $2 RETURNING user_id")
        .bind(&org_id)
        .bind(user_id)
        .bind(&role)
        .fetch_optional(&state.pg)
        .await?;
    if updated.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "That user is not a member of this org."));
    }
    Ok(Json(json!({ "updated": true })))
}

async fn delete_member(State(state): State<SharedState>, user: AuthUser, Path((org_id, user_id)): Path<(String, i32)>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    // No tier gate — removing a member frees a seat, never consumes one.
    sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2").bind(&org_id).bind(user_id).execute(&state.pg).await?;
    Ok(Json(json!({ "deleted": true })))
}

// ─── Org invites ───

#[derive(Deserialize)]
struct CreateInviteBody {
    email: Option<String>,
    role: Option<String>,
}

async fn create_invite(
    State(state): State<SharedState>,
    user: AuthUser,
    Path(org_id): Path<String>,
    Json(body): Json<CreateInviteBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    // Team invites moved from Enterprise-only to Pro+ (was
    // `require_enterprise_mode` — the whole server's on/off switch,
    // independent of the calling org's actual plan). Same shape as
    // HITL's Pro+ gate in ee/hitl.rs. OIDC login/config functions in this
    // same file keep `require_enterprise_mode`, unchanged.
    require_tier(&state, &org_id, agentraas_core::tier::Tier::Team).await?;

    let email = body.email.unwrap_or_default();
    if !is_valid_email(&email) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Enter a valid email address."));
    }
    let invite_role = body.role.unwrap_or_else(|| "developer".to_string());
    if !ROLE_VALUES.contains(&invite_role.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("role must be one of: {}", ROLE_VALUES.join(", "))));
    }

    sqlx::query("INSERT INTO orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING").bind(&org_id).execute(&state.pg).await?;

    let mut raw_token_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw_token_bytes);
    let raw_token = hex::encode(raw_token_bytes);
    let token_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(raw_token.as_bytes()))
    };
    let expires_at = chrono::Utc::now() + chrono::Duration::days(INVITE_TTL_DAYS);

    // Seat cap by tier (`Tier::seat_limit`); every account is Enterprise
    // today (see `effective_tier`), so unlimited. Counts existing
    // members + still-pending invites together (an accepted invite
    // becomes a member, so both reserve a seat). No separate "+1 for the
    // owner": require_org_admin above already guarantees the owner has a
    // real org_members row by this point (it bootstraps one if missing),
    // so they're already included in member_count. `SELECT ... FOR
    // UPDATE` on the org's own row (just upserted above) serializes
    // concurrent invite creations for the same org, so two requests
    // racing at exactly the boundary can't both succeed past the cap.
    // Doesn't touch orgs that already exceed their tier's limit from
    // before this check existed — grandfathered in place, just blocked
    // from adding more until they're back under the cap or upgrade.
    let mut tx = state.pg.begin().await?;
    sqlx::query("SELECT 1 FROM orgs WHERE org_id = $1 FOR UPDATE").bind(&org_id).execute(&mut *tx).await?;
    let org_tier = crate::agent::db::effective_tier(&state, &org_id).await;
    if let Some(limit) = org_tier.seat_limit() {
        let member_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE org_id = $1").bind(&org_id).fetch_one(&mut *tx).await?;
        let pending_invite_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_invites WHERE org_id = $1 AND accepted_at IS NULL AND expires_at > NOW()")
            .bind(&org_id)
            .fetch_one(&mut *tx)
            .await?;
        if member_count + pending_invite_count >= limit as i64 {
            return Err(ApiError::new(
                StatusCode::PAYMENT_REQUIRED,
                format!("Seat limit reached ({limit} on this plan). Remove or revoke an existing member or pending invite first, or contact hello@agentraas.io for more seats."),
            ));
        }
    }

    let id: i32 = sqlx::query_scalar(
        "INSERT INTO org_invites (org_id, email, role, token_hash, invited_by_user_id, expires_at) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id",
    )
    .bind(&org_id)
    .bind(&email)
    .bind(&invite_role)
    .bind(&token_hash)
    .bind(user.sub)
    .bind(expires_at)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    let accept_url = format!("{}/dashboard?invite_token={}", state.public_url, raw_token);
    state.mailer.send_org_invite_email(&email, &org_id, &invite_role, &accept_url).await;

    let mut response = json!({ "invited": true, "id": id });
    if !state.mailer.is_configured() || state.expose_dev_verify_url {
        response["dev_accept_url"] = json!(accept_url);
    }
    Ok(Json(response))
}

async fn list_invites(State(state): State<SharedState>, user: AuthUser, Path(org_id): Path<String>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    // No tier gate — same reasoning as list_members.
    #[derive(sqlx::FromRow, serde::Serialize)]
    struct Row {
        id: i32,
        email: String,
        role: String,
        expires_at: chrono::DateTime<chrono::Utc>,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, email, role, expires_at, created_at FROM org_invites
         WHERE org_id = $1 AND accepted_at IS NULL AND expires_at > NOW() ORDER BY created_at DESC",
    )
    .bind(&org_id)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "invites": rows })))
}

async fn delete_invite(State(state): State<SharedState>, user: AuthUser, Path((org_id, id)): Path<(String, i32)>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    require_org_admin(&state, user.sub, &org_id).await?;
    // No tier gate — revoking a pending invite frees its reserved seat.
    sqlx::query("DELETE FROM org_invites WHERE id = $1 AND org_id = $2").bind(id).bind(&org_id).execute(&state.pg).await?;
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct AcceptInviteBody {
    token: Option<String>,
    password: Option<String>,
}

/// Public — accepting an invite doesn't require an existing session.
/// `password` is required only to create a brand-new account; an existing
/// user's password is left untouched, only their `org_members` role is
/// granted.
async fn accept_invite(State(state): State<SharedState>, jar: CookieJar, Json(body): Json<AcceptInviteBody>) -> Result<(CookieJar, Json<Value>), ApiError> {
    // No tier gate — the seat this invite consumes was already checked at
    // creation time (create_invite); accepting doesn't consume a new one,
    // so an org that's since downgraded shouldn't block a pending accept.
    let Some(token) = body.token.filter(|t| !t.is_empty()) else {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "An invite token is required."));
    };
    let token_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(token.as_bytes()))
    };

    #[derive(sqlx::FromRow)]
    struct InviteRow {
        id: i32,
        org_id: String,
        email: String,
        role: String,
    }
    let invite: Option<InviteRow> = sqlx::query_as("SELECT id, org_id, email, role FROM org_invites WHERE token_hash = $1 AND accepted_at IS NULL AND expires_at > NOW()")
        .bind(&token_hash)
        .fetch_optional(&state.pg)
        .await?;
    let Some(invite) = invite else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "This invite is invalid, already used, or has expired."));
    };

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: i32,
        email: String,
        org_id: Option<String>,
    }
    let existing: Option<UserRow> = sqlx::query_as("SELECT id, email, org_id FROM users WHERE email = $1").bind(&invite.email).fetch_optional(&state.pg).await?;
    let user = match existing {
        Some(u) => u,
        None => {
            let Some(password) = &body.password else {
                return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Password must be at least 8 characters (required to create your account)."));
            };
            if !is_valid_password(password) {
                return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Password must be at least 8 characters (required to create your account)."));
            }
            let password_hash = hash_password(password).await?;
            sqlx::query_as::<_, UserRow>("INSERT INTO users (email, password_hash, org_id, email_verified) VALUES ($1, $2, $3, true) RETURNING id, email, org_id")
                .bind(&invite.email)
                .bind(&password_hash)
                .bind(&invite.org_id)
                .fetch_one(&state.pg)
                .await?
        }
    };

    upsert_membership(&state, user.id, &invite.org_id, &invite.role).await?;
    sqlx::query("UPDATE org_invites SET accepted_at = NOW() WHERE id = $1").bind(invite.id).execute(&state.pg).await?;

    let session_token = sign_session(&state.jwt_secret, user.id, &user.email, user.org_id.as_deref());
    let jar = jar.add(session_cookie(&state, session_token));
    Ok((jar, Json(json!({ "accepted": true, "org_id": invite.org_id, "role": invite.role }))))
}

