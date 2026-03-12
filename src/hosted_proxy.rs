//! Hosted proxy registration helpers.
//!
//! The hosted proxy stores one canonical tunnel origin per installation.
//! Validation stays conservative: HTTPS only, origin-only URLs, and no
//! localhost, loopback, or private-network targets.

use reqwest::Url;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelUrlError {
    Invalid(&'static str),
    Unsafe(&'static str),
}

impl TunnelUrlError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_tunnel_url",
            Self::Unsafe(_) => "unsafe_tunnel_url",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::Invalid(message) | Self::Unsafe(message) => message,
        }
    }
}

impl fmt::Display for TunnelUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for TunnelUrlError {}

/// Validate and canonicalize a hosted proxy tunnel URL into the stored form.
///
/// The canonical form is `https://host[:port]` with a lowercase host and the
/// default HTTPS port removed.
pub fn canonicalize_tunnel_url(input: &str) -> Result<String, TunnelUrlError> {
    ensure_origin_form(input)?;

    let url = Url::parse(input)
        .map_err(|_| TunnelUrlError::Invalid("tunnel_url must be a valid absolute URL"))?;

    if url.scheme() != "https" {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must use the https scheme",
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must not include embedded credentials",
        ));
    }

    if url.query().is_some() {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must not include a query string",
        ));
    }

    if url.fragment().is_some() {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must not include a fragment",
        ));
    }

    let host = url
        .host_str()
        .ok_or(TunnelUrlError::Invalid("tunnel_url must include a host"))?;

    if is_local_hostname(host) {
        return Err(TunnelUrlError::Unsafe(
            "tunnel_url must not target localhost",
        ));
    }
    if let Some(ip) = host_to_ip(host) {
        if !is_public_ip(ip) {
            return Err(TunnelUrlError::Unsafe(
                "tunnel_url must use a public host or IP",
            ));
        }
    }

    let mut canonical = format!("https://{}", host.to_ascii_lowercase());
    if let Some(port) = url.port() {
        if port != 443 {
            canonical.push(':');
            canonical.push_str(&port.to_string());
        }
    }

    Ok(canonical)
}

fn ensure_origin_form(input: &str) -> Result<(), TunnelUrlError> {
    let (scheme, rest) = input.split_once("://").ok_or(TunnelUrlError::Invalid(
        "tunnel_url must be a valid absolute URL",
    ))?;

    if !scheme.eq_ignore_ascii_case("https") {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must use the https scheme",
        ));
    }
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must be exactly https://host[:port]",
        ));
    }

    Ok(())
}

fn is_local_hostname(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

fn host_to_ip(host: &str) -> Option<IpAddr> {
    host.parse::<IpAddr>().ok().or_else(|| {
        host.strip_prefix('[')?
            .strip_suffix(']')?
            .parse::<IpAddr>()
            .ok()
    })
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();

    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || (octets[0] & 0xf0) == 240)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();

    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_tunnel_url, TunnelUrlError};

    #[test]
    fn canonicalizes_host_case_and_default_port() {
        let cases = [
            (
                "https://AbC123.TryCloudflare.com:443",
                "https://abc123.trycloudflare.com",
            ),
            ("HTTPS://EXAMPLE.com", "https://example.com"),
            ("https://Example.com:8443", "https://example.com:8443"),
        ];

        for (input, expected) in cases {
            assert_eq!(canonicalize_tunnel_url(input), Ok(expected.to_string()));
        }
    }

    #[test]
    fn preserves_non_default_port() {
        let canonical = canonicalize_tunnel_url("https://Example.com:8443").unwrap();
        assert_eq!(canonical, "https://example.com:8443");
    }

    #[test]
    fn rejects_non_https_urls() {
        let error = canonicalize_tunnel_url("http://example.com").unwrap_err();
        assert_eq!(
            error,
            TunnelUrlError::Invalid("tunnel_url must use the https scheme")
        );
        assert_eq!(error.code(), "invalid_tunnel_url");
    }

    #[test]
    fn rejects_paths_queries_fragments_and_credentials() {
        let cases = [
            (
                "https://example.com/",
                TunnelUrlError::Invalid("tunnel_url must be exactly https://host[:port]"),
            ),
            (
                "https://example.com/webhook",
                TunnelUrlError::Invalid("tunnel_url must be exactly https://host[:port]"),
            ),
            (
                "https://example.com//",
                TunnelUrlError::Invalid("tunnel_url must be exactly https://host[:port]"),
            ),
            (
                "https://example.com?token=1",
                TunnelUrlError::Invalid("tunnel_url must be exactly https://host[:port]"),
            ),
            (
                "https://example.com/#frag",
                TunnelUrlError::Invalid("tunnel_url must be exactly https://host[:port]"),
            ),
            (
                "https://user@example.com",
                TunnelUrlError::Invalid("tunnel_url must not include embedded credentials"),
            ),
            (
                "https://user:pass@example.com",
                TunnelUrlError::Invalid("tunnel_url must not include embedded credentials"),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(canonicalize_tunnel_url(input), Err(expected));
        }
    }

    #[test]
    fn rejects_localhost_targets() {
        for input in ["https://localhost", "https://api.localhost"] {
            let error = canonicalize_tunnel_url(input).unwrap_err();
            assert_eq!(
                error,
                TunnelUrlError::Unsafe("tunnel_url must not target localhost")
            );
        }
    }

    #[test]
    fn rejects_private_or_local_ip_literals() {
        for input in [
            "https://0.0.0.0",
            "https://127.0.0.1",
            "https://10.0.0.8",
            "https://192.168.1.10:8443",
            "https://169.254.10.20",
            "https://172.16.0.10",
            "https://100.64.0.1",
            "https://[::1]",
            "https://[fc00::1]",
            "https://[fe80::1]",
            "https://[2001:db8::1]",
        ] {
            let error = canonicalize_tunnel_url(input).unwrap_err();
            assert_eq!(
                error,
                TunnelUrlError::Unsafe("tunnel_url must use a public host or IP"),
            );
        }
    }

    #[test]
    fn accepts_public_ip_literals() {
        let cases = [
            ("https://1.1.1.1", "https://1.1.1.1"),
            (
                "https://[2606:4700:4700::1111]",
                "https://[2606:4700:4700::1111]",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(canonicalize_tunnel_url(input), Ok(expected.to_string()));
        }
    }
}
