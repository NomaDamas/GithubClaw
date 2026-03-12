use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

pub const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedClaimProof {
    pub installation_id: u64,
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationResult {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateResult {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
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
    SecretRotationFailure,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedHostedProxyState {
    #[serde(default)]
    claim_proofs: HashMap<String, StoredClaimProof>,
    #[serde(default)]
    installations: HashMap<String, StoredInstallation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredClaimProof {
    installation_id: u64,
    expires_at: DateTime<Utc>,
    used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredInstallation {
    tunnel_url: String,
    update_secret_hash: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

pub struct HostedProxyService {
    state_path: PathBuf,
    inner: Mutex<PersistedHostedProxyState>,
}

impl HostedProxyService {
    pub fn new(state_path: &Path) -> Result<Self, String> {
        let inner = if state_path.exists() {
            let raw = std::fs::read_to_string(state_path).map_err(|e| {
                format!(
                    "Failed to read hosted proxy state {}: {}",
                    state_path.display(),
                    e
                )
            })?;
            serde_json::from_str(&raw).map_err(|e| {
                format!(
                    "Failed to parse hosted proxy state {}: {}",
                    state_path.display(),
                    e
                )
            })?
        } else {
            PersistedHostedProxyState::default()
        };

        Ok(Self {
            state_path: state_path.to_path_buf(),
            inner: Mutex::new(inner),
        })
    }

    pub fn mint_claim_proof_at(
        &self,
        installation_id: u64,
        issued_at: DateTime<Utc>,
    ) -> Result<IssuedClaimProof, RegisterError> {
        if installation_id == 0 {
            return Err(RegisterError::InvalidRequest);
        }

        let claim_proof = format!("cp_{}", Uuid::new_v4().simple());
        let expires_at = issued_at + Duration::minutes(CLAIM_PROOF_TTL_MINUTES);

        let mut state = self.inner.lock().unwrap();
        state.claim_proofs.insert(
            hash_secret(&claim_proof),
            StoredClaimProof {
                installation_id,
                expires_at,
                used_at: None,
            },
        );
        self.save_locked(&state)?;

        Ok(IssuedClaimProof {
            installation_id,
            claim_proof,
            expires_at,
        })
    }

    pub fn register_with_claim_proof_at(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: &str,
        now: DateTime<Utc>,
    ) -> Result<RegistrationResult, RegisterError> {
        if installation_id == 0 || claim_proof.is_empty() {
            return Err(RegisterError::InvalidRequest);
        }

        let normalized_tunnel_url = normalize_tunnel_url(tunnel_url)?;
        let proof_hash = hash_secret(claim_proof);
        let update_secret = format!("us_{}", Uuid::new_v4().simple());
        let update_secret_hash = hash_secret(&update_secret);

        let mut state = self.inner.lock().unwrap();
        let proof = state
            .claim_proofs
            .get(&proof_hash)
            .ok_or(RegisterError::InvalidClaimProof)?;

        if proof.installation_id != installation_id {
            return Err(RegisterError::OwnershipMismatch);
        }
        if proof.used_at.is_some() {
            return Err(RegisterError::ClaimProofAlreadyUsed);
        }
        if now >= proof.expires_at {
            return Err(RegisterError::ExpiredClaimProof);
        }
        if state
            .installations
            .contains_key(&installation_id.to_string())
        {
            return Err(RegisterError::AlreadyClaimed);
        }

        let created_at = now;
        state.installations.insert(
            installation_id.to_string(),
            StoredInstallation {
                tunnel_url: normalized_tunnel_url.clone(),
                update_secret_hash,
                created_at,
                updated_at: created_at,
            },
        );
        if let Some(proof) = state.claim_proofs.get_mut(&proof_hash) {
            proof.used_at = Some(now);
        }
        self.save_locked(&state)?;

        Ok(RegistrationResult {
            installation_id,
            tunnel_url: normalized_tunnel_url,
            update_secret,
            created_at,
        })
    }

    pub fn update_registration_at(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        update_secret: &str,
        now: DateTime<Utc>,
    ) -> Result<UpdateResult, RegisterError> {
        if installation_id == 0 || update_secret.is_empty() {
            return Err(RegisterError::InvalidRequest);
        }

        let normalized_tunnel_url = normalize_tunnel_url(tunnel_url)?;
        let supplied_hash = hash_secret(update_secret);

        let mut state = self.inner.lock().unwrap();
        let matched_installation_id = state
            .installations
            .iter()
            .find_map(|(installation_key, registration)| {
                (registration.update_secret_hash == supplied_hash)
                    .then(|| installation_key.parse::<u64>().ok())
                    .flatten()
            })
            .ok_or(RegisterError::InvalidUpdateSecret)?;

        if matched_installation_id != installation_id {
            return Err(RegisterError::OwnershipMismatch);
        }

        let replacement_secret = format!("us_{}", Uuid::new_v4().simple());
        let replacement_hash = hash_secret(&replacement_secret);
        let registration = state
            .installations
            .get_mut(&installation_id.to_string())
            .ok_or(RegisterError::InvalidUpdateSecret)?;

        registration.tunnel_url = normalized_tunnel_url.clone();
        registration.update_secret_hash = replacement_hash;
        registration.updated_at = now;
        self.save_locked(&state)
            .map_err(|_| RegisterError::SecretRotationFailure)?;

        Ok(UpdateResult {
            installation_id,
            tunnel_url: normalized_tunnel_url,
            update_secret: replacement_secret,
            updated_at: now,
        })
    }

    fn save_locked(&self, state: &PersistedHostedProxyState) -> Result<(), RegisterError> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| RegisterError::PersistenceFailure)?;
        }

        let serialized =
            serde_json::to_vec_pretty(state).map_err(|_| RegisterError::PersistenceFailure)?;
        let temp_path = self.state_path.with_extension("tmp");
        std::fs::write(&temp_path, serialized).map_err(|_| RegisterError::PersistenceFailure)?;
        std::fs::rename(&temp_path, &self.state_path)
            .map_err(|_| RegisterError::PersistenceFailure)?;
        Ok(())
    }
}

pub fn error_code(error: &RegisterError) -> &'static str {
    match error {
        RegisterError::InvalidRequest => "invalid_request",
        RegisterError::InvalidTunnelUrl => "invalid_tunnel_url",
        RegisterError::UnsafeTunnelUrl => "unsafe_tunnel_url",
        RegisterError::InvalidClaimProof => "invalid_claim_proof",
        RegisterError::ExpiredClaimProof => "expired_claim_proof",
        RegisterError::ClaimProofAlreadyUsed => "claim_proof_already_used",
        RegisterError::InvalidUpdateSecret => "invalid_update_secret",
        RegisterError::OwnershipMismatch => "ownership_mismatch",
        RegisterError::AlreadyClaimed => "already_claimed",
        RegisterError::PersistenceFailure => "persistence_failure",
        RegisterError::SecretRotationFailure => "secret_rotation_failure",
    }
}

pub fn error_message(error: &RegisterError) -> &'static str {
    match error {
        RegisterError::InvalidRequest => "request shape is invalid",
        RegisterError::InvalidTunnelUrl => "tunnel_url must be an HTTPS origin",
        RegisterError::UnsafeTunnelUrl => "tunnel_url points to an unsafe destination",
        RegisterError::InvalidClaimProof => "claim_proof is not recognized",
        RegisterError::ExpiredClaimProof => "claim_proof has expired",
        RegisterError::ClaimProofAlreadyUsed => "claim_proof was already consumed",
        RegisterError::InvalidUpdateSecret => "update_secret is not valid",
        RegisterError::OwnershipMismatch => {
            "the supplied credential is bound to a different installation"
        }
        RegisterError::AlreadyClaimed => "installation already has a registered tunnel",
        RegisterError::PersistenceFailure => "failed to persist hosted proxy state",
        RegisterError::SecretRotationFailure => "failed to rotate update_secret",
    }
}

fn normalize_tunnel_url(raw: &str) -> Result<String, RegisterError> {
    let url = Url::parse(raw).map_err(|_| RegisterError::InvalidTunnelUrl)?;

    if url.scheme() != "https" {
        return Err(RegisterError::InvalidTunnelUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RegisterError::InvalidTunnelUrl);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(RegisterError::InvalidTunnelUrl);
    }
    if url.path() != "/" {
        return Err(RegisterError::InvalidTunnelUrl);
    }

    let host = url.host_str().ok_or(RegisterError::InvalidTunnelUrl)?;
    if let Ok(ipv4) = host.parse::<Ipv4Addr>() {
        if is_unsafe_ip(IpAddr::V4(ipv4)) {
            return Err(RegisterError::UnsafeTunnelUrl);
        }
    } else if let Ok(ipv6) = host.parse::<Ipv6Addr>() {
        if is_unsafe_ip(IpAddr::V6(ipv6)) {
            return Err(RegisterError::UnsafeTunnelUrl);
        }
    } else if is_unsafe_domain(host) {
        return Err(RegisterError::UnsafeTunnelUrl);
    }

    Ok(url.origin().ascii_serialization())
}

fn is_unsafe_domain(domain: &str) -> bool {
    let lowered = domain.to_ascii_lowercase();
    lowered == "localhost"
        || lowered.ends_with(".localhost")
        || lowered.ends_with(".local")
        || lowered.ends_with(".internal")
}

fn is_unsafe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_unsafe_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_unsafe_ipv6(ipv6),
    }
}

fn is_unsafe_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.octets()[0] == 0
}

fn is_unsafe_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local() || ip.is_unicast_link_local()
}

fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 12, 10, 0, 0).unwrap()
    }

    #[test]
    fn test_mint_claim_proof_creates_bound_short_lived_proof() {
        let tmp = TempDir::new().unwrap();
        let service = HostedProxyService::new(&tmp.path().join("hosted-proxy.json")).unwrap();

        let issued = service.mint_claim_proof_at(42, fixed_time()).unwrap();

        assert_eq!(issued.installation_id, 42);
        assert!(issued.claim_proof.starts_with("cp_"));
        assert_eq!(
            issued.expires_at,
            fixed_time() + Duration::minutes(CLAIM_PROOF_TTL_MINUTES)
        );
    }

    #[test]
    fn test_register_rejects_expired_claim_proof() {
        let tmp = TempDir::new().unwrap();
        let service = HostedProxyService::new(&tmp.path().join("hosted-proxy.json")).unwrap();
        let issued = service.mint_claim_proof_at(42, fixed_time()).unwrap();

        let err = service
            .register_with_claim_proof_at(
                42,
                "https://abc123.trycloudflare.com",
                &issued.claim_proof,
                fixed_time() + Duration::minutes(CLAIM_PROOF_TTL_MINUTES),
            )
            .unwrap_err();

        assert_eq!(err, RegisterError::ExpiredClaimProof);
    }

    #[test]
    fn test_register_consumes_claim_proof_after_first_success() {
        let tmp = TempDir::new().unwrap();
        let service = HostedProxyService::new(&tmp.path().join("hosted-proxy.json")).unwrap();
        let issued = service.mint_claim_proof_at(42, fixed_time()).unwrap();

        let result = service
            .register_with_claim_proof_at(
                42,
                "https://abc123.trycloudflare.com",
                &issued.claim_proof,
                fixed_time() + Duration::minutes(1),
            )
            .unwrap();
        assert_eq!(result.installation_id, 42);
        assert!(result.update_secret.starts_with("us_"));

        let err = service
            .register_with_claim_proof_at(
                42,
                "https://next456.trycloudflare.com",
                &issued.claim_proof,
                fixed_time() + Duration::minutes(2),
            )
            .unwrap_err();
        assert_eq!(err, RegisterError::ClaimProofAlreadyUsed);
    }

    #[test]
    fn test_register_rejects_claim_proof_for_different_installation() {
        let tmp = TempDir::new().unwrap();
        let service = HostedProxyService::new(&tmp.path().join("hosted-proxy.json")).unwrap();
        let issued = service.mint_claim_proof_at(42, fixed_time()).unwrap();

        let err = service
            .register_with_claim_proof_at(
                7,
                "https://abc123.trycloudflare.com",
                &issued.claim_proof,
                fixed_time() + Duration::minutes(1),
            )
            .unwrap_err();

        assert_eq!(err, RegisterError::OwnershipMismatch);
    }
}
