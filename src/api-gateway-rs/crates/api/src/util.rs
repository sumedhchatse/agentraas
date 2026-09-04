/// Matches JS's `new Date().toISOString()` exactly (millisecond precision,
/// literal `Z`) rather than chrono's default RFC3339 (nanosecond precision,
/// `+00:00` offset) — every timestamp string this server puts in a JSON
/// response body should look identical to the same field from Node.
pub fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Header names, per RFC 7230 token chars (no spaces/colons) — mirrors
/// `isValidHeaderName` in `server.js`.
pub fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 100
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
        })
}

fn is_private_or_reserved_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127
                || o[0] == 10
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 172 && (16..=31).contains(&o[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// SSRF guard for custom-action target URLs / notification webhook URLs —
/// mirrors `validateTargetUrl` in `server.js`. Returns `Some(error message)`
/// on rejection, `None` if the URL is safe to register.
pub async fn validate_target_url(target_url: &str) -> Option<String> {
    let Ok(parsed) = url::Url::parse(target_url) else {
        return Some("Invalid URL.".to_string());
    };
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Some("Only http:// and https:// URLs are allowed.".to_string());
    }
    let Some(hostname) = parsed.host_str() else {
        return Some("Invalid URL.".to_string());
    };
    let hostname = hostname.to_lowercase();
    const BLOCKED: &[&str] = &["localhost", "ar-postgres", "ar-redis", "ar-minio", "ar-api"];
    if BLOCKED.contains(&hostname.as_str()) || hostname.ends_with(".local") {
        return Some("Local or internal hostnames are not allowed.".to_string());
    }

    let lookup_target = format!("{hostname}:0");
    let lookup_result = tokio::net::lookup_host(&lookup_target).await;
    match lookup_result {
        Ok(addrs) => {
            for addr in addrs {
                if is_private_or_reserved_ip(&addr.ip()) {
                    return Some("Target resolves to a private/internal IP address, which is not allowed.".to_string());
                }
            }
            None
        }
        Err(_) => Some("Could not resolve the target hostname.".to_string()),
    }
}
