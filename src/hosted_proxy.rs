//! Hosted proxy registration helpers.
//!
//! The hosted proxy stores one canonical tunnel origin per installation.
//! Validation stays conservative: HTTPS only, origin-only URLs, and public DNS
//! hostnames rather than localhost, internal names, or IP literals.

use reqwest::Url;
use std::fmt;
use std::net::IpAddr;

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

    if url.path() != "/" {
        return Err(TunnelUrlError::Invalid(
            "tunnel_url must be an origin without a path",
        ));
    }

    let host = url
        .host_str()
        .ok_or(TunnelUrlError::Invalid("tunnel_url must include a host"))?;

    if is_unsafe_host(host) {
        return Err(TunnelUrlError::Unsafe(
            "tunnel_url must use a public DNS hostname",
        ));
    }

    let mut canonical = format!(
        "https://{}",
        host.trim_end_matches('.').to_ascii_lowercase()
    );
    if let Some(port) = url.port() {
        if port != 443 {
            canonical.push(':');
            canonical.push_str(&port.to_string());
        }
    }

    Ok(canonical)
}

fn is_unsafe_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();

    if host.is_empty() || !host.contains('.') {
        return true;
    }

    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return true;
    }

    if host.parse::<IpAddr>().is_ok() {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_tunnel_url, TunnelUrlError};

    #[test]
    fn canonicalizes_host_case_default_port_and_root_slash() {
        let cases = [
            (
                "https://AbC123.TryCloudflare.com:443",
                "https://abc123.trycloudflare.com",
            ),
            ("HTTPS://EXAMPLE.com/", "https://example.com"),
            ("https://Example.com.:8443/", "https://example.com:8443"),
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
                "https://example.com/webhook",
                TunnelUrlError::Invalid("tunnel_url must be an origin without a path"),
            ),
            (
                "https://example.com//",
                TunnelUrlError::Invalid("tunnel_url must be an origin without a path"),
            ),
            (
                "https://example.com?token=1",
                TunnelUrlError::Invalid("tunnel_url must not include a query string"),
            ),
            (
                "https://example.com/#frag",
                TunnelUrlError::Invalid("tunnel_url must not include a fragment"),
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
    fn rejects_localhost_internal_and_single_label_hosts() {
        for input in [
            "https://localhost",
            "https://api.localhost",
            "https://service.local",
            "https://service.internal",
            "https://devbox",
        ] {
            let error = canonicalize_tunnel_url(input).unwrap_err();
            assert_eq!(
                error,
                TunnelUrlError::Unsafe("tunnel_url must use a public DNS hostname")
            );
        }
    }

    #[test]
    fn rejects_ip_literals() {
        for input in [
            "https://127.0.0.1",
            "https://10.0.0.8",
            "https://192.168.1.10:8443",
            "https://[::1]",
            "https://[2001:db8::1]",
        ] {
            let error = canonicalize_tunnel_url(input).unwrap_err();
            assert_eq!(
                error,
                TunnelUrlError::Unsafe("tunnel_url must use a public DNS hostname")
            );
        }
    }
}
