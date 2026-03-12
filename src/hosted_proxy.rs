use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimProofRecord {
    pub installation_id: u64,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallationRecord {
    pub tunnel_url: String,
    pub update_secret: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostedProxyState {
    #[serde(default)]
    pub proofs: HashMap<String, ClaimProofRecord>,
    #[serde(default)]
    pub installations: HashMap<u64, InstallationRecord>,
}

#[derive(Debug)]
pub struct HostedProxyStore {
    path: PathBuf,
    state: HostedProxyState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MintedClaimProof {
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSuccess {
    pub status: &'static str,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
    pub status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    InvalidClaimProof,
    ExpiredClaimProof,
    ClaimProofAlreadyUsed,
    InvalidUpdateSecret,
    OwnershipMismatch,
    AlreadyClaimed,
    PersistenceFailure(String),
}

impl RegisterError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidClaimProof => "invalid_claim_proof",
            Self::ExpiredClaimProof => "expired_claim_proof",
            Self::ClaimProofAlreadyUsed => "claim_proof_already_used",
            Self::InvalidUpdateSecret => "invalid_update_secret",
            Self::OwnershipMismatch => "ownership_mismatch",
            Self::AlreadyClaimed => "already_claimed",
            Self::PersistenceFailure(_) => "persistence_failure",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidClaimProof => "Claim proof is invalid.".to_string(),
            Self::ExpiredClaimProof => "Claim proof has expired.".to_string(),
            Self::ClaimProofAlreadyUsed => "Claim proof has already been used.".to_string(),
            Self::InvalidUpdateSecret => "Update secret is invalid.".to_string(),
            Self::OwnershipMismatch => {
                "Credential belongs to a different installation.".to_string()
            }
            Self::AlreadyClaimed => "Installation has already been claimed.".to_string(),
            Self::PersistenceFailure(message) => message.clone(),
        }
    }

    pub fn status_code(&self) -> u16 {
        match self {
            Self::AlreadyClaimed => 409,
            Self::InvalidClaimProof
            | Self::ExpiredClaimProof
            | Self::ClaimProofAlreadyUsed
            | Self::InvalidUpdateSecret
            | Self::OwnershipMismatch => 403,
            Self::PersistenceFailure(_) => 500,
        }
    }
}

impl HostedProxyStore {
    pub fn load_or_default(home: &Path) -> Result<Self, String> {
        let path = home.join("hosted_proxy_state.json");
        if !path.exists() {
            return Ok(Self {
                path,
                state: HostedProxyState::default(),
            });
        }

        let content = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "Failed to read hosted proxy state {}: {}",
                path.display(),
                e
            )
        })?;
        let state = serde_json::from_str::<HostedProxyState>(&content).map_err(|e| {
            format!(
                "Failed to parse hosted proxy state {}: {}",
                path.display(),
                e
            )
        })?;

        Ok(Self { path, state })
    }

    pub fn mint_claim_proof(
        &mut self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<MintedClaimProof, String> {
        self.prune_stale_used_proofs(now);

        let claim_proof = format!("cp_{}", uuid::Uuid::new_v4().simple());
        let expires_at = now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES);
        self.state.proofs.insert(
            claim_proof.clone(),
            ClaimProofRecord {
                installation_id,
                expires_at,
                used_at: None,
            },
        );
        self.persist()?;

        Ok(MintedClaimProof {
            claim_proof,
            expires_at,
        })
    }

    pub fn register_with_claim_proof(
        &mut self,
        installation_id: u64,
        tunnel_url: String,
        claim_proof: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        self.prune_stale_used_proofs(now);

        if self.state.installations.contains_key(&installation_id) {
            return Err(RegisterError::AlreadyClaimed);
        }

        let proof = self
            .state
            .proofs
            .get(claim_proof)
            .cloned()
            .ok_or(RegisterError::InvalidClaimProof)?;

        if proof.installation_id != installation_id {
            return Err(RegisterError::OwnershipMismatch);
        }
        if proof.used_at.is_some() {
            return Err(RegisterError::ClaimProofAlreadyUsed);
        }
        if now > proof.expires_at {
            return Err(RegisterError::ExpiredClaimProof);
        }

        let update_secret = new_update_secret();
        self.state.installations.insert(
            installation_id,
            InstallationRecord {
                tunnel_url: tunnel_url.clone(),
                update_secret: update_secret.clone(),
                created_at: now,
                updated_at: now,
            },
        );
        if let Some(stored_proof) = self.state.proofs.get_mut(claim_proof) {
            stored_proof.used_at = Some(now);
        }
        self.persist().map_err(RegisterError::PersistenceFailure)?;

        Ok(RegisterSuccess {
            status: "claimed",
            installation_id,
            tunnel_url,
            update_secret,
            rotated: false,
            status_code: 201,
        })
    }

    pub fn register_with_update_secret(
        &mut self,
        installation_id: u64,
        tunnel_url: String,
        update_secret: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        self.prune_stale_used_proofs(now);

        let existing = self
            .state
            .installations
            .get(&installation_id)
            .cloned()
            .ok_or(RegisterError::InvalidUpdateSecret)?;

        if existing.update_secret != update_secret {
            if self
                .state
                .installations
                .iter()
                .any(|(stored_installation_id, record)| {
                    *stored_installation_id != installation_id
                        && record.update_secret == update_secret
                })
            {
                return Err(RegisterError::OwnershipMismatch);
            }
            return Err(RegisterError::InvalidUpdateSecret);
        }

        let replacement_secret = new_update_secret();
        self.state.installations.insert(
            installation_id,
            InstallationRecord {
                tunnel_url: tunnel_url.clone(),
                update_secret: replacement_secret.clone(),
                created_at: existing.created_at,
                updated_at: now,
            },
        );
        self.persist().map_err(RegisterError::PersistenceFailure)?;

        Ok(RegisterSuccess {
            status: "updated",
            installation_id,
            tunnel_url,
            update_secret: replacement_secret,
            rotated: true,
            status_code: 200,
        })
    }

    pub fn state(&self) -> &HostedProxyState {
        &self.state
    }

    fn prune_stale_used_proofs(&mut self, now: DateTime<Utc>) {
        self.state
            .proofs
            .retain(|_, proof| proof.used_at.is_none() || proof.expires_at >= now);
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create hosted proxy state dir {}: {}",
                    parent.display(),
                    e
                )
            })?;
        }

        let serialized = serde_json::to_string_pretty(&self.state)
            .map_err(|e| format!("Failed to serialize hosted proxy state: {}", e))?;
        std::fs::write(&self.path, serialized).map_err(|e| {
            format!(
                "Failed to write hosted proxy state {}: {}",
                self.path.display(),
                e
            )
        })
    }
}

fn new_update_secret() -> String {
    format!("us_{}", uuid::Uuid::new_v4().simple())
}

pub fn normalize_tunnel_url(raw: &str) -> Result<String, TunnelUrlError> {
    let url = Url::parse(raw).map_err(|_| TunnelUrlError::InvalidTunnelUrl)?;
    if url.scheme() != "https" {
        return Err(TunnelUrlError::InvalidTunnelUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TunnelUrlError::InvalidTunnelUrl);
    }
    if url.host_str().is_none() {
        return Err(TunnelUrlError::InvalidTunnelUrl);
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(TunnelUrlError::InvalidTunnelUrl);
    }

    let host = url.host_str().unwrap();
    if host.eq_ignore_ascii_case("localhost") {
        return Err(TunnelUrlError::UnsafeTunnelUrl);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_unsafe_ip(ip) {
            return Err(TunnelUrlError::UnsafeTunnelUrl);
        }
    }

    let mut normalized = format!("https://{}", host.to_ascii_lowercase());
    if let Some(port) = url.port() {
        normalized.push(':');
        normalized.push_str(&port.to_string());
    }
    Ok(normalized)
}

fn is_unsafe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelUrlError {
    InvalidTunnelUrl,
    UnsafeTunnelUrl,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn claim_proof_persists_and_reload_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();

        let proof = {
            let mut store = HostedProxyStore::load_or_default(tmp.path()).unwrap();
            store.mint_claim_proof(42, now).unwrap().claim_proof
        };

        let reloaded = HostedProxyStore::load_or_default(tmp.path()).unwrap();
        let stored = reloaded.state().proofs.get(&proof).unwrap();
        assert_eq!(stored.installation_id, 42);
        assert!(stored.used_at.is_none());
    }

    #[test]
    fn normalize_tunnel_url_rejects_paths_and_private_hosts() {
        assert_eq!(
            normalize_tunnel_url("https://example.com/path"),
            Err(TunnelUrlError::InvalidTunnelUrl)
        );
        assert_eq!(
            normalize_tunnel_url("https://127.0.0.1"),
            Err(TunnelUrlError::UnsafeTunnelUrl)
        );
        assert_eq!(
            normalize_tunnel_url("http://example.com"),
            Err(TunnelUrlError::InvalidTunnelUrl)
        );
    }

    #[test]
    fn update_secret_rotation_invalidates_previous_secret() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let mut store = HostedProxyStore::load_or_default(tmp.path()).unwrap();
        let minted = store.mint_claim_proof(7, now).unwrap();
        let claimed = store
            .register_with_claim_proof(
                7,
                "https://one.example".to_string(),
                &minted.claim_proof,
                now,
            )
            .unwrap();
        let updated = store
            .register_with_update_secret(
                7,
                "https://two.example".to_string(),
                &claimed.update_secret,
                now + Duration::seconds(1),
            )
            .unwrap();

        assert_eq!(updated.status, "updated");
        assert_eq!(
            store.register_with_update_secret(
                7,
                "https://three.example".to_string(),
                &claimed.update_secret,
                now + Duration::seconds(2),
            ),
            Err(RegisterError::InvalidUpdateSecret)
        );
    }

    #[test]
    fn claim_proof_enforces_expiry_and_installation_binding() {
        let tmp = TempDir::new().unwrap();
        let now = Utc::now();
        let mut store = HostedProxyStore::load_or_default(tmp.path()).unwrap();
        let minted = store.mint_claim_proof(99, now).unwrap();

        assert_eq!(
            store.register_with_claim_proof(
                100,
                "https://bind.example".to_string(),
                &minted.claim_proof,
                now,
            ),
            Err(RegisterError::OwnershipMismatch)
        );

        assert_eq!(
            store.register_with_claim_proof(
                99,
                "https://expired.example".to_string(),
                &minted.claim_proof,
                now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES + 1),
            ),
            Err(RegisterError::ExpiredClaimProof)
        );
    }
}
