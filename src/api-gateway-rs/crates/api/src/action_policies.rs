//! What each agent is allowed to do (SPEC-ACTION-POLICIES.md). Rules use
//! the validation-rule field syntax (`agentraas_core::validator`), and every
//! matching policy applies. Not under `ee/`, same reasoning as spend caps:
//! a safety control on the org's own agents, available on every tier. Only
//! `on_violation = "hitl"` needs Team + the enterprise build, checked at
//! creation time so enforcement never meets a rule it can't act on.

use agentraas_core::validator::{host_of, is_valid_rule_definition, validate_fields};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::db::{check_org_write_permission, get_user_org_ids, require_tier};
use crate::auth::{check_dashboard_rate_limit, is_valid_action_name, is_valid_identifier, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/action-policies", post(create_policy).get(list_policies))
        .route("/api/v1/action-policies/:id", axum::routing::delete(delete_policy))
}

#[derive(Deserialize)]
struct CreatePolicyBody {
    org_id: String,
    agent_id: Option<String>,
    service: String,
    action: String,
    effect: String,
    fields: Option<Value>,
    on_violation: Option<String>,
}

fn unprocessable(msg: &str) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, msg)
}

async fn create_policy(State(state): State<SharedState>, user: AuthUser, Json(body): Json<CreatePolicyBody>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    if !is_valid_identifier(&body.org_id) {
        return Err(unprocessable("org_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !check_org_write_permission(&state.pg, user.sub, &body.org_id).await? {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Auditors have read-only access to this org."));
    }
    if body.agent_id.as_deref().is_some_and(|a| !is_valid_identifier(a)) {
        return Err(unprocessable("agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if !is_valid_identifier(&body.service) {
        return Err(unprocessable("service must be 1-100 characters, letters/numbers/underscore/hyphen only."));
    }
    if body.action != "*" && !is_valid_action_name(&body.action) {
        return Err(unprocessable("action must be \"*\" or 1-100 characters, letters/numbers/dots/underscore/hyphen only."));
    }
    let fields = match (body.effect.as_str(), &body.fields) {
        ("deny", None | Some(Value::Null)) => None,
        ("deny", Some(_)) => return Err(unprocessable("A deny policy has no fields; use effect \"require\" to limit what a call may contain.")),
        ("require", Some(f)) => {
            if let Some(err) = is_valid_rule_definition(f) {
                return Err(unprocessable(&err));
            }
            Some(f.clone())
        }
        ("require", None) => return Err(unprocessable("A require policy needs fields.")),
        _ => return Err(unprocessable("effect must be \"deny\" or \"require\".")),
    };
    let on_violation = body.on_violation.unwrap_or_else(|| "block".to_string());
    if fields.as_ref().is_some_and(|f| !destination_fields(f).is_empty()) && on_violation != "hitl" {
        return Err(unprocessable("new_destination needs on_violation \"hitl\": approving a new destination is what adds it to the known list."));
    }
    match on_violation.as_str() {
        "block" => {}
        "hitl" => {
            require_tier(&state, &body.org_id, agentraas_core::tier::Tier::Team).await?;
            if !cfg!(feature = "enterprise") {
                return Err(ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Routing to approval needs the Enterprise-featured build (ee/); this deployment doesn't have it. Use on_violation=\"block\" instead.",
                ));
            }
        }
        _ => return Err(unprocessable("on_violation must be \"block\" or \"hitl\".")),
    }

    let id: i32 = sqlx::query_scalar(
        "INSERT INTO action_policies (org_id, agent_id, service, action, effect, fields, on_violation, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(&body.org_id)
    .bind(&body.agent_id)
    .bind(&body.service)
    .bind(&body.action)
    .bind(&body.effect)
    .bind(&fields)
    .bind(&on_violation)
    .bind(user.sub)
    .fetch_one(&state.pg)
    .await?;
    Ok(Json(json!({ "id": id })))
}

async fn list_policies(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "policies": [] })));
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'org_id', org_id, 'agent_id', agent_id, 'service', service,
                'action', action, 'effect', effect, 'fields', fields, 'on_violation', on_violation)
         FROM action_policies WHERE org_id = ANY($1) ORDER BY created_at DESC",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "policies": rows })))
}

async fn delete_policy(State(state): State<SharedState>, user: AuthUser, Path(id): Path<i32>) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    let deleted: Option<i32> = sqlx::query_scalar("DELETE FROM action_policies WHERE id = $1 AND org_id = ANY($2) RETURNING id")
        .bind(id)
        .bind(&org_ids)
        .fetch_optional(&state.pg)
        .await?;
    if deleted.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Action policy not found."));
    }
    Ok(Json(json!({ "deleted": true })))
}

// ─── enforcement (called from agent::mod::handle_request) ───

#[derive(sqlx::FromRow)]
pub struct Policy {
    pub id: i32,
    pub effect: String,
    pub fields: Option<Value>,
    pub on_violation: String,
}

pub struct Violation {
    /// "policy_denied", "policy_violation" or "policy_new_destination",
    /// the audit log's reason.
    pub reason: &'static str,
    pub hitl: bool,
    pub message: String,
}

/// Fields a policy marks `new_destination: true`.
fn destination_fields(fields: &Value) -> Vec<&str> {
    fields
        .as_object()
        .map(|o| o.iter().filter(|(_, r)| r.get("new_destination").and_then(Value::as_bool) == Some(true)).map(|(k, _)| k.as_str()).collect())
        .unwrap_or_default()
}

/// What counts as "the same destination": a URL's host, otherwise the
/// whole value (a full email address, phone number or account id), lowercased.
fn normalize_destination(s: &str) -> String {
    if s.contains("://") { host_of(s) } else { s.trim().to_ascii_lowercase() }
}

/// (field, normalized value) for every new_destination field one policy covers.
fn policy_destinations(p: &Policy, payload: &Value) -> Vec<(String, String)> {
    let Some(fields) = p.fields.as_ref().filter(|_| p.effect == "require") else { return vec![] };
    let mut out = vec![];
    for field in destination_fields(fields) {
        let values: Vec<&str> = match payload.get(field) {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        out.extend(values.into_iter().map(|v| (field.to_string(), normalize_destination(v))));
    }
    out
}

/// First violation that should win: any block beats any approval, so an
/// approver can never wave through a call another policy forbids.
/// `unknown` = destinations (field, value) this org has never approved.
pub fn evaluate(policies: &[Policy], service: &str, action: &str, payload: &Value, unknown: &[(String, String)]) -> Option<Violation> {
    let mut hitl = None;
    for p in policies {
        let (reason, message) = if p.effect == "deny" {
            ("policy_denied", format!("This agent may not call {service}.{action} (action policy #{})", p.id))
        } else if let Some(err) = p.fields.as_ref().and_then(|f| validate_fields(payload, f)) {
            ("policy_violation", format!("{err} (action policy #{})", p.id))
        } else if let Some((field, value)) = policy_destinations(p, payload).into_iter().find(|d| unknown.contains(d)) {
            ("policy_new_destination", format!("{field} ({value}) is a new destination for {service}; it needs approval once (action policy #{})", p.id))
        } else {
            continue;
        };
        let v = Violation { reason, hitl: p.on_violation == "hitl", message };
        if !v.hitl {
            return Some(v);
        }
        hitl.get_or_insert(v);
    }
    hitl
}

async fn matching_policies(state: &SharedState, org_id: &str, agent_id: Option<&str>, service: &str, action: &str) -> Result<Vec<Policy>, sqlx::Error> {
    // agent_id None = every agent's policies (used when remembering).
    sqlx::query_as(
        "SELECT id, effect, fields, on_violation FROM action_policies
         WHERE org_id = $1 AND service = $2 AND (action = $3 OR action = '*')
           AND ($4::text IS NULL OR agent_id = $4 OR agent_id IS NULL)
         ORDER BY id",
    )
    .bind(org_id)
    .bind(service)
    .bind(action)
    .bind(agent_id)
    .fetch_all(&state.pg)
    .await
}

pub async fn check(state: &SharedState, org_id: &str, agent_id: &str, service: &str, action: &str, payload: &Value) -> Result<Option<Violation>, sqlx::Error> {
    let policies = matching_policies(state, org_id, Some(agent_id), service, action).await?;
    let wanted: Vec<(String, String)> = policies.iter().flat_map(|p| policy_destinations(p, payload)).collect();
    let mut unknown = vec![];
    if !wanted.is_empty() {
        let (fields, values): (Vec<String>, Vec<String>) = wanted.iter().cloned().unzip();
        let known: Vec<(String, String)> = sqlx::query_as(
            "SELECT field, value FROM known_destinations WHERE org_id = $1 AND service = $2 AND field = ANY($3) AND value = ANY($4)",
        )
        .bind(org_id)
        .bind(service)
        .bind(&fields)
        .bind(&values)
        .fetch_all(&state.pg)
        .await?;
        unknown = wanted.into_iter().filter(|d| !known.contains(d)).collect();
    }
    Ok(evaluate(&policies, service, action, payload, &unknown))
}

/// Called once a human approved a call: its destinations become known, so
/// the next call to the same place goes straight through.
pub async fn remember_destinations(state: &SharedState, org_id: &str, service: &str, action: &str, payload: &Value) {
    let policies = match matching_policies(state, org_id, None, service, action).await {
        Ok(p) => p,
        Err(err) => return tracing::error!(?err, "remember_destinations: policy lookup failed"),
    };
    let (fields, values): (Vec<String>, Vec<String>) = policies.iter().flat_map(|p| policy_destinations(p, payload)).unzip();
    if fields.is_empty() {
        return;
    }
    let res = sqlx::query(
        "INSERT INTO known_destinations (org_id, service, field, value)
         SELECT $1, $2, f, v FROM unnest($3::text[], $4::text[]) AS t(f, v) ON CONFLICT DO NOTHING",
    )
    .bind(org_id)
    .bind(service)
    .bind(&fields)
    .bind(&values)
    .execute(&state.pg)
    .await;
    if let Err(err) = res {
        tracing::error!(?err, "remember_destinations insert failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: i32, effect: &str, fields: Option<Value>, on_violation: &str) -> Policy {
        Policy { id, effect: effect.into(), fields, on_violation: on_violation.into() }
    }

    #[test]
    fn no_policies_or_passing_payload_is_allowed() {
        assert!(evaluate(&[], "stripe", "refunds.create", &json!({}), &[]).is_none());
        let ps = [p(1, "require", Some(json!({ "amount": { "max": 50 } })), "block")];
        assert!(evaluate(&ps, "stripe", "refunds.create", &json!({ "amount": 20 }), &[]).is_none());
    }

    #[test]
    fn require_violation_and_deny_block() {
        let ps = [p(1, "require", Some(json!({ "amount": { "max": 50 } })), "block")];
        let v = evaluate(&ps, "stripe", "refunds.create", &json!({ "amount": 80 }), &[]).unwrap();
        assert_eq!((v.reason, v.hitl), ("policy_violation", false));
        assert!(v.message.ends_with("(action policy #1)"));
        assert!(v.message.contains("amount must be at most 50"));

        let v = evaluate(&[p(2, "deny", None, "block")], "hubspot", "contacts.delete", &json!({}), &[]).unwrap();
        assert_eq!(v.reason, "policy_denied");
    }

    #[test]
    fn a_block_beats_an_approval_whatever_the_order() {
        let ps = [p(1, "require", Some(json!({ "amount": { "max": 50 } })), "hitl"), p(2, "deny", None, "block")];
        let v = evaluate(&ps, "stripe", "refunds.create", &json!({ "amount": 80 }), &[]).unwrap();
        assert!(!v.hitl && v.message.contains("#2"));

        let v = evaluate(&ps[..1], "stripe", "refunds.create", &json!({ "amount": 80 }), &[]).unwrap();
        assert!(v.hitl);
    }

    #[test]
    fn new_destination_flags_only_unknown_values() {
        let ps = [p(1, "require", Some(json!({ "to": { "new_destination": true } })), "hitl")];
        let payload = json!({ "to": ["Boss@Acme.com", "x@evil.io"] });
        let wanted = policy_destinations(&ps[0], &payload);
        assert_eq!(wanted, vec![("to".to_string(), "boss@acme.com".to_string()), ("to".to_string(), "x@evil.io".to_string())]);

        assert!(evaluate(&ps, "resend", "emails.send", &payload, &[]).is_none(), "all known");
        let v = evaluate(&ps, "resend", "emails.send", &payload, &wanted[1..]).unwrap();
        assert_eq!((v.reason, v.hitl), ("policy_new_destination", true));
        assert!(v.message.contains("x@evil.io"));
        assert_eq!(normalize_destination("https://Hooks.Example.com/a?b"), "hooks.example.com");
    }
}
