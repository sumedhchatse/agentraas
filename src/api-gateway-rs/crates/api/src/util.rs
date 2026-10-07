/// Matches JS's `new Date().toISOString()` exactly (millisecond precision,
/// literal `Z`) rather than chrono's default RFC3339 (nanosecond precision,
/// `+00:00` offset), so every timestamp in a JSON response has one format
/// the dashboard and SDKs can rely on.
pub fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// `std::env::var` treats a present-but-empty variable as `Ok("")`, not
/// absent — compose.yaml declares every optional secret/config var with
/// a `${VAR:-}` default, so an unset one still arrives as an empty
/// string inside the container. Both must read as "not configured" in every
/// "is this deployment set up for X" check (Paddle, pay-as-you-go billing,
/// and anything new).
pub fn configured_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Header names, per RFC 7230 token chars (no spaces/colons).
pub fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 100
        && name.chars().all(|c| {
            c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
        })
}

/// Real client IP, used for the per-IP login/register/audit rate limits.
/// Only a header set by a proxy we run is trusted: `CLIENT_IP_HEADER` names
/// it. Production (behind Cloudflare) uses `cf-connecting-ip`, which
/// Cloudflare sets on every request itself, so a client can't forge it.
/// Unset, the TCP peer address is used. X-Forwarded-For is never trusted by
/// default: since the Cloudflare Tunnel replaced Caddy, nothing strips a
/// client-supplied value, so its first entry was attacker-chosen and any
/// caller could dodge the rate limits.
pub fn real_client_ip(headers: &axum::http::HeaderMap, connect_addr: &std::net::SocketAddr) -> String {
    static HEADER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let header = HEADER.get_or_init(|| configured_env("CLIENT_IP_HEADER").map(|h| h.trim().to_ascii_lowercase()));
    client_ip_from(headers, connect_addr, header.as_deref())
}

fn client_ip_from(headers: &axum::http::HeaderMap, connect_addr: &std::net::SocketAddr, trusted_header: Option<&str>) -> String {
    trusted_header
        .and_then(|name| headers.get(name))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| connect_addr.ip().to_string())
}

fn is_private_or_reserved_v4(v4: &std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 0
        || o[0] == 127
        || o[0] == 10
        || (o[0] == 192 && o[1] == 168)
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24, IETF protocol assignments
        || (o[0] == 192 && o[1] == 88 && o[2] == 99) // 192.88.99.0/24, 6to4 relay anycast
        || (o[0] == 169 && o[1] == 254)
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        || (o[0] == 100 && (64..=127).contains(&o[1])) // 100.64.0.0/10, carrier-grade NAT
        || o[0] >= 224 // 224.0.0.0/4 multicast, 240.0.0.0/4 reserved, 255.255.255.255 broadcast
}

/// Extracts the IPv4 address embedded in an IPv6 transition-mechanism
/// address, if `v6` is one of the well-known forms that carries one:
/// 6to4 (`2002::/16`), Teredo (`2001:0000::/32`, embedded octets
/// obfuscated by XOR-0xff per RFC 4380), or NAT64 (`64:ff9b::/96`). Each
/// of these can otherwise be used to smuggle an arbitrary IPv4 address
/// (including a private/reserved one) past IPv6-only reserved-range
/// checks.
fn embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let s = v6.segments();
    if s[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8));
    }
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return Some(std::net::Ipv4Addr::new((s[6] >> 8) as u8 ^ 0xff, s[6] as u8 ^ 0xff, (s[7] >> 8) as u8 ^ 0xff, s[7] as u8 ^ 0xff));
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return Some(std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    None
}

fn is_private_or_reserved_ip(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => is_private_or_reserved_v4(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // Unwraps IPv4-mapped (`::ffff:a.b.c.d`) and the
                // transition-mechanism forms above so an embedded
                // loopback/link-local/metadata IPv4 address can't slip
                // past the IPv6-only checks.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_or_reserved_v4(&v4))
                || embedded_ipv4(v6).is_some_and(|v4| is_private_or_reserved_v4(&v4))
        }
    }
}

/// SSRF guard for custom-action target URLs / notification webhook URLs.
/// Returns `Some(error message)`
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

/// DNS resolver for the shared HTTP client: drops private/reserved addresses
/// at connect time, so a hostname that resolved public for
/// `validate_target_url` can't rebind to an internal one for the real request.
/// `localhost` is exempt: user URLs can't name it (rejected above), only the
/// app's own static config does (the internal mockpay route).
pub struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let allowed = filter_public(&host, addrs);
            if allowed.is_empty() {
                return Err(format!("{host} resolves only to private/internal addresses").into());
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn filter_public(host: &str, addrs: Vec<std::net::SocketAddr>) -> Vec<std::net::SocketAddr> {
    if host.eq_ignore_ascii_case("localhost") {
        return addrs;
    }
    addrs.into_iter().filter(|a| !is_private_or_reserved_ip(&a.ip())).collect()
}

#[cfg(test)]
mod public_only_resolver_tests {
    use super::filter_public;

    #[test]
    fn drops_private_addresses_except_for_localhost() {
        let addrs = vec!["10.0.0.5:0".parse().unwrap(), "169.254.169.254:0".parse().unwrap(), "93.184.216.34:0".parse().unwrap()];
        assert_eq!(filter_public("evil.example", addrs.clone()), vec!["93.184.216.34:0".parse().unwrap()]);
        assert!(filter_public("evil.example", vec!["127.0.0.1:0".parse().unwrap()]).is_empty());
        assert_eq!(filter_public("localhost", vec!["127.0.0.1:0".parse().unwrap()]).len(), 1);
    }
}

#[cfg(test)]
mod real_client_ip_tests {
    use super::client_ip_from;
    use axum::http::HeaderMap;
    use std::net::SocketAddr;

    fn connect_addr() -> SocketAddr {
        "127.0.0.1:1234".parse().unwrap()
    }

    #[test]
    fn uses_the_configured_trusted_header() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        assert_eq!(client_ip_from(&headers, &connect_addr(), Some("cf-connecting-ip")), "203.0.113.7");
    }

    #[test]
    fn a_forged_x_forwarded_for_is_ignored() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "6.6.6.6".parse().unwrap());
        headers.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        assert_eq!(client_ip_from(&headers, &connect_addr(), Some("cf-connecting-ip")), "203.0.113.7");
        assert_eq!(client_ip_from(&headers, &connect_addr(), None), "127.0.0.1", "unset: peer address, never XFF");
    }

    #[test]
    fn falls_back_to_connect_addr_when_trusted_header_missing_or_empty() {
        let mut headers = HeaderMap::new();
        assert_eq!(client_ip_from(&headers, &connect_addr(), Some("cf-connecting-ip")), "127.0.0.1");
        headers.insert("cf-connecting-ip", "".parse().unwrap());
        assert_eq!(client_ip_from(&headers, &connect_addr(), Some("cf-connecting-ip")), "127.0.0.1");
    }
}
