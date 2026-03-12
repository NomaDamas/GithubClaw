use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HostedProxyState {
    #[serde(default)]
    mappings: HashMap<u64, InstallationMapping>,
    #[serde(default)]
    claim_proofs: HashMap<String, StoredClaimProof>,
    #[serde(default)]
    used_claim_proofs: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallationMapping {
    tunnel_url: String,
    current_update_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredClaimProof {
    installation_id: u64,
    issued_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct HostedProxyStore {
    path: PathBuf,
    state: HostedProxyState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSuccess {
    pub status: &'static str,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
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

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationMappingSnapshot {
    pub tunnel_url: String,
    pub current_update_secret: String,
}

impl HostedProxyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = load_state(&path).unwrap_or_default();
        Self { path, state }
    }

    pub fn register_claim(
        &mut self,
        installation_id: u64,
        tunnel_url: String,
        claim_proof: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        let proof_hash = hash_token(claim_proof);
        let stored_proof = match self.state.claim_proofs.get(&proof_hash) {
            Some(stored) => stored,
            None if self.state.used_claim_proofs.contains(&proof_hash) => {
                return Err(RegisterError::ClaimProofAlreadyUsed)
            }
            None => return Err(RegisterError::InvalidClaimProof),
        };

        if stored_proof.installation_id != installation_id {
            return Err(RegisterError::OwnershipMismatch);
        }

        if now > stored_proof.issued_at + Duration::minutes(CLAIM_PROOF_TTL_MINUTES) {
            return Err(RegisterError::ExpiredClaimProof);
        }

        if self.state.mappings.contains_key(&installation_id) {
            return Err(RegisterError::AlreadyClaimed);
        }

        let mut next = self.state.clone();
        next.claim_proofs.remove(&proof_hash);
        next.used_claim_proofs.insert(proof_hash);
        next.mappings.insert(
            installation_id,
            InstallationMapping {
                tunnel_url: tunnel_url.clone(),
                current_update_secret: generate_update_secret(),
            },
        );

        self.persist(next)
            .map_err(RegisterError::PersistenceFailure)?;
        let mapping = self.state.mappings.get(&installation_id).unwrap();
        Ok(RegisterSuccess {
            status: "claimed",
            installation_id,
            tunnel_url,
            update_secret: mapping.current_update_secret.clone(),
            rotated: false,
        })
    }

    pub fn register_update(
        &mut self,
        installation_id: u64,
        tunnel_url: String,
        update_secret: &str,
    ) -> Result<RegisterSuccess, RegisterError> {
        let current_installation = self.state.mappings.iter().find_map(|(id, mapping)| {
            (mapping.current_update_secret == update_secret).then_some(*id)
        });

        match current_installation {
            Some(id) if id != installation_id => return Err(RegisterError::OwnershipMismatch),
            Some(_) => {}
            None => return Err(RegisterError::InvalidUpdateSecret),
        }

        let mut next = self.state.clone();
        let mapping = next
            .mappings
            .get_mut(&installation_id)
            .ok_or(RegisterError::InvalidUpdateSecret)?;
        mapping.tunnel_url = tunnel_url.clone();
        mapping.current_update_secret = generate_update_secret();

        self.persist(next)
            .map_err(RegisterError::PersistenceFailure)?;
        let mapping = self.state.mappings.get(&installation_id).unwrap();
        Ok(RegisterSuccess {
            status: "updated",
            installation_id,
            tunnel_url,
            update_secret: mapping.current_update_secret.clone(),
            rotated: true,
        })
    }

    fn persist(&mut self, next: HostedProxyState) -> Result<(), String> {
        persist_state(&self.path, &next)?;
        self.state = next;
        Ok(())
    }

    #[cfg(test)]
    pub fn mint_claim_proof_for_tests(
        &mut self,
        installation_id: u64,
        issued_at: DateTime<Utc>,
    ) -> String {
        let raw = format!("cp_{}", Uuid::new_v4().simple());
        let mut next = self.state.clone();
        next.claim_proofs.insert(
            hash_token(&raw),
            StoredClaimProof {
                installation_id,
                issued_at,
            },
        );
        self.persist(next).expect("persist test claim proof");
        raw
    }

    #[cfg(test)]
    pub fn mapping_for_tests(&self, installation_id: u64) -> Option<InstallationMappingSnapshot> {
        self.state
            .mappings
            .get(&installation_id)
            .map(|mapping| InstallationMappingSnapshot {
                tunnel_url: mapping.tunnel_url.clone(),
                current_update_secret: mapping.current_update_secret.clone(),
            })
    }
}

fn load_state(path: &Path) -> Result<HostedProxyState, String> {
    if !path.exists() {
        return Ok(HostedProxyState::default());
    }

    let content = fs::read_to_string(path).map_err(|e| {
        format!(
            "Failed to read hosted proxy state {}: {}",
            path.display(),
            e
        )
    })?;
    serde_json::from_str(&content).map_err(|e| {
        format!(
            "Failed to parse hosted proxy state {}: {}",
            path.display(),
            e
        )
    })
}

fn persist_state(path: &Path, state: &HostedProxyState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create hosted proxy state dir {}: {}",
                parent.display(),
                e
            )
        })?;
    }

    let data = serde_json::to_string_pretty(state).map_err(|e| {
        format!(
            "Failed to encode hosted proxy state {}: {}",
            path.display(),
            e
        )
    })?;
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, data).map_err(|e| {
        format!(
            "Failed to write hosted proxy temp state {}: {}",
            tmp_path.display(),
            e
        )
    })?;
    fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "Failed to move hosted proxy state into place {}: {}",
            path.display(),
            e
        )
    })?;
    Ok(())
}

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_update_secret() -> String {
    format!("us_{}", Uuid::new_v4().simple())
}
