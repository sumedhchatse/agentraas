//! Prometheus `/metrics`: call counts and handling time per service and
//! outcome, fed from the same audit-log hook as the OTLP export. Off (404)
//! unless `METRICS_TOKEN` is set; scrapers send it as a bearer token
//! (`authorization: { credentials: ... }` in prometheus.yml). No org label:
//! on the multi-tenant cloud one token must not reveal per-customer traffic.
//! Counters live in process memory and reset on restart, as Prometheus expects.

use std::collections::BTreeMap;
use std::sync::Mutex;

use axum::{http::HeaderMap, http::StatusCode, response::IntoResponse, routing::get, Router};
use sha2::{Digest, Sha256};

use crate::state::SharedState;
use crate::util::configured_env;

const BUCKETS_MS: [f64; 11] = [5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0];

#[derive(Default)]
struct Series {
    count: u64,
    sum_ms: f64,
    buckets: [u64; BUCKETS_MS.len()],
}

// ponytail: one global mutex, held for a few ns per call; shard it if a profile ever shows contention.
// Labels are (service, status); service is bounded by the registered services/actions.
static SERIES: Mutex<BTreeMap<(String, String), Series>> = Mutex::new(BTreeMap::new());

pub fn record(service: &str, status: &str, duration_ms: i64) {
    let ms = duration_ms.max(0) as f64;
    let mut map = SERIES.lock().unwrap_or_else(|e| e.into_inner());
    let s = map.entry((service.to_string(), status.to_string())).or_default();
    s.count += 1;
    s.sum_ms += ms;
    for (i, b) in BUCKETS_MS.iter().enumerate() {
        if ms <= *b {
            s.buckets[i] += 1;
        }
    }
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn render() -> String {
    let map = SERIES.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::from(
        "# HELP agentraas_calls_total Agent calls handled, by service and outcome.\n# TYPE agentraas_calls_total counter\n",
    );
    for ((svc, st), s) in map.iter() {
        out += &format!("agentraas_calls_total{{service=\"{}\",status=\"{}\"}} {}\n", esc(svc), esc(st), s.count);
    }
    out += "# HELP agentraas_call_duration_ms Time from receiving a call to answering it, in milliseconds.\n# TYPE agentraas_call_duration_ms histogram\n";
    for ((svc, st), s) in map.iter() {
        let l = format!("service=\"{}\",status=\"{}\"", esc(svc), esc(st));
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            out += &format!("agentraas_call_duration_ms_bucket{{{l},le=\"{b}\"}} {}\n", s.buckets[i]);
        }
        out += &format!("agentraas_call_duration_ms_bucket{{{l},le=\"+Inf\"}} {}\n", s.count);
        out += &format!("agentraas_call_duration_ms_sum{{{l}}} {}\n", s.sum_ms);
        out += &format!("agentraas_call_duration_ms_count{{{l}}} {}\n", s.count);
    }
    out
}

async fn metrics(headers: HeaderMap) -> impl IntoResponse {
    let Some(token) = configured_env("METRICS_TOKEN") else {
        return (StatusCode::NOT_FOUND, [("content-type", "text/plain")], String::new());
    };
    let given = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    // Compare digests so the check takes the same time however much of the token matches.
    if Sha256::digest(given.as_bytes()) != Sha256::digest(token.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, [("content-type", "text/plain")], String::new());
    }
    (StatusCode::OK, [("content-type", "text/plain; version=0.0.4")], render())
}

pub fn router() -> Router<SharedState> {
    Router::new().route("/metrics", get(metrics))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_is_cumulative_and_escaped() {
        record("svc\"x", "deduplicated", 7);
        record("svc\"x", "deduplicated", 300);
        let out = render();
        assert!(out.contains("agentraas_calls_total{service=\"svc\\\"x\",status=\"deduplicated\"} 2"));
        assert!(out.contains("status=\"deduplicated\",le=\"5\"} 0"));
        assert!(out.contains("status=\"deduplicated\",le=\"10\"} 1"));
        assert!(out.contains("status=\"deduplicated\",le=\"500\"} 2"));
        assert!(out.contains("status=\"deduplicated\",le=\"+Inf\"} 2"));
        assert!(out.contains("agentraas_call_duration_ms_sum{service=\"svc\\\"x\",status=\"deduplicated\"} 307"));
    }
}
