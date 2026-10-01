//! OpenTelemetry export: every audit-log event also goes out as one OTLP
//! span, so AgentRaaS's decisions (ran, deduped, blocked, approved) show up
//! in whatever tracing backend a team already uses (Langfuse, Grafana,
//! Datadog, Honeycomb, Jaeger...). Off unless the standard
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (full URL) or
//! `OTEL_EXPORTER_OTLP_ENDPOINT` (base, `/v1/traces` appended) is set;
//! `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k2=v2`) carries auth. OTLP/HTTP JSON
//! over the existing reqwest, so no OpenTelemetry SDK dependency.
//!
//! Span naming follows the GenAI semantic conventions (`execute_tool`).
//! Calls with the same `run_id` share a trace id, so a run's steps line up.
//! The payload is never exported.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::util::configured_env;

struct Exporter {
    url: String,
    headers: Vec<(String, String)>,
    client: reqwest::Client,
}

fn exporter() -> Option<&'static Exporter> {
    static EXPORTER: OnceLock<Option<Exporter>> = OnceLock::new();
    EXPORTER
        .get_or_init(|| {
            let url = configured_env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
                .or_else(|| configured_env("OTEL_EXPORTER_OTLP_ENDPOINT").map(|b| format!("{}/v1/traces", b.trim_end_matches('/'))))?;
            let headers = configured_env("OTEL_EXPORTER_OTLP_HEADERS").map(|h| parse_headers(&h)).unwrap_or_default();
            let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(5)).build().ok()?;
            Some(Exporter { url, headers, client })
        })
        .as_ref()
}

fn parse_headers(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().replace("%20", " ")))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

fn hex_id(seed: &str, bytes: usize) -> String {
    hex::encode(&Sha256::digest(seed.as_bytes())[..bytes])
}

pub struct Event<'a> {
    pub req_id: &'a str,
    pub org_id: &'a str,
    pub agent_id: &'a str,
    pub service: &'a str,
    pub action: &'a str,
    pub status: &'a str,
    pub error_type: Option<&'a str>,
    pub duration_ms: i64,
    pub run_id: Option<&'a str>,
    pub step_id: Option<&'a str>,
}

fn span_body(e: &Event, end_ns: u128) -> Value {
    let start_ns = end_ns.saturating_sub(e.duration_ms.max(0) as u128 * 1_000_000);
    let tool = format!("{}.{}", e.service, e.action);
    let mut attrs = vec![
        ("gen_ai.operation.name", "execute_tool"),
        ("gen_ai.tool.name", tool.as_str()),
        ("gen_ai.agent.id", e.agent_id),
        ("agentraas.org_id", e.org_id),
        ("agentraas.req_id", e.req_id),
        ("agentraas.status", e.status),
    ];
    if let Some(v) = e.error_type {
        attrs.push(("error.type", v));
    }
    if let Some(v) = e.run_id {
        attrs.push(("agentraas.run_id", v));
    }
    if let Some(v) = e.step_id {
        attrs.push(("agentraas.step_id", v));
    }
    let attributes: Vec<Value> = attrs.iter().map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } })).collect();
    // 2 = ERROR for blocked/error/outcome unknown; a dedup is the product working, not a failure.
    let status = if matches!(e.status, "success" | "deduplicated") { json!({}) } else { json!({ "code": 2, "message": e.error_type.unwrap_or(e.status) }) };
    json!({ "resourceSpans": [{
        "resource": { "attributes": [{ "key": "service.name", "value": { "stringValue": "agentraas" } }] },
        "scopeSpans": [{
            "scope": { "name": "agentraas" },
            "spans": [{
                "traceId": hex_id(e.run_id.unwrap_or(e.req_id), 16),
                "spanId": hex_id(e.req_id, 8),
                "name": format!("execute_tool {tool}"),
                "kind": 3,
                "startTimeUnixNano": start_ns.to_string(),
                "endTimeUnixNano": end_ns.to_string(),
                "attributes": attributes,
                "status": status,
            }]
        }]
    }]})
}

/// Fire-and-forget: a slow or down collector never delays or fails a call.
// ponytail: one POST per event, add batching if a collector ever complains about volume.
pub fn export(e: Event) {
    let Some(exp) = exporter() else { return };
    let end_ns = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let body = span_body(&e, end_ns);
    tokio::spawn(async move {
        let mut req = exp.client.post(&exp.url).json(&body);
        for (k, v) in &exp.headers {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(r) if !r.status().is_success() => tracing::warn!(status = %r.status(), "OTLP export rejected"),
            Err(err) => tracing::warn!(?err, "OTLP export failed"),
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_groups_by_run_and_marks_non_success() {
        let e = |req, status, run| Event { req_id: req, org_id: "o", agent_id: "a", service: "stripe", action: "refunds.create", status, error_type: Some("policy_denied"), duration_ms: 12, run_id: run, step_id: None };
        let a = span_body(&e("r1", "blocked", Some("run9")), 1_000_000_000_000);
        let b = span_body(&e("r2", "success", Some("run9")), 1_000_000_000_000);
        let span = |v: &Value| v["resourceSpans"][0]["scopeSpans"][0]["spans"][0].clone();
        assert_eq!(span(&a)["traceId"], span(&b)["traceId"]);
        assert_ne!(span(&a)["spanId"], span(&b)["spanId"]);
        assert_eq!(span(&a)["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(span(&a)["name"], "execute_tool stripe.refunds.create");
        assert_eq!(span(&a)["status"]["code"], 2);
        assert_eq!(span(&b)["status"], json!({}));
        assert_eq!(span(&a)["startTimeUnixNano"], "999988000000");
        assert_eq!(parse_headers("Authorization=Basic%20abc, x-team = 1"), vec![("Authorization".into(), "Basic abc".into()), ("x-team".into(), "1".into())]);
    }
}
