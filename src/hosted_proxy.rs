//! Hosted proxy tunnel URL validation and canonicalization.
//!
//! The registration contract accepts only conservative HTTPS origins and
//! rejects paths, queries, fragments, embedded credentials, and local/private
//! network targets. The final handler can reuse this module to normalize the
//! stored tunnel URL deterministically.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use reqwest::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelUrlError {
    InvalidTunnelUrl,
    UnsafeTunnelUrl,
}

impl fmt::Display for TunnelUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTunnelUrl => write!(f, "invalid_tunnel_url"),
            Self::UnsafeTunnelUrl => write!(f, "unsafe_tunnel_url"),
        }
    }
}

impl std::error::Error for TunnelUrlError {}

/// Accept only conservative HTTPS origins and normalize them for storage.
pub fn canonicalize_tunnel_url(input: &str) -> Result<String, TunnelUrlError> {
    let url = Url::parse(input).map_err(|_| TunnelUrlError::InvalidTunnelUrl)?;
    if url.scheme() != "https" {
        return Err(TunnelUrlError::InvalidTunnelUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TunnelUrlError::UnsafeTunnelUrl);
    }
    if raw_input_has_path_query_or_fragment(input) {
        return Err(TunnelUrlError::UnsafeTunnelUrl);
    }

    let host = url.host_str().ok_or(TunnelUrlError::InvalidTunnelUrl)?;
    if is_local_hostname(host) {
        return Err(TunnelUrlError::UnsafeTunnelUrl);
    }
    if let Some(ip) = parse_ip_literal(host) {
        if !is_public_ip(ip) {
            return Err(TunnelUrlError::UnsafeTunnelUrl);
        }
    }

    let canonical_host = host.to_ascii_lowercase();
    match url.port() {
        Some(443) | None => Ok(format!("https://{canonical_host}")),
        Some(port) => Ok(format!("https://{canonical_host}:{port}")),
    }
}

fn raw_input_has_path_query_or_fragment(input: &str) -> bool {
    let Some(authority_start) = input.find("://").map(|idx| idx + 3) else {
        return false;
    };

    input[authority_start..]
        .bytes()
        .any(|byte| matches!(byte, b'/' | b'?' | b'#'))
}

fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    let unwrapped = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);

    unwrapped.parse::<IpAddr>().ok()
}

fn is_local_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
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
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }

    let segments = ip.segments();

    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_tunnel_url, TunnelUrlError};

    #[test]
    fn canonicalizes_equivalent_https_origins() {
        let cases = [
            ("https://Example.COM:443", "https://example.com"),
            ("HTTPS://EXAMPLE.com", "https://example.com"),
            ("hTtPs://Example.com:443", "https://example.com"),
            ("https://Example.COM:8443", "https://example.com:8443"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Ok(expected.to_string()),
                "{input} should canonicalize deterministically"
            );
        }
    }

    #[test]
    fn rejects_non_https_urls() {
        for input in ["http://example.com", "ws://example.com", "example.com"] {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(TunnelUrlError::InvalidTunnelUrl),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_paths_queries_fragments_and_ports_without_origin_form() {
        for input in [
            "https://example.com/",
            "https://example.com/path",
            "https://example.com//",
            "https://example.com?x=1",
            "https://example.com?",
            "https://example.com#frag",
            "https://example.com:443/path",
        ] {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(TunnelUrlError::UnsafeTunnelUrl),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_embedded_credentials() {
        for input in ["https://user@example.com", "https://user:pass@example.com"] {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(TunnelUrlError::UnsafeTunnelUrl),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_local_hostnames() {
        for input in [
            "https://localhost",
            "https://LOCALHOST",
            "https://foo.localhost",
            "https://localhost.",
        ] {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(TunnelUrlError::UnsafeTunnelUrl),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_private_and_special_ipv4_targets() {
        for input in [
            "https://0.0.0.0",
            "https://10.0.0.8",
            "https://100.64.0.1",
            "https://127.0.0.1",
            "https://169.254.10.20",
            "https://172.16.0.10",
            "https://192.168.1.20",
            "https://192.0.2.10",
            "https://198.18.0.1",
            "https://224.0.0.1",
            "https://240.0.0.1",
            "https://255.255.255.255",
        ] {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(TunnelUrlError::UnsafeTunnelUrl),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_private_and_special_ipv6_targets() {
        for input in [
            "https://[::]",
            "https://[::1]",
            "https://[fc00::1]",
            "https://[fe80::1]",
            "https://[ff02::1]",
            "https://[2001:db8::1]",
            "https://[::ffff:127.0.0.1]",
        ] {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(TunnelUrlError::UnsafeTunnelUrl),
                "{input} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_public_ip_targets() {
        let cases = [
            ("https://1.1.1.1", "https://1.1.1.1"),
            (
                "https://[2606:4700:4700::1111]",
                "https://[2606:4700:4700::1111]",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Ok(expected.to_string()),
                "{input} should be accepted"
            );
        }
    }
}
