//! Static landing/dashboard/doc/SEO pages — mirrors the "STATIC LANDING
//! PAGE"/"STATIC DASHBOARD"/"STATIC GUIDE"/"SEO" sections of `server.js`.
//! Reads from `/public` (mounted read-only from `src/api-gateway/public`)
//! and `/repo` (the self-host snapshot source — see `self_host.rs`) at
//! request time, same as Node reads from disk per-request rather than
//! embedding at build time.

use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::SharedState;

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/", get(landing))
        .route("/dashboard", get(dashboard))
        .route("/dashboard/", get(dashboard))
        .route("/guide", get(guide))
        .route("/webhook-audit", get(webhook_audit_page))
        .route("/status", get(status_page))
        .route("/vendor/chart.umd.min.js", get(vendor_chart_js))
        .route("/robots.txt", get(robots_txt))
        .route("/sitemap.xml", get(sitemap_xml))
        .route("/api/v1/public/status", get(public_status))
        .route("/license", get(doc_license))
        .route("/privacy", get(doc_privacy))
        .route("/terms", get(doc_terms))
        .route("/security", get(doc_security))
        .route("/readme", get(doc_readme))
}

fn public_file(name: &str) -> std::path::PathBuf {
    std::path::Path::new("/public").join(name)
}

async fn serve_html_file(path: std::path::PathBuf, not_found_message: &'static str) -> axum::response::Response {
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => Html(contents).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({ "error": not_found_message }))).into_response(),
    }
}

async fn landing() -> axum::response::Response {
    serve_html_file(public_file("landing.html"), "Landing page not found").await
}

async fn dashboard() -> axum::response::Response {
    serve_html_file(public_file("index.html"), "Dashboard not found").await
}

async fn guide() -> axum::response::Response {
    serve_html_file(public_file("guide.html"), "Guide not found").await
}

async fn webhook_audit_page() -> axum::response::Response {
    serve_html_file(public_file("webhook-audit.html"), "Webhook audit tool not found").await
}

async fn status_page() -> axum::response::Response {
    serve_html_file(public_file("status.html"), "Status page not found").await
}

async fn vendor_chart_js() -> axum::response::Response {
    let path = public_file("vendor/chart.umd.min.js");
    match tokio::fs::read_to_string(&path).await {
        Ok(contents) => ([(header::CONTENT_TYPE, "application/javascript")], contents).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({ "error": "chart.umd.min.js not found on this deployment." }))).into_response(),
    }
}

async fn robots_txt(axum::extract::State(state): axum::extract::State<SharedState>) -> impl IntoResponse {
    let body = format!("User-agent: *\nAllow: /\nDisallow: /dashboard\nSitemap: {}/sitemap.xml\n", state.public_url);
    ([(header::CONTENT_TYPE, "text/plain")], body)
}

const SITEMAP_ROUTES: &[&str] = &["/", "/guide", "/webhook-audit", "/status", "/license", "/privacy", "/terms", "/security", "/readme"];

async fn sitemap_xml(axum::extract::State(state): axum::extract::State<SharedState>) -> impl IntoResponse {
    let urls: String = SITEMAP_ROUTES.iter().map(|route| format!("  <url><loc>{}{route}</loc></url>", state.public_url)).collect::<Vec<_>>().join("\n");
    let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n{urls}\n</urlset>\n");
    ([(header::CONTENT_TYPE, "application/xml")], body)
}

/// Unauthenticated, deliberately coarse: circuit state and uptime are
/// already platform-wide, so nothing org-specific ever appears here.
/// Internal-only services (mockpay) are excluded. Fixed 90-day window.
async fn public_status(axum::extract::State(state): axum::extract::State<SharedState>) -> Result<Json<serde_json::Value>, crate::state::ApiError> {
    let services: Vec<String> = state
        .service_routes
        .iter()
        .filter(|(_, r)| !r.internal)
        .map(|(k, _)| k.split('.').next().unwrap_or(k).to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let uptime_rows: Vec<(String, f64)> = sqlx::query_as(
        "WITH events AS (
           SELECT service, to_state, occurred_at,
                  LEAD(occurred_at) OVER (PARTITION BY service ORDER BY occurred_at) AS next_at
           FROM circuit_breaker_events
           WHERE service = ANY($1) AND occurred_at >= NOW() - INTERVAL '90 days'
         )
         SELECT service,
                COALESCE(SUM(EXTRACT(EPOCH FROM (LEAST(COALESCE(next_at, NOW()), NOW()) - occurred_at))) FILTER (WHERE to_state = 'open'), 0)::float8 AS open_seconds
         FROM events GROUP BY service",
    )
    .bind(&services)
    .fetch_all(&state.pg)
    .await?;
    let open_seconds_map: std::collections::HashMap<String, f64> = uptime_rows.into_iter().collect();

    let mut conn = state.redis.get_multiplexed_async_connection().await?;
    let (circuit_states, _) = agentraas_core::circuit_breaker::get_circuit_states_batch(&mut conn, &services).await?;

    const RANGE_SECONDS: f64 = 90.0 * 86400.0;
    let mut report: Vec<serde_json::Value> = services
        .iter()
        .map(|svc| {
            let open_seconds = open_seconds_map.get(svc).copied().unwrap_or(0.0).min(RANGE_SECONDS);
            let uptime_pct = ((1.0 - open_seconds / RANGE_SECONDS) * 10000.0).round() / 100.0;
            let circuit_state = circuit_states.get(svc).cloned().unwrap_or_else(|| "closed".to_string());
            let status = match circuit_state.as_str() {
                "open" => "down",
                "half-open" => "degraded",
                _ => "operational",
            };
            (svc.clone(), status, uptime_pct)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|(svc, status, uptime_pct)| json!({ "service": svc, "status": status, "uptime_90d": uptime_pct }))
        .collect();
    report.sort_by(|a, b| a["service"].as_str().unwrap_or("").cmp(b["service"].as_str().unwrap_or("")));

    let overall = if report.iter().any(|s| s["status"] == "down") {
        "major_outage"
    } else if report.iter().any(|s| s["status"] == "degraded") {
        "degraded"
    } else {
        "operational"
    };

    Ok(Json(json!({ "overall": overall, "generated_at": crate::util::iso_now(), "services": report })))
}

/// Legal/docs documents, served as styled HTML (client-side markdown via
/// marked.js from a CDN, matching Node) rather than raw plaintext. Read
/// from the self-host snapshot dir (`crate::self_host::SNAPSHOT_DIR`),
/// same source Node reads from.
async fn doc_page(filename: &str, title: &str) -> axum::response::Response {
    let path = std::path::Path::new(crate::self_host::SNAPSHOT_DIR).join(filename);
    match tokio::fs::read_to_string(&path).await {
        Ok(raw_markdown) => Html(render_doc_page(title, &raw_markdown)).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("{filename} not found on this deployment.") }))).into_response(),
    }
}

async fn doc_license() -> axum::response::Response {
    doc_page("LICENSE.md", "License").await
}
async fn doc_privacy() -> axum::response::Response {
    doc_page("PRIVACY.md", "Privacy policy").await
}
async fn doc_terms() -> axum::response::Response {
    doc_page("TERMS.md", "Terms of service").await
}
async fn doc_security() -> axum::response::Response {
    doc_page("SECURITY.md", "Security").await
}
async fn doc_readme() -> axum::response::Response {
    doc_page("README.md", "Documentation").await
}

/// Byte-for-byte the same table-based inline-styled layout as
/// `renderDocPage()` in server.js.
fn render_doc_page(title: &str, raw_markdown: &str) -> String {
    // Mirrors `JSON.stringify(rawMarkdown).replace(/<\/script/gi, ...)` —
    // safely escapes for embedding in a script tag.
    let safe_markdown = serde_json::to_string(raw_markdown).unwrap().replace("</script", "<\\/script").replace("</SCRIPT", "<\\/SCRIPT");
    format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{title} — AgentRaaS</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'%3E%3Cpath d='M12 2.5L4.5 5.5V11C4.5 16 7.8 20.2 12 21.5C16.2 20.2 19.5 16 19.5 11V5.5L12 2.5Z' fill='%2306231C' stroke='%2300E0A8' stroke-width='0.8'/%3E%3Ccircle cx='12' cy='11.5' r='2.4' fill='%2300E0A8'/%3E%3C/svg%3E">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@500;600;700;800&family=Manrope:wght@400;500;600;700;800&display=swap" rel="stylesheet">
<style>
  :root {{
    --ink: #0A0D14; --ink-2: #0F1420;
    --border-dark: #232A3A;
    --text: #F5F6F9; --muted: #8B93A6;
    --signal: #00E0A8;
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ background: var(--ink); color: var(--text); font-family: 'Manrope', sans-serif; font-size: 16px; line-height: 1.7; }}
  .wrap {{ max-width: 820px; margin: 0 auto; padding: 60px 32px 100px; }}
  a {{ color: var(--signal); }}
  header.nav {{ border-bottom: 1px solid var(--border-dark); padding: 18px 32px; display: flex; align-items: center; justify-content: space-between; }}
  .brand {{ display: flex; align-items: center; gap: 10px; text-decoration: none; color: var(--text); }}
  .brand .logo {{ width: 34px; height: 34px; background: var(--signal); border-radius: 9px; display: flex; align-items: center; justify-content: center; }}
  .brand h1 {{ font-family: 'Space Grotesk', sans-serif; font-size: 18px; font-weight: 700; }}
  .brand span {{ display: block; font-size: 12px; color: var(--muted); font-weight: 500; }}
  .back-link {{ font-size: 14px; color: var(--muted); }}
  #doc-content h1 {{ font-family: 'Space Grotesk', sans-serif; font-size: 34px; margin-bottom: 20px; }}
  #doc-content h2 {{ font-family: 'Space Grotesk', sans-serif; font-size: 24px; margin: 36px 0 14px; }}
  #doc-content h3 {{ font-family: 'Space Grotesk', sans-serif; font-size: 17px; margin: 24px 0 10px; color: var(--signal); }}
  #doc-content p {{ margin-bottom: 14px; color: #C5CAD6; }}
  #doc-content ol, #doc-content ul {{ margin: 0 0 16px 22px; color: #C5CAD6; }}
  #doc-content li {{ margin-bottom: 6px; }}
  #doc-content code {{ background: var(--ink-2); border: 1px solid var(--border-dark); border-radius: 5px; padding: 2px 6px; font-size: 14px; font-family: 'Space Grotesk', monospace; color: var(--signal); }}
  #doc-content pre {{ background: var(--ink-2); border: 1px solid var(--border-dark); border-radius: 10px; padding: 18px 20px; overflow-x: auto; margin-bottom: 16px; }}
  #doc-content pre code {{ background: none; border: none; padding: 0; color: #C5CAD6; }}
  #doc-content hr {{ border: none; border-top: 1px solid var(--border-dark); margin: 32px 0; }}
  #doc-content table {{ width: 100%; border-collapse: collapse; margin-bottom: 20px; }}
  #doc-content th, #doc-content td {{ text-align: left; padding: 10px 14px; border-bottom: 1px solid var(--border-dark); font-size: 14.5px; }}
  #doc-content blockquote {{ border-left: 3px solid var(--signal); padding-left: 16px; color: var(--muted); margin-bottom: 16px; }}
</style>
</head>
<body>
<header class="nav">
  <a href="/" class="brand">
    <span class="logo"><svg width="18" height="18" viewBox="0 0 24 24"><path d="M12 2.5L4.5 5.5V11C4.5 16 7.8 20.2 12 21.5C16.2 20.2 19.5 16 19.5 11V5.5L12 2.5Z" fill="#06231C" stroke="#00E0A8" stroke-width="1"/><circle cx="12" cy="11.5" r="2.6" fill="#00E0A8"/></svg></span>
    <div><h1>AgentRaaS</h1><span>Agent Reliability as a Service</span></div>
  </a>
  <a href="/dashboard" class="back-link">← Back to dashboard</a>
</header>
<div class="wrap">
  <div id="doc-content">Loading…</div>
</div>
<script src="https://cdnjs.cloudflare.com/ajax/libs/marked/12.0.0/marked.min.js"></script>
<script>
  const rawMarkdown = {safe_markdown};
  document.getElementById('doc-content').innerHTML = marked.parse(rawMarkdown);
</script>
</body>
</html>"##
    )
}
