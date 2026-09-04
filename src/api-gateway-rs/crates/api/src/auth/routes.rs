use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::state::{ApiError, SharedState};

use super::{
    check_dashboard_rate_limit, check_login_rate_limit, clear_login_rate_limit,
    clear_session_cookie, hash_password, is_valid_email, is_valid_password, session_cookie,
    sign_session, verify_password, AuthUser,
};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/auth/register", post(register))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/verify-email", get(verify_email))
        .route(
            "/api/v1/auth/resend-verification",
            post(resend_verification),
        )
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/forgot-password", post(forgot_password))
        .route("/api/v1/auth/reset-password", post(reset_password))
        .route("/api/v1/auth/me", get(me))
        .route("/api/v1/auth/password", post(change_password))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

fn client_ip(addr: &SocketAddr) -> String {
    addr.ip().to_string()
}

// ─── POST /api/v1/auth/register ───

#[derive(Deserialize)]
struct RegisterBody {
    email: Option<String>,
    password: Option<String>,
    org_id: Option<String>,
}

async fn register(
    State(state): State<SharedState>,
    Json(body): Json<RegisterBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let email = body.email.unwrap_or_default();
    let password = body.password.unwrap_or_default();

    if !is_valid_email(&email) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Enter a valid email address."));
    }
    if !is_valid_password(&password) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Password must be at least 8 characters.",
        ));
    }

    let existing = sqlx::query_scalar::<_, i32>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.pg)
        .await?;
    if existing.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "An account with that email already exists.",
        ));
    }

    let password_hash = hash_password(&password).await?;
    // Same convention as register(): most users never hand-type an org_id,
    // so auto-generate a working default one.
    let default_org_id = body.org_id.unwrap_or_else(|| format!("org_{}", random_hex(6)));

    let user_id: i32 = sqlx::query_scalar(
        "INSERT INTO users (email, password_hash, org_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&email)
    .bind(&password_hash)
    .bind(&default_org_id)
    .fetch_one(&state.pg)
    .await?;

    let raw_token = random_hex(32);
    let token_hash = sha256_hex(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);
    sqlx::query(
        "INSERT INTO email_verification_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires_at)
    .execute(&state.pg)
    .await?;

    let verify_url = format!("{}/dashboard?verify_token={}", state.public_url, raw_token);
    state.mailer.send_verification_email(&email, &verify_url).await;

    let mut response = json!({
        "registered": true,
        "message": "Check your email to verify your account before logging in.",
    });
    if !state.mailer.is_configured() || state.expose_dev_verify_url {
        response["dev_verify_url"] = json!(verify_url);
    }
    Ok(Json(response))
}

// ─── POST /api/v1/auth/login ───

#[derive(Deserialize)]
struct LoginBody {
    email: Option<String>,
    password: Option<String>,
}

#[derive(sqlx::FromRow)]
struct LoginUserRow {
    id: i32,
    email: String,
    org_id: Option<String>,
    plan: String,
    password_hash: String,
    is_admin: bool,
    email_verified: bool,
}

async fn login(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> Result<(CookieJar, Json<serde_json::Value>), ApiError> {
    let email = body.email.unwrap_or_default();
    let password = body.password.unwrap_or_default();
    if !is_valid_email(&email) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Email and password are required.",
        ));
    }

    let ip = client_ip(&addr);
    if !check_login_rate_limit(&state.redis, &ip, &email).await? {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many login attempts. Try again in 15 minutes.",
        ));
    }

    let user = sqlx::query_as::<_, LoginUserRow>(
        "SELECT id, email, org_id, plan, password_hash, is_admin, email_verified FROM users WHERE email = $1",
    )
    .bind(&email)
    .fetch_optional(&state.pg)
    .await?;

    let Some(user) = user else {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid email or password."));
    };

    if !verify_password(&password, &user.password_hash).await {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid email or password."));
    }

    if !user.email_verified {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Please verify your email before logging in.",
        )
        .with_code("EMAIL_NOT_VERIFIED"));
    }

    clear_login_rate_limit(&state.redis, &ip, &email).await?;
    sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user.id)
        .execute(&state.pg)
        .await?;

    // The single system-wide admin's password rotates on every login — see
    // auth/mod.rs's session-cookie doc comment for the session side; this
    // mirrors server.js's login handler exactly, including logging the new
    // one-time password at WARN so `podman logs ar-api-rs` is the recovery
    // path, same as Node.
    if user.is_admin {
        let mut buf = [0u8; 18];
        rand::thread_rng().fill_bytes(&mut buf);
        use base64::Engine;
        let rotated_password = base64::engine::general_purpose::STANDARD.encode(buf);
        let rotated_hash = hash_password(&rotated_password).await?;
        sqlx::query(
            "UPDATE users SET password_hash = $1, must_change_password = false WHERE id = $2",
        )
        .bind(&rotated_hash)
        .bind(user.id)
        .execute(&state.pg)
        .await?;
        tracing::warn!(
            email = %user.email,
            "[ADMIN PASSWORD ROTATED] New password for {}: {}",
            user.email,
            rotated_password
        );
    }

    let token = sign_session(&state.jwt_secret, user.id, &user.email, user.org_id.as_deref());
    let jar = jar.add(session_cookie(&state, token));

    Ok((
        jar,
        Json(json!({
            "user": {
                "id": user.id,
                "email": user.email,
                "org_id": user.org_id,
                "plan": user.plan,
                "is_admin": user.is_admin,
                "deployment_mode": state.deployment_mode,
                "must_change_password": false,
            }
        })),
    ))
}

// ─── GET /api/v1/auth/verify-email ───

#[derive(Deserialize)]
struct VerifyEmailQuery {
    token: Option<String>,
}

#[derive(sqlx::FromRow)]
struct VerifiedUserRow {
    id: i32,
    email: String,
    org_id: Option<String>,
    plan: String,
    is_admin: bool,
    must_change_password: bool,
}

async fn verify_email(
    State(state): State<SharedState>,
    Query(query): Query<VerifyEmailQuery>,
    jar: CookieJar,
) -> Result<(CookieJar, Json<serde_json::Value>), ApiError> {
    let token = query
        .token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "A verification token is required."))?;

    let token_hash = sha256_hex(&token);
    let row = sqlx::query_as::<_, (i32, i32)>(
        "SELECT id, user_id FROM email_verification_tokens WHERE token_hash = $1 AND used_at IS NULL AND expires_at > NOW()",
    )
    .bind(&token_hash)
    .fetch_optional(&state.pg)
    .await?;

    let Some((token_id, user_id)) = row else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This verification link is invalid or has expired. Request a new one.",
        ));
    };

    let mut tx = state.pg.begin().await?;
    let user = sqlx::query_as::<_, VerifiedUserRow>(
        "UPDATE users SET email_verified = true WHERE id = $1 RETURNING id, email, org_id, plan, is_admin, must_change_password",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE email_verification_tokens SET used_at = NOW() WHERE id = $1")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let session_token = sign_session(&state.jwt_secret, user.id, &user.email, user.org_id.as_deref());
    let jar = jar.add(session_cookie(&state, session_token));

    Ok((
        jar,
        Json(json!({
            "verified": true,
            "user": {
                "id": user.id,
                "email": user.email,
                "org_id": user.org_id,
                "plan": user.plan,
                "is_admin": user.is_admin,
                "deployment_mode": state.deployment_mode,
                "must_change_password": user.must_change_password,
            }
        })),
    ))
}

// ─── POST /api/v1/auth/resend-verification ───

#[derive(Deserialize)]
struct EmailOnlyBody {
    email: Option<String>,
}

fn generic_resend_response() -> serde_json::Value {
    json!({ "message": "If that account exists and needs verification, a new link has been sent." })
}

async fn resend_verification(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<EmailOnlyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(email) = body.email.filter(|e| is_valid_email(e)) else {
        return Ok(Json(generic_resend_response()));
    };

    let ip = client_ip(&addr);
    let key = format!("resend-verify:{email}");
    if !check_login_rate_limit(&state.redis, &ip, &key).await? {
        return Ok(Json(generic_resend_response()));
    }

    let user = sqlx::query_as::<_, (i32, bool)>("SELECT id, email_verified FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.pg)
        .await?;
    let Some((user_id, email_verified)) = user else {
        return Ok(Json(generic_resend_response()));
    };
    if email_verified {
        return Ok(Json(generic_resend_response()));
    }

    let raw_token = random_hex(32);
    let token_hash = sha256_hex(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(24);
    sqlx::query(
        "INSERT INTO email_verification_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires_at)
    .execute(&state.pg)
    .await?;

    let verify_url = format!("{}/dashboard?verify_token={}", state.public_url, raw_token);
    state.mailer.send_verification_email(&email, &verify_url).await;

    let mut response = generic_resend_response();
    if !state.mailer.is_configured() || state.expose_dev_verify_url {
        response["dev_verify_url"] = json!(verify_url);
    }
    Ok(Json(response))
}

// ─── POST /api/v1/auth/logout ───

async fn logout(jar: CookieJar) -> (CookieJar, Json<serde_json::Value>) {
    let jar = jar.add(clear_session_cookie());
    (jar, Json(json!({ "loggedOut": true })))
}

// ─── POST /api/v1/auth/forgot-password ───

fn generic_forgot_password_response() -> serde_json::Value {
    json!({ "message": "If an account exists for that email, a reset link has been sent." })
}

async fn forgot_password(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<EmailOnlyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Some(email) = body.email.filter(|e| is_valid_email(e)) else {
        return Ok(Json(generic_forgot_password_response()));
    };

    let ip = client_ip(&addr);
    let key = format!("reset:{email}");
    if !check_login_rate_limit(&state.redis, &ip, &key).await? {
        return Ok(Json(generic_forgot_password_response()));
    }

    let user_id = sqlx::query_scalar::<_, i32>("SELECT id FROM users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.pg)
        .await?;
    let Some(user_id) = user_id else {
        return Ok(Json(generic_forgot_password_response()));
    };

    let raw_token = random_hex(32);
    let token_hash = sha256_hex(&raw_token);
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);
    sqlx::query(
        "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(user_id)
    .bind(&token_hash)
    .bind(expires_at)
    .execute(&state.pg)
    .await?;

    let reset_url = format!("{}/dashboard?reset_token={}", state.public_url, raw_token);
    state.mailer.send_password_reset_email(&email, &reset_url).await;

    // Deliberately no dev_reset_url field, even locally — matches Node
    // exactly: only the verification-link routes expose a dev fallback in
    // the response body, the reset link is logged (via Mailer) but never
    // echoed back.
    Ok(Json(generic_forgot_password_response()))
}

// ─── POST /api/v1/auth/reset-password ───

#[derive(Deserialize)]
struct ResetPasswordBody {
    token: Option<String>,
    new_password: Option<String>,
}

async fn reset_password(
    State(state): State<SharedState>,
    Json(body): Json<ResetPasswordBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let token = body
        .token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "A reset token is required."))?;
    let new_password = body.new_password.unwrap_or_default();
    if !is_valid_password(&new_password) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "New password must be at least 8 characters.",
        ));
    }

    let token_hash = sha256_hex(&token);
    let row = sqlx::query_as::<_, (i32, i32)>(
        "SELECT id, user_id FROM password_reset_tokens WHERE token_hash = $1 AND used_at IS NULL AND expires_at > NOW()",
    )
    .bind(&token_hash)
    .fetch_optional(&state.pg)
    .await?;
    let Some((token_id, user_id)) = row else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This reset link is invalid or has expired. Request a new one.",
        ));
    };

    let new_hash = hash_password(&new_password).await?;
    let mut tx = state.pg.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(&new_hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE password_reset_tokens SET used_at = NOW() WHERE id = $1")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Json(json!({ "reset": true })))
}

// ─── GET /api/v1/auth/me ───

#[derive(Serialize)]
struct OrgMembership {
    org_id: String,
    role: String,
}

async fn me(State(state): State<SharedState>, user: AuthUser) -> Result<Json<serde_json::Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let admin_row = sqlx::query_as::<_, (bool, bool)>(
        "SELECT is_admin, must_change_password FROM users WHERE id = $1",
    )
    .bind(user.sub)
    .fetch_optional(&state.pg)
    .await?;
    let (is_admin, must_change_password) = admin_row.unwrap_or((false, false));

    let memberships = sqlx::query_as::<_, (String, String)>(
        "SELECT org_id, role FROM org_members WHERE user_id = $1",
    )
    .bind(user.sub)
    .fetch_all(&state.pg)
    .await?
    .into_iter()
    .map(|(org_id, role)| OrgMembership { org_id, role })
    .collect::<Vec<_>>();

    Ok(Json(json!({
        "user": {
            "id": user.sub,
            "email": user.email,
            "org_id": user.org_id,
            "is_admin": is_admin,
            "deployment_mode": state.deployment_mode,
            "must_change_password": must_change_password,
            "orgs": memberships,
        }
    })))
}

// ─── POST /api/v1/auth/password ───

#[derive(Deserialize)]
struct ChangePasswordBody {
    current_password: Option<String>,
    new_password: Option<String>,
}

async fn change_password(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<ChangePasswordBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let new_password = body.new_password.unwrap_or_default();
    if !is_valid_password(&new_password) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "New password must be at least 8 characters.",
        ));
    }

    let current_hash = sqlx::query_scalar::<_, String>("SELECT password_hash FROM users WHERE id=$1")
        .bind(user.sub)
        .fetch_optional(&state.pg)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "User not found."))?;

    let current_password = body.current_password.unwrap_or_default();
    if !verify_password(&current_password, &current_hash).await {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Current password is incorrect."));
    }

    let new_hash = hash_password(&new_password).await?;
    sqlx::query("UPDATE users SET password_hash=$1, must_change_password=false WHERE id=$2")
        .bind(&new_hash)
        .bind(user.sub)
        .execute(&state.pg)
        .await?;

    Ok(Json(json!({ "updated": true })))
}
