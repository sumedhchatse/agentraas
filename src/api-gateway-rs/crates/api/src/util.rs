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
