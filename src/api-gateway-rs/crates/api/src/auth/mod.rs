//! Mirrors `src/api-gateway/auth.js` plus the JWT/cookie/preHandler bits of
//! `server.js` (`requireAuth`, `dashboardRateLimit`, `COOKIE_OPTS`) — kept
//! together since Node keeps them all in the "auth" conceptual area even
//! though they're split across two files there.

pub mod routes;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum_extra::extract::cookie::{Cookie, SameSite};
use axum_extra::extract::CookieJar;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::state::{ApiError, SharedState};

pub const SESSION_COOKIE_NAME: &str = "ar_session";
const SESSION_MAX_AGE_SECONDS: i64 = 60 * 60 * 24 * 7; // 7 days — kept in lockstep with the JWT's own `exp` below, same as Node.

// ─── auth.js equivalents ───

pub const SALT_ROUNDS: u32 = 12;

pub async fn hash_password(password: &str) -> Result<String, ApiError> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || bcrypt::hash(password, SALT_ROUNDS))
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password hashing failed."))?
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Password hashing failed."))
}

/// Verifies against bcryptjs's `$2a$` hashes too, not just this crate's own
/// `$2b$` output — the `bcrypt` crate's algorithm is identical across the
/// 2a/2b/2y prefixes, only the prefix byte differs by implementation
/// history, and `bcrypt::verify` doesn't care which prefix a stored hash
/// uses. Confirmed empirically against a real bcryptjs hash (see
/// PORT_PROGRESS.md).
pub async fn verify_password(password: &str, hash: &str) -> bool {
    let password = password.to_string();
    let hash = hash.to_string();
    tokio::task::spawn_blocking(move || bcrypt::verify(password, &hash))
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false)
}

pub fn is_valid_email(email: &str) -> bool {
    // Same deliberately-loose pattern as auth.js: ^[^\s@]+@[^\s@]+\.[^\s@]+$
    let mut parts = email.splitn(2, '@');
    let (local, domain) = match (parts.next(), parts.next()) {
        (Some(l), Some(d)) if !l.is_empty() && !d.is_empty() => (l, d),
        _ => return false,
    };
    if local.chars().any(char::is_whitespace) {
        return false;
    }
    if domain.chars().any(char::is_whitespace) || !domain.contains('.') {
        return false;
    }
    let (before, after) = domain.rsplit_once('.').unwrap();
    !before.is_empty() && !after.is_empty()
}

pub fn is_valid_password(password: &str) -> bool {
    password.chars().count() >= 8
}

pub fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 100
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Curated service actions are dotted (`charge.create`) — a bit more
/// permissive than `is_valid_identifier` while keeping the charset safe
/// for a JSONB/SQL key.
pub fn is_valid_action_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 100
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

const LOGIN_ATTEMPT_LIMIT: i64 = 10;
const LOGIN_ATTEMPT_WINDOW_SECONDS: i64 = 15 * 60;

/// Same non-atomic INCR-then-EXPIRE-on-first pattern as the Node original —
/// a deliberate compatibility choice (see auth.js's own comment on the
/// narrow race), not an oversight.
pub async fn check_login_rate_limit(
    redis: &redis::Client,
    ip: &str,
    email: &str,
) -> Result<bool, ApiError> {
    let mut conn = redis.get_multiplexed_async_connection().await?;
    let key = format!("loginlimit:{ip}:{email}");
    let attempts: i64 = redis::cmd("INCR").arg(&key).query_async(&mut conn).await?;
    if attempts == 1 {
        let _: () = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(LOGIN_ATTEMPT_WINDOW_SECONDS)
            .query_async(&mut conn)
            .await?;
    }
    Ok(attempts <= LOGIN_ATTEMPT_LIMIT)
}

pub async fn clear_login_rate_limit(
    redis: &redis::Client,
    ip: &str,
    email: &str,
) -> Result<(), ApiError> {
    let mut conn = redis.get_multiplexed_async_connection().await?;
    let key = format!("loginlimit:{ip}:{email}");
    let _: () = redis::cmd("DEL").arg(&key).query_async(&mut conn).await?;
    Ok(())
}

// ─── JWT / session cookie ───

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: i32,
    pub email: String,
    pub org_id: Option<String>,
    pub iat: i64,
    pub exp: i64,
}

pub fn sign_session(secret: &str, sub: i32, email: &str, org_id: Option<&str>) -> String {
    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        sub,
        email: email.to_string(),
        org_id: org_id.map(|s| s.to_string()),
        iat: now,
        exp: now + SESSION_MAX_AGE_SECONDS,
    };
    // HS256, matching @fastify/jwt's default when only a plain secret
    // string is registered (no explicit algorithm/key-pair config).
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .expect("JWT signing should never fail for a well-formed Claims struct")
}

pub fn verify_session(secret: &str, token: &str) -> Option<Claims> {
    let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    decode::<Claims>(token, &DecodingKey::from_secret(secret.as_bytes()), &validation)
        .ok()
        .map(|data| data.claims)
}

/// Same shape as Node's `COOKIE_OPTS`: httpOnly, `secure` only in
/// production, `SameSite=Lax`, path `/`, 7-day maxAge.
pub fn session_cookie(state: &SharedState, token: String) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, token);
    cookie.set_http_only(true);
    cookie.set_secure(state.is_production);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::seconds(SESSION_MAX_AGE_SECONDS));
    cookie
}

pub fn clear_session_cookie() -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE_NAME, "");
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::seconds(0));
    cookie
}

// ─── requireAuth (as an Axum extractor) ───

/// The logged-in dashboard user, resolved from the `ar_session` cookie or an
/// `Authorization: Bearer <token>` header — mirrors `requireAuth` +
/// `@fastify/jwt`'s cookie-fallback behavior. Reject with the exact same
/// `401 {"error": "Not authenticated"}` body Node returns.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub sub: i32,
    pub email: String,
    pub org_id: Option<String>,
}

#[async_trait::async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    SharedState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = SharedState::from_ref(state);
        let unauthenticated =
            || ApiError::new(StatusCode::UNAUTHORIZED, "Not authenticated");

        let token = if let Some(auth_header) = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        {
            auth_header
                .strip_prefix("Bearer ")
                .map(|s| s.to_string())
                .ok_or_else(unauthenticated)?
        } else {
            let jar = CookieJar::from_headers(&parts.headers);
            jar.get(SESSION_COOKIE_NAME)
                .map(|c| c.value().to_string())
                .ok_or_else(unauthenticated)?
        };

        let claims = verify_session(&app_state.jwt_secret, &token).ok_or_else(unauthenticated)?;
        Ok(AuthUser {
            sub: claims.sub,
            email: claims.email,
            org_id: claims.org_id,
        })
    }
}

/// Mirrors `dashboardRateLimit` — a per-user, per-minute Redis counter.
/// Call this explicitly after pulling `AuthUser` out of the request (an
/// extractor can't cleanly depend on another extractor's *value* the way a
/// Fastify preHandler array can share `request.user`), so every
/// `requireAuthRateLimited`-equivalent handler does:
/// `let user = AuthUser::from...; check_dashboard_rate_limit(&state, user.sub).await?;`
pub async fn check_dashboard_rate_limit(state: &SharedState, user_id: i32) -> Result<(), ApiError> {
    let mut conn = state.redis.get_multiplexed_async_connection().await?;
    let window = chrono::Utc::now().timestamp() / 60;
    let key = format!("ratelimit:dashboard:{user_id}:{window}");
    let count: i64 = redis::cmd("INCR").arg(&key).query_async(&mut conn).await?;
    if count == 1 {
        let _: () = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(65)
            .query_async(&mut conn)
            .await?;
    }
    if count > state.dashboard_rate_limit_per_min as i64 {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit exceeded. Slow down and try again shortly.",
        ));
    }
    Ok(())
}
