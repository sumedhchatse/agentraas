//! Self-host package download — mirrors the "SELF-HOST PACKAGE SNAPSHOT"
//! block and `/api/v1/download/self-host*` routes in `server.js`. Reads
//! directly from `/repo` on every request proved unreliable under rootless
//! Podman's UID/SELinux handling (per Node's own comment), so this copies
//! the needed files ONCE at startup into a location this container
//! reliably owns, and serves the zip from that stable local copy.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{check_dashboard_rate_limit, hash_password, AuthUser};
use crate::state::{ApiError, SharedState};

pub const SNAPSHOT_DIR: &str = "/tmp/.self-host-snapshot";
const REPO_DIR: &str = "/repo";

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/download/self-host/request", post(request_download))
        .route("/api/v1/download/self-host", get(download))
}

fn should_skip(path: &std::path::Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("node_modules") || s.ends_with(".env") || s.ends_with(".log")
}

/// Copies `src`/`infra`/`config` (filtered) plus a handful of top-level
/// files from `/repo` into `SNAPSHOT_DIR`, once, at process startup.
/// Best-effort: a missing `/repo` mount (this container started without
/// it) just means the download endpoint 500s later, same as Node.
pub fn build_snapshot() {
    if !std::path::Path::new(REPO_DIR).exists() {
        tracing::warn!("no /repo mount — self-host download endpoint will not work");
        return;
    }
    let _ = std::fs::remove_dir_all(SNAPSHOT_DIR);
    if let Err(err) = std::fs::create_dir_all(SNAPSHOT_DIR) {
        tracing::warn!(?err, "could not create self-host snapshot dir");
        return;
    }

    const SNAPSHOT_DIRS: &[&str] = &["src", "infra", "config"];
    const SNAPSHOT_FILES: &[&str] = &[
        "README.md", "LICENSE.md", "PRIVACY.md", "TERMS.md", "SECURITY.md",
        "CODE_OF_CONDUCT.md", "CONTRIBUTING.md", "GETTING_STARTED.md", "compose.yaml", "install.sh",
        ".env.example", ".gitignore",
    ];

    for dir in SNAPSHOT_DIRS {
        let src_dir = std::path::Path::new(REPO_DIR).join(dir);
        if !src_dir.exists() {
            continue;
        }
        let dest_root = std::path::Path::new(SNAPSHOT_DIR).join(dir);
        for entry in walkdir::WalkDir::new(&src_dir).into_iter().filter_map(Result::ok) {
            let rel = match entry.path().strip_prefix(&src_dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if should_skip(entry.path()) {
                continue;
            }
            let dest = dest_root.join(rel);
            if entry.file_type().is_dir() {
                let _ = std::fs::create_dir_all(&dest);
            } else if entry.file_type().is_file() {
                if let Some(parent) = dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(err) = std::fs::copy(entry.path(), &dest) {
                    tracing::warn!(?err, path = %entry.path().display(), "self-host snapshot: failed to copy file");
                }
            }
        }
    }
    for file in SNAPSHOT_FILES {
        let src_file = std::path::Path::new(REPO_DIR).join(file);
        if src_file.exists() {
            let _ = std::fs::copy(&src_file, std::path::Path::new(SNAPSHOT_DIR).join(file));
        }
    }
    tracing::info!(dir = SNAPSHOT_DIR, "self-host package snapshot created");
}

async fn has_connected_agent(state: &SharedState, user_id: i32) -> Result<bool, sqlx::Error> {
    let row: Option<i32> = sqlx::query_scalar("SELECT 1 FROM api_keys WHERE user_id = $1 LIMIT 1").bind(user_id).fetch_optional(&state.pg).await?;
    Ok(row.is_some())
}

#[derive(Deserialize)]
struct RequestBody {
    reason: Option<String>,
    company: Option<String>,
}

async fn request_download(State(state): State<SharedState>, user: AuthUser, Json(body): Json<RequestBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !has_connected_agent(&state, user.sub).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Connect an agent first — the self-host download unlocks once you have.").with_code("NO_AGENT_CONNECTED"));
    }

    let reason = body.reason.unwrap_or_default();
    let trimmed_reason = reason.trim();
    if trimmed_reason.chars().count() < 3 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Tell us a bit about what you'll use it for (at least a few words)."));
    }
    if reason.chars().count() > 2000 {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "Reason must be under 2000 characters."));
    }
    let trimmed_company: Option<String> = body.company.map(|c| c.trim().chars().take(255).collect()).filter(|c: &String| !c.is_empty());

    sqlx::query("INSERT INTO self_host_download_requests (user_id, reason, company) VALUES ($1, $2, $3)")
        .bind(user.sub)
        .bind(trimmed_reason)
        .bind(&trimmed_company)
        .execute(&state.pg)
        .await?;
    Ok(Json(json!({ "unlocked": true })))
}

const EXCLUDED_SCRIPTS: &[&str] = &["bootstrap-admin.sh", "set-org-limit.sh", "create-test-user.sh"];
const EXCLUDED_MIGRATIONS: &[&str] = &["013_single_admin_and_forced_password_change.sql", "014_move_admin_to_local_range.sql"];
const INCLUDE_DIRS: &[&str] = &["src", "infra", "config"];
const INCLUDE_FILES: &[&str] = &[
    "README.md", "LICENSE.md", "PRIVACY.md", "TERMS.md", "SECURITY.md",
    "CODE_OF_CONDUCT.md", "CONTRIBUTING.md", "GETTING_STARTED.md", "compose.yaml", "install.sh",
    ".env.example", ".gitignore",
];

/// Builds the zip archive synchronously (the `zip` crate is blocking) —
/// run inside `spawn_blocking`. Returns the complete archive bytes.
fn build_zip_archive(seed_sql: String, instructions_txt: String) -> Result<Vec<u8>, String> {
    let buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(buf);
    let options = zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for dir in INCLUDE_DIRS {
        let full_dir = std::path::Path::new(SNAPSHOT_DIR).join(dir);
        if !full_dir.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&full_dir).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let base_name = entry.file_name().to_string_lossy();
            if EXCLUDED_SCRIPTS.contains(&base_name.as_ref()) || EXCLUDED_MIGRATIONS.contains(&base_name.as_ref()) {
                continue;
            }
            let rel_path = entry.path().strip_prefix(SNAPSHOT_DIR).map_err(|e| e.to_string())?;
            let zip_name = rel_path.to_string_lossy().replace('\\', "/");
            zip.start_file(zip_name, options).map_err(|e| e.to_string())?;
            let contents = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut zip, &contents).map_err(|e| e.to_string())?;
        }
    }
    for file in INCLUDE_FILES {
        let full_path = std::path::Path::new(SNAPSHOT_DIR).join(file);
        if full_path.exists() {
            zip.start_file(*file, options).map_err(|e| e.to_string())?;
            let contents = std::fs::read(&full_path).map_err(|e| e.to_string())?;
            std::io::Write::write_all(&mut zip, &contents).map_err(|e| e.to_string())?;
        }
    }

    zip.start_file("infra/migrations/010_seed_your_account.sql", options).map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut zip, seed_sql.as_bytes()).map_err(|e| e.to_string())?;
    zip.start_file("SETUP_INSTRUCTIONS.txt", options).map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut zip, instructions_txt.as_bytes()).map_err(|e| e.to_string())?;

    let finished = zip.finish().map_err(|e| e.to_string())?;
    Ok(finished.into_inner())
}

async fn download(State(state): State<SharedState>, user: AuthUser) -> Result<impl IntoResponse, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !has_connected_agent(&state, user.sub).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Connect an agent first — the self-host download unlocks once you have.").with_code("NO_AGENT_CONNECTED"));
    }
    let has_request: Option<i32> = sqlx::query_scalar("SELECT 1 FROM self_host_download_requests WHERE user_id = $1 LIMIT 1").bind(user.sub).fetch_optional(&state.pg).await?;
    if has_request.is_none() {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Submit the request form first.").with_code("NO_REQUEST_SUBMITTED"));
    }
    if !std::path::Path::new(SNAPSHOT_DIR).exists() {
        return Err(ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "The self-host package is not available from this deployment."));
    }

    // Pre-seed the downloader's own account, so they don't need to
    // register and verify their email again on their self-hosted
    // instance — reuses the password-reset flow (a fresh single-use
    // token) rather than shipping a real password hash.
    let seed_email = user.email.clone();
    let escaped_email = seed_email.replace('\'', "''");
    let mut raw_bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut raw_bytes);
    let raw_setup_token = hex::encode(raw_bytes);
    let setup_token_hash = {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(raw_setup_token.as_bytes()))
    };
    let mut unusable_pw_bytes = [0u8; 24];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut unusable_pw_bytes);
    let unusable_password_hash = hash_password(&hex::encode(unusable_pw_bytes)).await?;

    let seed_sql = format!(
        "-- 010_seed_your_account.sql\n-- Auto-generated when this package was downloaded. Pre-seeds YOUR account\n-- ({seed_email}) so you don't need to register again on this self-hosted\n-- instance — you already verified this email on AgentRaaS Cloud.\n-- Set your password using the link in SETUP_INSTRUCTIONS.txt.\n\nINSERT INTO users (email, password_hash, is_admin, email_verified)\nVALUES ('{escaped_email}', '{unusable_password_hash}', true, true)\nON CONFLICT (email) DO NOTHING;\n\nINSERT INTO password_reset_tokens (user_id, token_hash, expires_at)\nSELECT id, '{setup_token_hash}', NOW() + INTERVAL '7 days'\nFROM users WHERE email = '{escaped_email}';\n"
    );
    let instructions_txt = format!(
        "AgentRaaS self-host — your account is pre-registered\n=====================================================\n\nEmail: {seed_email}\n\nThis package was generated for your account, so you don't need to\nregister again. After running ./install.sh and the dashboard is up:\n\n1. Go to http://localhost:13000/dashboard\n2. Click \"Forgot password\" and enter: {seed_email}\n   -- OR --\n   Use this direct link (valid 7 days, works once):\n   http://localhost:13000/dashboard?reset_token={raw_setup_token}\n\nEither way, you'll set a new password for THIS self-hosted instance\n(separate from your AgentRaaS Cloud password) and be logged in as an\nadmin on this deployment.\n\nIf your instance runs on a different host/port than localhost:13000,\nreplace that part of the URL above accordingly.\n"
    );

    let archive_bytes = tokio::task::spawn_blocking(move || build_zip_archive(seed_sql, instructions_txt))
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Zip packaging failed."))?
        .map_err(|err| {
            tracing::error!(err, "zip packaging failed");
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Zip packaging failed.")
        })?;

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/zip".parse().unwrap());
    headers.insert(header::CONTENT_DISPOSITION, "attachment; filename=\"agentraas-self-host.zip\"".parse().unwrap());
    Ok((headers, archive_bytes))
}

