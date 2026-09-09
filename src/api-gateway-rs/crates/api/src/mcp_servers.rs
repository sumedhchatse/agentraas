//! MCP Custom Actions — register a third-party MCP server so every tool it
//! exposes gets AgentRaaS's dedup/circuit-breaker/audit pipeline, the same
//! value HTTP Custom Actions (`custom_actions.rs`) already give a plain
//! REST endpoint. CRUD only here — the resolver (`agent/db.rs::
//! resolve_mcp_tool_route`) and forwarder (`agent/forward.rs::
//! forward_mcp_tool_call`) are what actually proxy a call through it;
//! `mcp.rs` is what wires both into `tools/list`/`tools/call`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_agency_tenant_cap, check_org_write_permission};
use crate::auth::{check_dashboard_rate_limit, is_valid_identifier, AuthUser};
use crate::credentials::save_credential;
use crate::state::{ApiError, SharedState};
use crate::util::validate_target_url;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/mcp-servers", post(create_mcp_server).get(list_mcp_servers))
        .route("/api/v1/mcp-servers/:id", delete(revoke_mcp_server))
}

#[derive(Deserialize)]
struct CreateMcpServerBody {
    org_id: Option<String>,
    name: Option<String>,
    target_url: Option<String>,
    auth_type: Option<String>,
    auth_header_name: Option<String>,
    credential: Option<Value>,
}

async fn create_mcp_server(
    State(state): State<SharedState>,
    user: AuthUser,
    Json(body): Json<CreateMcpServerBody>,
) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;

    let org_id = body.org_id.unwrap_or_default();
    let name = body.name.unwrap_or_default();
    if !is_valid_identifier(&org_id) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !is_valid_identifier(&name) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "name must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if name == "custom" {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "\"custom\" is reserved as the service keyword — pick a different name."));
    }
    if state.service_routes.keys().any(|k| k.split_once('.').map(|(svc, _)| svc) == Some(name.as_str())) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("\"{name}\" collides with a curated service name — pick a different name.")));
    }
    if !check_org_write_permission(&state.pg, user.sub, &org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    let tenant_cap = check_agency_tenant_cap(&state, user.sub, &org_id).await?;
    if !tenant_cap.ok {
        return Err(ApiError::new(
            StatusCode::PAYMENT_REQUIRED,
            format!("Agency plan is limited to {} client tenants. Contact hello@agentraas.io to increase this.", tenant_cap.limit),
        ));
    }

    const ALLOWED_AUTH_TYPES: &[&str] = &["none", "bearer", "basic", "header"];
    let auth_type = body.auth_type.unwrap_or_else(|| "none".to_string());
    if !ALLOWED_AUTH_TYPES.contains(&auth_type.as_str()) {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("auth_type must be one of: {}", ALLOWED_AUTH_TYPES.join(", "))));
    }
    if auth_type == "header" && body.auth_header_name.as_deref().unwrap_or("").is_empty() {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "auth_header_name is required when auth_type is \"header\"."));
    }
    let credential_valid = body.credential.as_ref().is_some_and(|c| c.as_object().is_some_and(|o| !o.is_empty()));
    if auth_type != "none" && !credential_valid {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "A credential is required when auth_type is not \"none\"."));
    }

    let target_url = body.target_url.unwrap_or_default();
    if let Some(err) = validate_target_url(&target_url).await {
        return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, err));
    }

    let mut tx = state.pg.begin().await?;
    sqlx::query("UPDATE custom_mcp_servers SET revoked_at = NOW() WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL")
        .bind(&org_id)
        .bind(&name)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO custom_mcp_servers (user_id, org_id, name, target_url, auth_type, auth_header_name)
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(user.sub)
    .bind(&org_id)
    .bind(&name)
    .bind(&target_url)
    .bind(&auth_type)
    .bind(&body.auth_header_name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    if auth_type != "none" {
        if let Some(credential) = &body.credential {
            save_credential(&state, user.sub, &org_id, &format!("mcp:{name}"), credential).await?;
        }
    }

    // Auto-discovered schemas: probe the server right now rather than
    // trusting the URL blindly — the org gets immediate confirmation it
    // actually speaks MCP, and a real tool count/list instead of having to
    // guess or hand-type one.
    let discovered = crate::mcp::probe_mcp_server_tools(&state, &org_id, &name, &target_url, &auth_type, body.auth_header_name.as_deref()).await;
    let discovered_names: Vec<&str> = discovered.iter().filter_map(|t| t.get("name").and_then(Value::as_str)).collect();
    let note = if discovered.is_empty() {
        "Saved, but no tools were discovered at that URL just now — double-check it's a working MCP tools/call endpoint. Tools will still be re-probed live on the next tools/list call.".to_string()
    } else {
        format!("Discovered {} tool(s): {}. Available now in tools/list.", discovered.len(), discovered_names.join(", "))
    };

    Ok(Json(json!({
        "saved": true,
        "org_id": org_id,
        "name": name,
        "discovered_tools": discovered_names,
        "note": note,
    })))
}

async fn list_mcp_servers(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Vec<Value>>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: i32,
        org_id: String,
        name: String,
        target_url: String,
        auth_type: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, org_id, name, target_url, auth_type, created_at
         FROM custom_mcp_servers WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC",
    )
    .bind(user.sub)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| json!({ "id": r.id, "org_id": r.org_id, "name": r.name, "target_url": r.target_url, "auth_type": r.auth_type, "created_at": r.created_at }))
            .collect(),
    ))
}

async fn revoke_mcp_server(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let revoked: Option<i32> = sqlx::query_scalar(
        "UPDATE custom_mcp_servers SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id",
    )
    .bind(id)
    .bind(user.sub)
    .fetch_optional(&state.pg)
    .await?;
    if revoked.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "MCP server not found."));
    }
    Ok(Json(json!({ "revoked": true })))
}
