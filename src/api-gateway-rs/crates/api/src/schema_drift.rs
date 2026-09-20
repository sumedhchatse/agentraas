//! DB-backed orchestration for upstream contract/schema drift detection
//! (SPEC-SCHEMA-DRIFT.md). Pure fingerprinting/comparison logic lives in
//! `agentraas_core::schema_drift`; this module owns reading/writing the
//! shared-per-service baseline and firing the notification.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::agent::db::get_user_org_ids;
use crate::auth::{check_dashboard_rate_limit, AuthUser};
use crate::state::{ApiError, SharedState};

pub fn router() -> Router<SharedState> {
    Router::new().route("/api/v1/schema-drift-events", get(list_events))
}

/// Called from `agent::mod`'s successful-forward path, spawned as a
/// background task so this never adds latency to the actual response —
/// best-effort throughout, any failure here is swallowed (logged, not
/// propagated), same as `notify_circuit_open`'s call site already is.
pub async fn check_and_record(state: &SharedState, org_id: &str, req_id: &str, service: &str, action: &str, response_body: &Value) {
    let Some(current) = agentraas_core::schema_drift::fingerprint(response_body) else { return };

    #[derive(sqlx::FromRow)]
    struct BaselineRow {
        field_paths: Value,
    }
    let existing: Option<BaselineRow> = match sqlx::query_as("SELECT field_paths FROM schema_baselines WHERE service = $1 AND action = $2")
        .bind(service)
        .bind(action)
        .fetch_optional(&state.pg)
        .await
    {
        Ok(row) => row,
        Err(err) => {
            tracing::warn!(?err, service, action, "schema_drift: baseline lookup failed");
            return;
        }
    };

    let Some(existing) = existing else {
        // First observation for this service.action - establish the
        // baseline, nothing to compare against yet.
        let _ = sqlx::query("INSERT INTO schema_baselines (service, action, field_paths) VALUES ($1, $2, $3) ON CONFLICT (service, action) DO NOTHING")
            .bind(service)
            .bind(action)
            .bind(json!(current))
            .execute(&state.pg)
            .await;
        return;
    };

    let baseline: Vec<String> = serde_json::from_value(existing.field_paths).unwrap_or_default();
    let result = agentraas_core::schema_drift::compare(&baseline, &current);

    if result.removed.is_empty() && result.type_changed.is_empty() {
        // Still absorb any newly-seen benign fields into the baseline.
        if result.next_baseline.len() != baseline.len() {
            let _ = sqlx::query("UPDATE schema_baselines SET field_paths = $1, sample_count = sample_count + 1, last_updated_at = NOW() WHERE service = $2 AND action = $3")
                .bind(json!(result.next_baseline))
                .bind(service)
                .bind(action)
                .execute(&state.pg)
                .await;
        }
        return;
    }

    // Real drift - record it, update the baseline so this exact change
    // doesn't keep re-triggering, and notify.
    let _ = sqlx::query(
        "INSERT INTO schema_drift_events (service, action, org_id, req_id, removed_fields, type_changed_fields) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(service)
    .bind(action)
    .bind(org_id)
    .bind(req_id)
    .bind(json!(result.removed))
    .bind(json!(result.type_changed))
    .execute(&state.pg)
    .await;

    let _ = sqlx::query("UPDATE schema_baselines SET field_paths = $1, sample_count = sample_count + 1, last_updated_at = NOW() WHERE service = $2 AND action = $3")
        .bind(json!(result.next_baseline))
        .bind(service)
        .bind(action)
        .execute(&state.pg)
        .await;

    crate::notifications::notify_schema_drift(state, org_id, service, action, &result.removed, &result.type_changed).await;
}

async fn list_events(State(state): State<SharedState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    check_dashboard_rate_limit(&state, user.sub).await?;
    let org_ids = get_user_org_ids(&state.pg, user.sub).await?;
    if org_ids.is_empty() {
        return Ok(Json(json!({ "events": [] })));
    }
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'service', service, 'action', action, 'removed_fields', removed_fields,
                'type_changed_fields', type_changed_fields, 'detected_at', detected_at)
         FROM schema_drift_events WHERE org_id = ANY($1) ORDER BY detected_at DESC LIMIT 50",
    )
    .bind(&org_ids)
    .fetch_all(&state.pg)
    .await?;
    Ok(Json(json!({ "events": rows })))
}
