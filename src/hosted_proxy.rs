use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

const CLAIM_PROOF_TTL_MINUTES: i64 = 10;
const FILE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterRequest {
    pub installation_id: u64,
    pub tunnel_url: String,
    #[serde(default)]
    pub claim_proof: Option<String>,
    #[serde(default)]
    pub update_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RegisterStatus {
    #[serde(rename = "claimed")]
    Claimed,
    #[serde(rename = "updated")]
    Updated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisterResponse {
    pub status: RegisterStatus,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    InvalidRequest(&'static str),
    InvalidTunnelUrl,
    UnsafeTunnelUrl,
    InvalidClaimProof,
    ExpiredClaimProof,
    ClaimProofAlreadyUsed,
    InvalidUpdateSecret,
    OwnershipMismatch,
    AlreadyClaimed,
    PersistenceFailure(String),
    SecretRotationFailure(String),
}

impl RegisterError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidTunnelUrl => "invalid_tunnel_url",
            Self::UnsafeTunnelUrl => "unsafe_tunnel_url",
            Self::InvalidClaimProof => "invalid_claim_proof",
            Self::ExpiredClaimProof => "expired_claim_proof",
            Self::ClaimProofAlreadyUsed => "claim_proof_already_used",
            Self::InvalidUpdateSecret => "invalid_update_secret",
            Self::OwnershipMismatch => "ownership_mismatch",
            Self::AlreadyClaimed => "already_claimed",
            Self::PersistenceFailure(_) => "persistence_failure",
            Self::SecretRotationFailure(_) => "secret_rotation_failure",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidRequest(message) => (*message).to_string(),
            Self::InvalidTunnelUrl => {
                "Tunnel URL must be an HTTPS origin without path, query, fragment, or userinfo."
                    .to_string()
            }
            Self::UnsafeTunnelUrl => {
                "Tunnel URL host must not be localhost, loopback, or a private network destination."
                    .to_string()
            }
            Self::InvalidClaimProof => "Claim proof is invalid.".to_string(),
            Self::ExpiredClaimProof => "Claim proof has expired.".to_string(),
            Self::ClaimProofAlreadyUsed => "Claim proof has already been used.".to_string(),
            Self::InvalidUpdateSecret => "Update secret is invalid.".to_string(),
            Self::OwnershipMismatch => {
                "Credential belongs to a different installation.".to_string()
            }
            Self::AlreadyClaimed => "Installation has already been claimed.".to_string(),
            Self::PersistenceFailure(message) | Self::SecretRotationFailure(message) => {
                message.clone()
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ClaimProof {
    issued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRegistration {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret_salt: String,
    pub update_secret_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_claim_proof_fingerprint: String,
    pub last_claim_proof_issued_at: DateTime<Utc>,
    pub last_claimed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsedClaimProofRecord {
    pub installation_id: u64,
    pub fingerprint: String,
    pub issued_at: DateTime<Utc>,
    pub consumed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostedProxyFileState {
    version: u32,
    installations: HashMap<String, StoredRegistration>,
    used_claim_proofs: HashMap<String, UsedClaimProofRecord>,
}

impl Default for HostedProxyFileState {
    fn default() -> Self {
        Self {
            version: FILE_FORMAT_VERSION,
            installations: HashMap::new(),
            used_claim_proofs: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaimOperation {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub proof_fingerprint: String,
    pub proof_issued_at: DateTime<Utc>,
    pub update_secret_salt: String,
    pub update_secret_hash: String,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct UpdateOperation {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub presented_update_secret: String,
    pub next_update_secret_salt: String,
    pub next_update_secret_hash: String,
    pub now: DateTime<Utc>,
}

pub trait HostedProxyStore: Send + Sync {
    fn claim(&self, operation: ClaimOperation) -> Result<StoredRegistration, RegisterError>;
    fn update(&self, operation: UpdateOperation) -> Result<StoredRegistration, RegisterError>;
    fn state_path(&self) -> &Path;
}

pub struct FileHostedProxyStore {
    path: PathBuf,
    state: Mutex<HostedProxyFileState>,
}

impl FileHostedProxyStore {
    pub fn new(path: PathBuf) -> Result<Self, RegisterError> {
        let state = if path.exists() {
            let content = fs::read_to_string(&path).map_err(|err| {
                RegisterError::PersistenceFailure(format!(
                    "Failed to read hosted proxy state at {}: {}",
                    path.display(),
                    err
                ))
            })?;
            serde_json::from_str::<HostedProxyFileState>(&content).map_err(|err| {
                RegisterError::PersistenceFailure(format!(
                    "Failed to parse hosted proxy state at {}: {}",
                    path.display(),
                    err
                ))
            })?
        } else {
            HostedProxyFileState::default()
        };

        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    fn persist_state(&self, state: &HostedProxyFileState) -> Result<(), RegisterError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                RegisterError::PersistenceFailure(format!(
                    "Failed to create hosted proxy state directory {}: {}",
                    parent.display(),
                    err
                ))
            })?;
        }

        let serialized = serde_json::to_vec_pretty(state).map_err(|err| {
            RegisterError::PersistenceFailure(format!(
                "Failed to serialize hosted proxy state for {}: {}",
                self.path.display(),
                err
            ))
        })?;
        let temp_path = self.path.with_extension("tmp");
        fs::write(&temp_path, serialized).map_err(|err| {
            RegisterError::PersistenceFailure(format!(
                "Failed to write hosted proxy temp state {}: {}",
                temp_path.display(),
                err
            ))
        })?;
        fs::rename(&temp_path, &self.path).map_err(|err| {
            RegisterError::PersistenceFailure(format!(
                "Failed to replace hosted proxy state {}: {}",
                self.path.display(),
                err
            ))
        })?;
        Ok(())
    }
}

impl HostedProxyStore for FileHostedProxyStore {
    fn claim(&self, operation: ClaimOperation) -> Result<StoredRegistration, RegisterError> {
        let mut guard = self.state.lock().map_err(|_| {
            RegisterError::PersistenceFailure("Hosted proxy state lock is poisoned.".to_string())
        })?;

        if guard
            .used_claim_proofs
            .contains_key(&operation.proof_fingerprint)
        {
            return Err(RegisterError::ClaimProofAlreadyUsed);
        }

        let installation_key = operation.installation_id.to_string();
        if guard.installations.contains_key(&installation_key) {
            return Err(RegisterError::AlreadyClaimed);
        }

        let mut next_state = guard.clone();
        let registration = StoredRegistration {
            installation_id: operation.installation_id,
            tunnel_url: operation.tunnel_url,
            update_secret_salt: operation.update_secret_salt,
            update_secret_hash: operation.update_secret_hash,
            created_at: operation.now,
            updated_at: operation.now,
            last_claim_proof_fingerprint: operation.proof_fingerprint.clone(),
            last_claim_proof_issued_at: operation.proof_issued_at,
            last_claimed_at: operation.now,
        };

        next_state
            .installations
            .insert(installation_key, registration.clone());
        next_state.used_claim_proofs.insert(
            operation.proof_fingerprint.clone(),
            UsedClaimProofRecord {
                installation_id: operation.installation_id,
                fingerprint: operation.proof_fingerprint,
                issued_at: operation.proof_issued_at,
                consumed_at: operation.now,
            },
        );

        self.persist_state(&next_state)?;
        *guard = next_state;
        Ok(registration)
    }

    fn update(&self, operation: UpdateOperation) -> Result<StoredRegistration, RegisterError> {
        let mut guard = self.state.lock().map_err(|_| {
            RegisterError::PersistenceFailure("Hosted proxy state lock is poisoned.".to_string())
        })?;

        let installation_key = operation.installation_id.to_string();
        let Some(current) = guard.installations.get(&installation_key) else {
            if guard.installations.values().any(|registration| {
                verify_update_secret(
                    &operation.presented_update_secret,
                    &registration.update_secret_salt,
                    &registration.update_secret_hash,
                )
            }) {
                return Err(RegisterError::OwnershipMismatch);
            }
            return Err(RegisterError::InvalidUpdateSecret);
        };

        if !verify_update_secret(
            &operation.presented_update_secret,
            &current.update_secret_salt,
            &current.update_secret_hash,
        ) {
            if guard.installations.values().any(|registration| {
                registration.installation_id != operation.installation_id
                    && verify_update_secret(
                        &operation.presented_update_secret,
                        &registration.update_secret_salt,
                        &registration.update_secret_hash,
                    )
            }) {
                return Err(RegisterError::OwnershipMismatch);
            }
            return Err(RegisterError::InvalidUpdateSecret);
        }

        let mut next_state = guard.clone();
        let registration = next_state
            .installations
            .get_mut(&installation_key)
            .expect("registration exists in cloned state");
        registration.tunnel_url = operation.tunnel_url;
        registration.update_secret_salt = operation.next_update_secret_salt;
        registration.update_secret_hash = operation.next_update_secret_hash;
        registration.updated_at = operation.now;

        let persisted = registration.clone();
        self.persist_state(&next_state)
            .map_err(|err| RegisterError::SecretRotationFailure(err.message()))?;
        *guard = next_state;
        Ok(persisted)
    }

    fn state_path(&self) -> &Path {
        &self.path
    }
}

pub struct HostedProxyService {
    claim_proof_secret: String,
    store: Box<dyn HostedProxyStore>,
}

impl HostedProxyService {
    pub fn new(claim_proof_secret: impl Into<String>, store: Box<dyn HostedProxyStore>) -> Self {
        Self {
            claim_proof_secret: claim_proof_secret.into(),
            store,
        }
    }

    pub fn mint_claim_proof(
        &self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<String, RegisterError> {
        if installation_id == 0 {
            return Err(RegisterError::InvalidRequest(
                "installation_id must be a positive integer.",
            ));
        }

        let nonce = Uuid::new_v4().simple().to_string();
        let payload = format!("v1:{installation_id}:{}:{nonce}", now.timestamp());
        let signature = self.sign_claim_payload(&payload);
        Ok(format!(
            "cp.v1.{installation_id}.{}.{}.{}",
            now.timestamp(),
            nonce,
            signature
        ))
    }

    pub fn register(
        &self,
        request: RegisterRequest,
        now: DateTime<Utc>,
    ) -> Result<RegisterResponse, RegisterError> {
        if request.installation_id == 0 {
            return Err(RegisterError::InvalidRequest(
                "installation_id must be a positive integer.",
            ));
        }

        match (&request.claim_proof, &request.update_secret) {
            (Some(_), Some(_)) => {
                return Err(RegisterError::InvalidRequest(
                    "Exactly one of claim_proof or update_secret must be present.",
                ))
            }
            (None, None) => {
                return Err(RegisterError::InvalidRequest(
                    "Exactly one of claim_proof or update_secret must be present.",
                ))
            }
            _ => {}
        }

        let tunnel_url = canonicalize_tunnel_url(&request.tunnel_url)?;

        if let Some(claim_proof) = request.claim_proof {
            let proof = self.verify_claim_proof(&claim_proof, request.installation_id, now)?;
            let update_secret = generate_update_secret();
            let (salt, hash) = hash_update_secret(&update_secret);
            let _ = self.store.claim(ClaimOperation {
                installation_id: request.installation_id,
                tunnel_url: tunnel_url.clone(),
                proof_fingerprint: fingerprint(&claim_proof),
                proof_issued_at: proof.issued_at,
                update_secret_salt: salt,
                update_secret_hash: hash,
                now,
            })?;

            return Ok(RegisterResponse {
                status: RegisterStatus::Claimed,
                installation_id: request.installation_id,
                tunnel_url,
                update_secret,
                rotated: false,
            });
        }

        let current_update_secret = request
            .update_secret
            .expect("update_secret present after auth field validation");
        let next_update_secret = generate_update_secret();
        let (salt, hash) = hash_update_secret(&next_update_secret);
        let _ = self.store.update(UpdateOperation {
            installation_id: request.installation_id,
            tunnel_url: tunnel_url.clone(),
            presented_update_secret: current_update_secret,
            next_update_secret_salt: salt,
            next_update_secret_hash: hash,
            now,
        })?;

        Ok(RegisterResponse {
            status: RegisterStatus::Updated,
            installation_id: request.installation_id,
            tunnel_url,
            update_secret: next_update_secret,
            rotated: true,
        })
    }

    pub fn storage_path(&self) -> &Path {
        self.store.state_path()
    }

    fn verify_claim_proof(
        &self,
        proof: &str,
        expected_installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<ClaimProof, RegisterError> {
        let parts: Vec<&str> = proof.split('.').collect();
        if parts.len() != 6 || parts[0] != "cp" || parts[1] != "v1" {
            return Err(RegisterError::InvalidClaimProof);
        }

        let installation_id = parts[2]
            .parse::<u64>()
            .map_err(|_| RegisterError::InvalidClaimProof)?;
        if installation_id != expected_installation_id {
            return Err(RegisterError::OwnershipMismatch);
        }

        let issued_at_timestamp = parts[3]
            .parse::<i64>()
            .map_err(|_| RegisterError::InvalidClaimProof)?;
        let issued_at = DateTime::<Utc>::from_timestamp(issued_at_timestamp, 0)
            .ok_or(RegisterError::InvalidClaimProof)?;
        if now > issued_at + Duration::minutes(CLAIM_PROOF_TTL_MINUTES) {
            return Err(RegisterError::ExpiredClaimProof);
        }

        let nonce = parts[4].to_string();
        let provided_signature = parts[5];
        let payload = format!("v1:{installation_id}:{issued_at_timestamp}:{nonce}");
        let expected_signature = self.sign_claim_payload(&payload);
        if provided_signature != expected_signature {
            return Err(RegisterError::InvalidClaimProof);
        }

        Ok(ClaimProof { issued_at })
    }

    fn sign_claim_payload(&self, payload: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.claim_proof_secret.as_bytes())
            .expect("HMAC accepts any key size");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

fn canonicalize_tunnel_url(raw: &str) -> Result<String, RegisterError> {
    let parsed = Url::parse(raw).map_err(|_| RegisterError::InvalidTunnelUrl)?;
    if parsed.scheme() != "https" {
        return Err(RegisterError::InvalidTunnelUrl);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RegisterError::InvalidTunnelUrl);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(RegisterError::InvalidTunnelUrl);
    }
    if parsed.path() != "/" {
        return Err(RegisterError::InvalidTunnelUrl);
    }

    let Some(host) = parsed.host_str() else {
        return Err(RegisterError::InvalidTunnelUrl);
    };
    if is_unsafe_host(host) {
        return Err(RegisterError::UnsafeTunnelUrl);
    }

    let canonical = match parsed.port() {
        Some(port) => format!("https://{}:{}", host.to_ascii_lowercase(), port),
        None => format!("https://{}", host.to_ascii_lowercase()),
    };
    Ok(canonical)
}

fn is_unsafe_host(host: &str) -> bool {
    let lowered = host.to_ascii_lowercase();
    if lowered == "localhost" || lowered.ends_with(".localhost") {
        return true;
    }

    let Ok(ip) = lowered.parse::<IpAddr>() else {
        return false;
    };
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_multicast()
                || ipv4.is_broadcast()
                || ipv4.is_unspecified()
                || ipv4.is_documentation()
        }
        IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_multicast()
                || ipv6.is_unique_local()
                || ipv6.is_unicast_link_local()
        }
    }
}

fn generate_update_secret() -> String {
    format!("us_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn hash_update_secret(secret: &str) -> (String, String) {
    let salt = Uuid::new_v4().simple().to_string();
    let hash = hash_material(&salt, secret);
    (salt, hash)
}

fn verify_update_secret(secret: &str, salt: &str, expected_hash: &str) -> bool {
    hash_material(salt, secret) == expected_hash
}

fn hash_material(salt: &str, material: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b":");
    hasher.update(material.as_bytes());
    hex::encode(hasher.finalize())
}

fn fingerprint(material: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(material.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{
        FileHostedProxyStore, HostedProxyService, RegisterError, RegisterRequest, RegisterStatus,
    };
    use chrono::{Duration, Utc};
    use tempfile::TempDir;

    #[test]
    fn claim_state_persists_across_restart_and_rejects_replay() {
        let tmp = TempDir::new().unwrap();
        let store = FileHostedProxyStore::new(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let service = HostedProxyService::new("test-proof-secret", Box::new(store));
        let now = Utc::now();

        let claim_proof = service
            .mint_claim_proof(42, now)
            .expect("claim proof should mint");
        let response = service
            .register(
                RegisterRequest {
                    installation_id: 42,
                    tunnel_url: "https://Example.COM:8443".to_string(),
                    claim_proof: Some(claim_proof.clone()),
                    update_secret: None,
                },
                now,
            )
            .expect("claim should succeed");

        assert_eq!(response.status, RegisterStatus::Claimed);
        assert_eq!(response.tunnel_url, "https://example.com:8443");

        let reloaded_store =
            FileHostedProxyStore::new(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let reloaded_service =
            HostedProxyService::new("test-proof-secret", Box::new(reloaded_store));

        let replay = reloaded_service.register(
            RegisterRequest {
                installation_id: 42,
                tunnel_url: "https://next.example.com".to_string(),
                claim_proof: Some(claim_proof),
                update_secret: None,
            },
            now + Duration::seconds(1),
        );

        assert!(matches!(replay, Err(RegisterError::ClaimProofAlreadyUsed)));
    }

    #[test]
    fn update_secret_rotation_persists_across_restart() {
        let tmp = TempDir::new().unwrap();
        let store = FileHostedProxyStore::new(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let service = HostedProxyService::new("test-proof-secret", Box::new(store));
        let now = Utc::now();

        let claim_proof = service.mint_claim_proof(7, now).unwrap();
        let claimed = service
            .register(
                RegisterRequest {
                    installation_id: 7,
                    tunnel_url: "https://first.example.com".to_string(),
                    claim_proof: Some(claim_proof),
                    update_secret: None,
                },
                now,
            )
            .unwrap();

        let reloaded_store =
            FileHostedProxyStore::new(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let reloaded_service =
            HostedProxyService::new("test-proof-secret", Box::new(reloaded_store));

        let updated = reloaded_service
            .register(
                RegisterRequest {
                    installation_id: 7,
                    tunnel_url: "https://second.example.com".to_string(),
                    claim_proof: None,
                    update_secret: Some(claimed.update_secret.clone()),
                },
                now + Duration::seconds(2),
            )
            .expect("update should succeed");

        assert_eq!(updated.status, RegisterStatus::Updated);
        assert_ne!(updated.update_secret, claimed.update_secret);

        let stale = reloaded_service.register(
            RegisterRequest {
                installation_id: 7,
                tunnel_url: "https://third.example.com".to_string(),
                claim_proof: None,
                update_secret: Some(claimed.update_secret),
            },
            now + Duration::seconds(3),
        );

        assert!(matches!(stale, Err(RegisterError::InvalidUpdateSecret)));
    }

    #[test]
    fn invalid_claim_does_not_consume_proof() {
        let tmp = TempDir::new().unwrap();
        let store = FileHostedProxyStore::new(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let service = HostedProxyService::new("test-proof-secret", Box::new(store));
        let now = Utc::now();

        let claim_proof = service.mint_claim_proof(9, now).unwrap();

        let invalid_attempt = service.register(
            RegisterRequest {
                installation_id: 9,
                tunnel_url: "http://insecure.example.com".to_string(),
                claim_proof: Some(claim_proof.clone()),
                update_secret: None,
            },
            now,
        );

        assert!(matches!(
            invalid_attempt,
            Err(RegisterError::InvalidTunnelUrl)
        ));

        let valid_attempt = service
            .register(
                RegisterRequest {
                    installation_id: 9,
                    tunnel_url: "https://valid.example.com".to_string(),
                    claim_proof: Some(claim_proof),
                    update_secret: None,
                },
                now + Duration::seconds(1),
            )
            .expect("proof should still be usable");

        assert_eq!(valid_attempt.status, RegisterStatus::Claimed);
    }
}
