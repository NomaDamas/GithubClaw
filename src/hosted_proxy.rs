use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimProofClaims {
    pub installation_id: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterRequest {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub claim_proof: Option<String>,
    pub update_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationStatus {
    Claimed,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterResponse {
    pub status: RegistrationStatus,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedRegistration {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedProxyError {
    InvalidRequest,
    InvalidTunnelUrl,
    UnsafeTunnelUrl,
    InvalidClaimProof,
    ExpiredClaimProof,
    ClaimProofAlreadyUsed,
    InvalidUpdateSecret,
    OwnershipMismatch,
    AlreadyClaimed,
    Persistence(String),
}

impl fmt::Display for HostedProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => write!(f, "invalid_request"),
            Self::InvalidTunnelUrl => write!(f, "invalid_tunnel_url"),
            Self::UnsafeTunnelUrl => write!(f, "unsafe_tunnel_url"),
            Self::InvalidClaimProof => write!(f, "invalid_claim_proof"),
            Self::ExpiredClaimProof => write!(f, "expired_claim_proof"),
            Self::ClaimProofAlreadyUsed => write!(f, "claim_proof_already_used"),
            Self::InvalidUpdateSecret => write!(f, "invalid_update_secret"),
            Self::OwnershipMismatch => write!(f, "ownership_mismatch"),
            Self::AlreadyClaimed => write!(f, "already_claimed"),
            Self::Persistence(message) => write!(f, "persistence_failure: {message}"),
        }
    }
}

impl std::error::Error for HostedProxyError {}

pub struct HostedProxyState {
    claim_proof_secret: String,
    inner: Mutex<PersistedState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    path: PathBuf,
    installations: HashMap<u64, StoredInstallation>,
    used_claim_proofs: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredInstallation {
    installation_id: u64,
    tunnel_url: String,
    update_secret: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedStateFile {
    #[serde(default)]
    installations: Vec<StoredInstallation>,
    #[serde(default)]
    used_claim_proofs: Vec<String>,
}

impl HostedProxyState {
    pub fn load(
        path: impl AsRef<Path>,
        claim_proof_secret: impl Into<String>,
    ) -> Result<Self, HostedProxyError> {
        Ok(Self {
            claim_proof_secret: claim_proof_secret.into(),
            inner: Mutex::new(PersistedState::load(path.as_ref()).map_err(persistence_error)?),
        })
    }

    pub fn register(&self, request: RegisterRequest) -> Result<RegisterResponse, HostedProxyError> {
        if request.installation_id == 0 {
            return Err(HostedProxyError::InvalidRequest);
        }

        let has_claim_proof = request.claim_proof.is_some();
        let has_update_secret = request.update_secret.is_some();
        if has_claim_proof == has_update_secret {
            return Err(HostedProxyError::InvalidRequest);
        }

        let tunnel_url = canonicalize_tunnel_url(&request.tunnel_url)?;

        if let Some(claim_proof) = request.claim_proof {
            return self.claim_installation(request.installation_id, tunnel_url, &claim_proof);
        }

        self.update_installation(
            request.installation_id,
            tunnel_url,
            request.update_secret.expect("validated update secret"),
        )
    }

    pub fn get_registration(&self, installation_id: u64) -> Option<HostedRegistration> {
        let state = self.inner.lock().expect("hosted proxy state lock poisoned");
        state
            .installations
            .get(&installation_id)
            .map(|registration| HostedRegistration {
                installation_id: registration.installation_id,
                tunnel_url: registration.tunnel_url.clone(),
                created_at: registration.created_at,
                updated_at: registration.updated_at,
            })
    }

    fn claim_installation(
        &self,
        installation_id: u64,
        tunnel_url: String,
        claim_proof: &str,
    ) -> Result<RegisterResponse, HostedProxyError> {
        let claims = verify_claim_proof(claim_proof, &self.claim_proof_secret)?;
        let mut state = self.inner.lock().expect("hosted proxy state lock poisoned");

        if state.used_claim_proofs.contains(&claims.nonce) {
            return Err(HostedProxyError::ClaimProofAlreadyUsed);
        }
        if claims.installation_id != installation_id {
            return Err(HostedProxyError::OwnershipMismatch);
        }
        if state.installations.contains_key(&installation_id) {
            return Err(HostedProxyError::AlreadyClaimed);
        }

        let now = Utc::now();
        let update_secret = new_update_secret();
        state.installations.insert(
            installation_id,
            StoredInstallation {
                installation_id,
                tunnel_url: tunnel_url.clone(),
                update_secret: update_secret.clone(),
                created_at: now,
                updated_at: now,
            },
        );
        state.used_claim_proofs.insert(claims.nonce);
        state.persist().map_err(persistence_error)?;

        Ok(RegisterResponse {
            status: RegistrationStatus::Claimed,
            installation_id,
            tunnel_url,
            update_secret,
            rotated: false,
        })
    }

    fn update_installation(
        &self,
        installation_id: u64,
        tunnel_url: String,
        update_secret: String,
    ) -> Result<RegisterResponse, HostedProxyError> {
        let mut state = self.inner.lock().expect("hosted proxy state lock poisoned");

        match state.find_secret_owner(&update_secret) {
            Some(owner) if owner != installation_id => {
                return Err(HostedProxyError::OwnershipMismatch);
            }
            Some(_) => {}
            None => return Err(HostedProxyError::InvalidUpdateSecret),
        }

        let registration = state
            .installations
            .get_mut(&installation_id)
            .ok_or(HostedProxyError::InvalidUpdateSecret)?;
        if registration.update_secret != update_secret {
            return Err(HostedProxyError::InvalidUpdateSecret);
        }

        let replacement_secret = new_update_secret();
        registration.tunnel_url = tunnel_url.clone();
        registration.update_secret = replacement_secret.clone();
        registration.updated_at = Utc::now();
        state.persist().map_err(persistence_error)?;

        Ok(RegisterResponse {
            status: RegistrationStatus::Updated,
            installation_id,
            tunnel_url,
            update_secret: replacement_secret,
            rotated: true,
        })
    }
}

impl PersistedState {
    fn load(path: &Path) -> std::io::Result<Self> {
        let path = path.to_path_buf();
        if !path.exists() {
            return Ok(Self {
                path,
                installations: HashMap::new(),
                used_claim_proofs: HashSet::new(),
            });
        }

        let contents = std::fs::read_to_string(&path)?;
        let file: PersistedStateFile =
            serde_json::from_str(&contents).unwrap_or(PersistedStateFile {
                installations: Vec::new(),
                used_claim_proofs: Vec::new(),
            });
        let installations = file
            .installations
            .into_iter()
            .map(|installation| (installation.installation_id, installation))
            .collect();
        let used_claim_proofs = file.used_claim_proofs.into_iter().collect();

        Ok(Self {
            path,
            installations,
            used_claim_proofs,
        })
    }

    fn persist(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut installations: Vec<_> = self.installations.values().cloned().collect();
        installations.sort_by_key(|installation| installation.installation_id);
        let mut used_claim_proofs: Vec<_> = self.used_claim_proofs.iter().cloned().collect();
        used_claim_proofs.sort();

        let contents = serde_json::to_string_pretty(&PersistedStateFile {
            installations,
            used_claim_proofs,
        })?;
        std::fs::write(&self.path, format!("{contents}\n"))
    }

    fn find_secret_owner(&self, update_secret: &str) -> Option<u64> {
        self.installations
            .values()
            .find(|installation| installation.update_secret == update_secret)
            .map(|installation| installation.installation_id)
    }
}

pub fn mint_claim_proof(claims: &ClaimProofClaims, secret: &str) -> String {
    let payload = serde_json::to_vec(claims).expect("claim proof claims should serialize");
    let payload_hex = hex::encode(&payload);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("claim proof HMAC key");
    mac.update(&payload);
    let signature_hex = hex::encode(mac.finalize().into_bytes());
    format!("{payload_hex}.{signature_hex}")
}

fn verify_claim_proof(token: &str, secret: &str) -> Result<ClaimProofClaims, HostedProxyError> {
    let (payload_hex, signature_hex) = token
        .split_once('.')
        .ok_or(HostedProxyError::InvalidClaimProof)?;
    let payload = hex::decode(payload_hex).map_err(|_| HostedProxyError::InvalidClaimProof)?;
    let signature = hex::decode(signature_hex).map_err(|_| HostedProxyError::InvalidClaimProof)?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("claim proof HMAC key");
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| HostedProxyError::InvalidClaimProof)?;

    let claims: ClaimProofClaims =
        serde_json::from_slice(&payload).map_err(|_| HostedProxyError::InvalidClaimProof)?;
    if claims.expires_at < Utc::now() {
        return Err(HostedProxyError::ExpiredClaimProof);
    }

    Ok(claims)
}

fn new_update_secret() -> String {
    format!("us_{}", Uuid::new_v4())
}

fn persistence_error(error: std::io::Error) -> HostedProxyError {
    HostedProxyError::Persistence(error.to_string())
}

pub fn canonicalize_tunnel_url(input: &str) -> Result<String, HostedProxyError> {
    let url = reqwest::Url::parse(input).map_err(|_| HostedProxyError::InvalidTunnelUrl)?;
    if url.scheme() != "https" {
        return Err(HostedProxyError::InvalidTunnelUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HostedProxyError::UnsafeTunnelUrl);
    }
    if raw_input_has_path_query_or_fragment(input) {
        return Err(HostedProxyError::UnsafeTunnelUrl);
    }

    let host = url.host_str().ok_or(HostedProxyError::InvalidTunnelUrl)?;
    if is_local_hostname(host) {
        return Err(HostedProxyError::UnsafeTunnelUrl);
    }
    if let Some(ip) = parse_ip_literal(host) {
        if !is_public_ip(ip) {
            return Err(HostedProxyError::UnsafeTunnelUrl);
        }
    }

    let canonical_host = format_host(host.to_ascii_lowercase());
    match url.port() {
        Some(443) | None => Ok(format!("https://{canonical_host}")),
        Some(port) => Ok(format!("https://{canonical_host}:{port}")),
    }
}

fn raw_input_has_path_query_or_fragment(input: &str) -> bool {
    let Some(authority_start) = input.find("://").map(|index| index + 3) else {
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

fn format_host(host: String) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_tunnel_url, HostedProxyError};

    #[test]
    fn canonicalize_tunnel_url_accepts_public_https_origins() {
        let cases = [
            ("https://Example.COM:443", "https://example.com"),
            ("HTTPS://EXAMPLE.com", "https://example.com"),
            ("https://Example.COM:8443", "https://example.com:8443"),
            (
                "https://[2606:4700:4700::1111]",
                "https://[2606:4700:4700::1111]",
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(canonicalize_tunnel_url(input), Ok(expected.to_string()));
        }
    }

    #[test]
    fn canonicalize_tunnel_url_rejects_invalid_or_unsafe_targets() {
        let invalid = ["http://example.com", "example.com"];
        for input in invalid {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(HostedProxyError::InvalidTunnelUrl)
            );
        }

        let unsafe_targets = [
            "https://example.com/path",
            "https://user@example.com",
            "https://localhost",
            "https://127.0.0.1",
            "https://[::1]",
        ];
        for input in unsafe_targets {
            assert_eq!(
                canonicalize_tunnel_url(input),
                Err(HostedProxyError::UnsafeTunnelUrl)
            );
        }
    }
}
