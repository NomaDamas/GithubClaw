use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug)]
pub struct HostedProxyStore {
    path: PathBuf,
    state: HostedProxyState,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostedProxyState {
    #[serde(default)]
    claim_proofs: HashMap<String, ClaimProofRecord>,
    #[serde(default)]
    installations: HashMap<String, InstallationRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimProofRecord {
    installation_id: u64,
    expires_at: DateTime<Utc>,
    used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallationRecord {
    tunnel_url: String,
    update_secret: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SetupReadyResponse {
    pub installation_id: u64,
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct RegistrationResult {
    pub status: RegistrationStatus,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    Claimed,
    Updated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyErrorCode {
    InvalidRequest,
    InvalidTunnelUrl,
    UnsafeTunnelUrl,
    InvalidClaimProof,
    ExpiredClaimProof,
    ClaimProofAlreadyUsed,
    InvalidUpdateSecret,
    OwnershipMismatch,
    AlreadyClaimed,
    PersistenceFailure,
}

impl ProxyErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidTunnelUrl => "invalid_tunnel_url",
            Self::UnsafeTunnelUrl => "unsafe_tunnel_url",
            Self::InvalidClaimProof => "invalid_claim_proof",
            Self::ExpiredClaimProof => "expired_claim_proof",
            Self::ClaimProofAlreadyUsed => "claim_proof_already_used",
            Self::InvalidUpdateSecret => "invalid_update_secret",
            Self::OwnershipMismatch => "ownership_mismatch",
            Self::AlreadyClaimed => "already_claimed",
            Self::PersistenceFailure => "persistence_failure",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProxyError {
    pub code: ProxyErrorCode,
    pub message: String,
}

impl ProxyError {
    fn new(code: ProxyErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl HostedProxyStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let state = load_state(&path);
        Self { path, state }
    }

    pub fn issue_claim_proof(
        &mut self,
        installation_id: u64,
    ) -> Result<SetupReadyResponse, ProxyError> {
        self.issue_claim_proof_with_expiry(
            installation_id,
            Utc::now() + Duration::minutes(CLAIM_PROOF_TTL_MINUTES),
        )
    }

    pub fn issue_claim_proof_with_expiry(
        &mut self,
        installation_id: u64,
        expires_at: DateTime<Utc>,
    ) -> Result<SetupReadyResponse, ProxyError> {
        if installation_id == 0 {
            return Err(ProxyError::new(
                ProxyErrorCode::InvalidRequest,
                "installation_id must be positive",
            ));
        }

        let claim_proof = format!("cp_{}", Uuid::new_v4().simple());
        self.state.claim_proofs.insert(
            claim_proof.clone(),
            ClaimProofRecord {
                installation_id,
                expires_at,
                used_at: None,
            },
        );
        self.save()?;

        Ok(SetupReadyResponse {
            installation_id,
            claim_proof,
            expires_at,
        })
    }

    pub fn register_claim(
        &mut self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: &str,
    ) -> Result<RegistrationResult, ProxyError> {
        let canonical_tunnel_url = canonicalize_tunnel_url(tunnel_url)?;
        let now = Utc::now();
        let Some(mut proof_record) = self.state.claim_proofs.get(claim_proof).cloned() else {
            return Err(ProxyError::new(
                ProxyErrorCode::InvalidClaimProof,
                "claim proof is not recognized",
            ));
        };

        if proof_record.installation_id != installation_id {
            return Err(ProxyError::new(
                ProxyErrorCode::OwnershipMismatch,
                "claim proof belongs to a different installation",
            ));
        }
        if proof_record.used_at.is_some() {
            return Err(ProxyError::new(
                ProxyErrorCode::ClaimProofAlreadyUsed,
                "claim proof has already been used",
            ));
        }
        if now > proof_record.expires_at {
            return Err(ProxyError::new(
                ProxyErrorCode::ExpiredClaimProof,
                "claim proof has expired",
            ));
        }
        if self.installation_key_exists(installation_id) {
            return Err(ProxyError::new(
                ProxyErrorCode::AlreadyClaimed,
                "installation already claimed",
            ));
        }

        let update_secret = new_update_secret();
        self.state.installations.insert(
            installation_id.to_string(),
            InstallationRecord {
                tunnel_url: canonical_tunnel_url.clone(),
                update_secret: update_secret.clone(),
                created_at: now,
                updated_at: now,
            },
        );
        proof_record.used_at = Some(now);
        self.state
            .claim_proofs
            .insert(claim_proof.to_string(), proof_record);
        self.save()?;

        Ok(RegistrationResult {
            status: RegistrationStatus::Claimed,
            installation_id,
            tunnel_url: canonical_tunnel_url,
            update_secret,
        })
    }

    pub fn update_registration(
        &mut self,
        installation_id: u64,
        tunnel_url: &str,
        update_secret: &str,
    ) -> Result<RegistrationResult, ProxyError> {
        let canonical_tunnel_url = canonicalize_tunnel_url(tunnel_url)?;
        let now = Utc::now();
        let Some(existing) = self
            .state
            .installations
            .get_mut(&installation_id.to_string())
        else {
            return Err(ProxyError::new(
                ProxyErrorCode::InvalidUpdateSecret,
                "update secret is invalid",
            ));
        };

        if existing.update_secret != update_secret {
            return Err(ProxyError::new(
                ProxyErrorCode::InvalidUpdateSecret,
                "update secret is invalid",
            ));
        }

        let replacement_secret = new_update_secret();
        existing.tunnel_url = canonical_tunnel_url.clone();
        existing.update_secret = replacement_secret.clone();
        existing.updated_at = now;
        self.save()?;

        Ok(RegistrationResult {
            status: RegistrationStatus::Updated,
            installation_id,
            tunnel_url: canonical_tunnel_url,
            update_secret: replacement_secret,
        })
    }

    fn installation_key_exists(&self, installation_id: u64) -> bool {
        self.state
            .installations
            .contains_key(&installation_id.to_string())
    }

    fn save(&self) -> Result<(), ProxyError> {
        let Some(parent) = self.path.parent() else {
            return Err(ProxyError::new(
                ProxyErrorCode::PersistenceFailure,
                "hosted proxy state path has no parent directory",
            ));
        };
        std::fs::create_dir_all(parent).map_err(|error| {
            ProxyError::new(
                ProxyErrorCode::PersistenceFailure,
                format!("failed to create hosted proxy state directory: {error}"),
            )
        })?;
        let payload = serde_json::to_vec_pretty(&self.state).map_err(|error| {
            ProxyError::new(
                ProxyErrorCode::PersistenceFailure,
                format!("failed to serialize hosted proxy state: {error}"),
            )
        })?;
        std::fs::write(&self.path, payload).map_err(|error| {
            ProxyError::new(
                ProxyErrorCode::PersistenceFailure,
                format!("failed to persist hosted proxy state: {error}"),
            )
        })
    }
}

fn load_state(path: &Path) -> HostedProxyState {
    if !path.exists() {
        return HostedProxyState::default();
    }

    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => HostedProxyState::default(),
    }
}

fn new_update_secret() -> String {
    format!("us_{}", Uuid::new_v4().simple())
}

fn canonicalize_tunnel_url(input: &str) -> Result<String, ProxyError> {
    let parsed = Url::parse(input).map_err(|_| {
        ProxyError::new(
            ProxyErrorCode::InvalidTunnelUrl,
            "tunnel_url must be a valid URL",
        )
    })?;

    if parsed.scheme() != "https" {
        return Err(ProxyError::new(
            ProxyErrorCode::InvalidTunnelUrl,
            "tunnel_url must use https",
        ));
    }
    if parsed.cannot_be_a_base()
        || parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err(ProxyError::new(
            ProxyErrorCode::InvalidTunnelUrl,
            "tunnel_url must be an https origin without path, query, fragment, or userinfo",
        ));
    }

    let Some(host) = parsed.host_str() else {
        return Err(ProxyError::new(
            ProxyErrorCode::InvalidTunnelUrl,
            "tunnel_url must include a host",
        ));
    };

    if host.eq_ignore_ascii_case("localhost") {
        return Err(ProxyError::new(
            ProxyErrorCode::UnsafeTunnelUrl,
            "localhost is not allowed for tunnel_url",
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_or_loopback(ip) {
            return Err(ProxyError::new(
                ProxyErrorCode::UnsafeTunnelUrl,
                "private, loopback, and local tunnel destinations are not allowed",
            ));
        }
    }

    let mut normalized = format!("https://{host}");
    if let Some(port) = parsed.port() {
        normalized.push(':');
        normalized.push_str(&port.to_string());
    }
    Ok(normalized)
}

fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_private()
                || ipv4.is_loopback()
                || ipv4.is_link_local()
                || ipv4.is_unspecified()
                || ipv4 == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_unique_local()
                || ipv6.is_unicast_link_local()
                || ipv6.is_multicast()
                || ipv6 == Ipv6Addr::LOCALHOST
        }
    }
}
