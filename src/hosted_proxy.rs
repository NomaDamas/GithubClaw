use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

const CLAIM_PROOF_PREFIX: &str = "cp";
const UPDATE_SECRET_PREFIX: &str = "us";
const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegisterRequest {
    pub installation_id: u64,
    pub tunnel_url: String,
    #[serde(default)]
    pub claim_proof: Option<String>,
    #[serde(default)]
    pub update_secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RegisterSuccess {
    pub status: String,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub error: ErrorInfo,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterResultKind {
    Claimed,
    Updated,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum HostedProxyError {
    #[error("invalid_request")]
    InvalidRequest,
    #[error("invalid_tunnel_url")]
    InvalidTunnelUrl,
    #[error("unsafe_tunnel_url")]
    UnsafeTunnelUrl,
    #[error("invalid_claim_proof")]
    InvalidClaimProof,
    #[error("expired_claim_proof")]
    ExpiredClaimProof,
    #[error("claim_proof_already_used")]
    ClaimProofAlreadyUsed,
    #[error("invalid_update_secret")]
    InvalidUpdateSecret,
    #[error("ownership_mismatch")]
    OwnershipMismatch,
    #[error("already_claimed")]
    AlreadyClaimed,
    #[error("persistence_failure")]
    PersistenceFailure,
    #[error("secret_rotation_failure")]
    SecretRotationFailure,
}

impl HostedProxyError {
    pub fn status_code(&self) -> StatusCode {
        match self {
            Self::InvalidRequest | Self::InvalidTunnelUrl | Self::UnsafeTunnelUrl => {
                StatusCode::BAD_REQUEST
            }
            Self::InvalidClaimProof
            | Self::ExpiredClaimProof
            | Self::ClaimProofAlreadyUsed
            | Self::InvalidUpdateSecret
            | Self::OwnershipMismatch => StatusCode::FORBIDDEN,
            Self::AlreadyClaimed => StatusCode::CONFLICT,
            Self::PersistenceFailure | Self::SecretRotationFailure => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    pub fn body(&self) -> ErrorBody {
        let (code, message) = match self {
            Self::InvalidRequest => (
                "invalid_request",
                "Request must include exactly one of claim_proof or update_secret.",
            ),
            Self::InvalidTunnelUrl => (
                "invalid_tunnel_url",
                "Tunnel URL must be a valid HTTPS origin.",
            ),
            Self::UnsafeTunnelUrl => (
                "unsafe_tunnel_url",
                "Tunnel URL must not target localhost, loopback, or private networks.",
            ),
            Self::InvalidClaimProof => (
                "invalid_claim_proof",
                "Claim proof is invalid for this installation.",
            ),
            Self::ExpiredClaimProof => ("expired_claim_proof", "Claim proof has expired."),
            Self::ClaimProofAlreadyUsed => (
                "claim_proof_already_used",
                "Claim proof was already used successfully.",
            ),
            Self::InvalidUpdateSecret => (
                "invalid_update_secret",
                "Update secret is invalid for this installation.",
            ),
            Self::OwnershipMismatch => (
                "ownership_mismatch",
                "Credential belongs to a different installation.",
            ),
            Self::AlreadyClaimed => (
                "already_claimed",
                "Installation has already been claimed; use update_secret instead.",
            ),
            Self::PersistenceFailure => (
                "persistence_failure",
                "Hosted proxy state could not be persisted.",
            ),
            Self::SecretRotationFailure => (
                "secret_rotation_failure",
                "Hosted proxy update secret could not be rotated.",
            ),
        };

        ErrorBody {
            error: ErrorInfo {
                code: code.to_string(),
                message: message.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstallationState {
    pub tunnel_url: String,
    pub update_secret_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub used_claim_proof_hashes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct HostedProxySnapshot {
    #[serde(default)]
    pub installations: HashMap<u64, InstallationState>,
}

pub trait HostedProxyStateStore: Send + Sync {
    fn load(&self) -> io::Result<HostedProxySnapshot>;
    fn save(&self, snapshot: &HostedProxySnapshot) -> io::Result<()>;
}

#[derive(Debug, Clone)]
pub struct FileHostedProxyStateStore {
    path: PathBuf,
}

impl FileHostedProxyStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl HostedProxyStateStore for FileHostedProxyStateStore {
    fn load(&self) -> io::Result<HostedProxySnapshot> {
        if !self.path.exists() {
            return Ok(HostedProxySnapshot::default());
        }

        let contents = fs::read_to_string(&self.path)?;
        let snapshot = serde_json::from_str(&contents).unwrap_or_default();
        Ok(snapshot)
    }

    fn save(&self, snapshot: &HostedProxySnapshot) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp_path = self.path.with_extension("tmp");
        let contents = serde_json::to_vec_pretty(snapshot)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        fs::write(&tmp_path, contents)?;
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }
}

pub struct HostedProxyService<S: HostedProxyStateStore> {
    store: S,
    claim_proof_secret: String,
    now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    next_update_secret: Box<dyn Fn() -> String + Send + Sync>,
}

impl<S: HostedProxyStateStore> HostedProxyService<S> {
    pub fn new(store: S, claim_proof_secret: impl Into<String>) -> Self {
        Self::new_with_hooks(
            store,
            claim_proof_secret,
            Box::new(Utc::now),
            Box::new(default_update_secret),
        )
    }

    pub fn new_with_hooks(
        store: S,
        claim_proof_secret: impl Into<String>,
        now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
        next_update_secret: Box<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self {
            store,
            claim_proof_secret: claim_proof_secret.into(),
            now,
            next_update_secret,
        }
    }

    pub fn register(
        &self,
        request: RegisterRequest,
    ) -> Result<(RegisterResultKind, RegisterSuccess), HostedProxyError> {
        if request.installation_id == 0 {
            return Err(HostedProxyError::InvalidRequest);
        }

        let using_claim = request.claim_proof.is_some();
        let using_update_secret = request.update_secret.is_some();
        if using_claim == using_update_secret {
            return Err(HostedProxyError::InvalidRequest);
        }

        let tunnel_url = normalize_tunnel_url(&request.tunnel_url)?;
        let now = (self.now)();
        let snapshot = self
            .store
            .load()
            .map_err(|_| HostedProxyError::PersistenceFailure)?;

        if let Some(claim_proof) = request.claim_proof {
            return self.claim(
                snapshot,
                request.installation_id,
                tunnel_url,
                claim_proof,
                now,
            );
        }

        self.update(
            snapshot,
            request.installation_id,
            tunnel_url,
            request.update_secret.expect("validated above"),
            now,
        )
    }

    fn claim(
        &self,
        mut snapshot: HostedProxySnapshot,
        installation_id: u64,
        tunnel_url: String,
        claim_proof: String,
        now: DateTime<Utc>,
    ) -> Result<(RegisterResultKind, RegisterSuccess), HostedProxyError> {
        verify_claim_proof(
            &self.claim_proof_secret,
            &snapshot,
            installation_id,
            &claim_proof,
            now,
        )?;

        if snapshot.installations.contains_key(&installation_id) {
            return Err(HostedProxyError::AlreadyClaimed);
        }

        let update_secret = (self.next_update_secret)();
        let claim_proof_hash = hash_secret(&claim_proof);
        let state = InstallationState {
            tunnel_url: tunnel_url.clone(),
            update_secret_hash: hash_secret(&update_secret),
            created_at: now,
            updated_at: now,
            used_claim_proof_hashes: vec![claim_proof_hash],
        };
        snapshot.installations.insert(installation_id, state);
        self.store
            .save(&snapshot)
            .map_err(|_| HostedProxyError::PersistenceFailure)?;

        Ok((
            RegisterResultKind::Claimed,
            RegisterSuccess {
                status: "claimed".to_string(),
                installation_id,
                tunnel_url,
                update_secret,
                rotated: false,
            },
        ))
    }

    fn update(
        &self,
        mut snapshot: HostedProxySnapshot,
        installation_id: u64,
        tunnel_url: String,
        update_secret: String,
        now: DateTime<Utc>,
    ) -> Result<(RegisterResultKind, RegisterSuccess), HostedProxyError> {
        let supplied_hash = hash_secret(&update_secret);
        let secret_belongs_elsewhere = snapshot.installations.iter().any(|(other_id, state)| {
            *other_id != installation_id && state.update_secret_hash == supplied_hash
        });

        let Some(current_state) = snapshot.installations.get_mut(&installation_id) else {
            if secret_belongs_elsewhere {
                return Err(HostedProxyError::OwnershipMismatch);
            }
            return Err(HostedProxyError::InvalidUpdateSecret);
        };

        let replacement_secret = (self.next_update_secret)();
        if current_state.update_secret_hash != supplied_hash {
            return if secret_belongs_elsewhere {
                Err(HostedProxyError::OwnershipMismatch)
            } else {
                Err(HostedProxyError::InvalidUpdateSecret)
            };
        }
        current_state.tunnel_url = tunnel_url.clone();
        current_state.update_secret_hash = hash_secret(&replacement_secret);
        current_state.updated_at = now;
        self.store
            .save(&snapshot)
            .map_err(|_| HostedProxyError::SecretRotationFailure)?;

        Ok((
            RegisterResultKind::Updated,
            RegisterSuccess {
                status: "updated".to_string(),
                installation_id,
                tunnel_url,
                update_secret: replacement_secret,
                rotated: true,
            },
        ))
    }
}

pub fn mint_claim_proof(
    claim_proof_secret: &str,
    installation_id: u64,
    issued_at: DateTime<Utc>,
) -> String {
    let issued_at_ts = issued_at.timestamp();
    let payload = format!("{installation_id}:{issued_at_ts}");
    let signature = sign_claim_payload(claim_proof_secret, &payload);
    format!("{CLAIM_PROOF_PREFIX}_{installation_id}_{issued_at_ts}_{signature}")
}

fn verify_claim_proof(
    claim_proof_secret: &str,
    snapshot: &HostedProxySnapshot,
    installation_id: u64,
    claim_proof: &str,
    now: DateTime<Utc>,
) -> Result<(), HostedProxyError> {
    let Some(proof_body) = claim_proof.strip_prefix(&format!("{CLAIM_PROOF_PREFIX}_")) else {
        return Err(HostedProxyError::InvalidClaimProof);
    };

    let mut parts = proof_body.splitn(3, '_');
    let proof_installation_id = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .ok_or(HostedProxyError::InvalidClaimProof)?;
    let issued_at_ts = parts
        .next()
        .and_then(|part| part.parse::<i64>().ok())
        .ok_or(HostedProxyError::InvalidClaimProof)?;
    let provided_signature = parts.next().ok_or(HostedProxyError::InvalidClaimProof)?;

    if proof_installation_id != installation_id {
        return Err(HostedProxyError::OwnershipMismatch);
    }

    let payload = format!("{proof_installation_id}:{issued_at_ts}");
    let expected_signature = sign_claim_payload(claim_proof_secret, &payload);
    if expected_signature != provided_signature {
        return Err(HostedProxyError::InvalidClaimProof);
    }

    let issued_at = DateTime::<Utc>::from_timestamp(issued_at_ts, 0)
        .ok_or(HostedProxyError::InvalidClaimProof)?;
    if now > issued_at + Duration::minutes(CLAIM_PROOF_TTL_MINUTES) {
        return Err(HostedProxyError::ExpiredClaimProof);
    }

    let claim_hash = hash_secret(claim_proof);
    if snapshot.installations.values().any(|state| {
        state
            .used_claim_proof_hashes
            .iter()
            .any(|used| used == &claim_hash)
    }) {
        return Err(HostedProxyError::ClaimProofAlreadyUsed);
    }

    Ok(())
}

fn normalize_tunnel_url(raw: &str) -> Result<String, HostedProxyError> {
    let url = Url::parse(raw).map_err(|_| HostedProxyError::InvalidTunnelUrl)?;
    if url.scheme() != "https" {
        return Err(HostedProxyError::InvalidTunnelUrl);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl);
    }
    if url.path() != "/" && !url.path().is_empty() {
        return Err(HostedProxyError::InvalidTunnelUrl);
    }

    let host = url.host_str().ok_or(HostedProxyError::InvalidTunnelUrl)?;
    if is_unsafe_host(host) {
        return Err(HostedProxyError::UnsafeTunnelUrl);
    }

    let mut normalized = format!("https://{host}");
    if let Some(port) = url.port() {
        normalized.push(':');
        normalized.push_str(&port.to_string());
    }
    Ok(normalized)
}

fn is_unsafe_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
            }
        };
    }

    false
}

fn sign_claim_payload(secret: &str, payload: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn hash_secret(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(digest)
}

fn default_update_secret() -> String {
    format!("{UPDATE_SECRET_PREFIX}_{}", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn file_store_round_trip_preserves_installation_state() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir
            .path()
            .join("hosted_proxy")
            .join("installations.json");
        let store = FileHostedProxyStateStore::new(&path);

        let now = Utc::now();
        let mut snapshot = HostedProxySnapshot::default();
        snapshot.installations.insert(
            42,
            InstallationState {
                tunnel_url: "https://example.com".to_string(),
                update_secret_hash: "hash".to_string(),
                created_at: now,
                updated_at: now,
                used_claim_proof_hashes: vec!["proof-hash".to_string()],
            },
        );

        store.save(&snapshot).unwrap();
        let reloaded = store.load().unwrap();

        assert_eq!(reloaded, snapshot);
    }

    #[test]
    fn claim_replay_and_secret_rotation_survive_reload() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir
            .path()
            .join("hosted_proxy")
            .join("installations.json");
        let issued_at = DateTime::<Utc>::from_timestamp(1_731_000_000, 0).unwrap();

        let service = HostedProxyService::new_with_hooks(
            FileHostedProxyStateStore::new(&path),
            "claim-secret",
            Box::new(move || issued_at + Duration::minutes(5)),
            Box::new(|| "us_initial".to_string()),
        );
        let claim_proof = mint_claim_proof("claim-secret", 7, issued_at);
        let (_, claim_response) = service
            .register(RegisterRequest {
                installation_id: 7,
                tunnel_url: "https://first.example.com/".to_string(),
                claim_proof: Some(claim_proof.clone()),
                update_secret: None,
            })
            .unwrap();
        assert_eq!(claim_response.update_secret, "us_initial");

        let replay_service = HostedProxyService::new_with_hooks(
            FileHostedProxyStateStore::new(&path),
            "claim-secret",
            Box::new(move || issued_at + Duration::minutes(5)),
            Box::new(|| "us_unused".to_string()),
        );
        let replay_error = replay_service
            .register(RegisterRequest {
                installation_id: 7,
                tunnel_url: "https://first.example.com".to_string(),
                claim_proof: Some(claim_proof),
                update_secret: None,
            })
            .unwrap_err();
        assert_eq!(replay_error, HostedProxyError::ClaimProofAlreadyUsed);

        let update_service = HostedProxyService::new_with_hooks(
            FileHostedProxyStateStore::new(&path),
            "claim-secret",
            Box::new(move || issued_at + Duration::minutes(6)),
            Box::new(|| "us_rotated".to_string()),
        );
        let (_, update_response) = update_service
            .register(RegisterRequest {
                installation_id: 7,
                tunnel_url: "https://next.example.com/".to_string(),
                claim_proof: None,
                update_secret: Some("us_initial".to_string()),
            })
            .unwrap();
        assert_eq!(update_response.update_secret, "us_rotated");
        assert_eq!(update_response.tunnel_url, "https://next.example.com");
    }

    #[test]
    fn stale_update_secret_is_rejected_after_reload() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir
            .path()
            .join("hosted_proxy")
            .join("installations.json");
        let issued_at = DateTime::<Utc>::from_timestamp(1_731_000_000, 0).unwrap();
        let claim_proof = mint_claim_proof("claim-secret", 9, issued_at);

        let claim_service = HostedProxyService::new_with_hooks(
            FileHostedProxyStateStore::new(&path),
            "claim-secret",
            Box::new(move || issued_at + Duration::minutes(2)),
            Box::new(|| "us_initial".to_string()),
        );
        claim_service
            .register(RegisterRequest {
                installation_id: 9,
                tunnel_url: "https://first.example.com".to_string(),
                claim_proof: Some(claim_proof),
                update_secret: None,
            })
            .unwrap();

        let update_service = HostedProxyService::new_with_hooks(
            FileHostedProxyStateStore::new(&path),
            "claim-secret",
            Box::new(move || issued_at + Duration::minutes(3)),
            Box::new(|| "us_rotated".to_string()),
        );
        let (_, update_response) = update_service
            .register(RegisterRequest {
                installation_id: 9,
                tunnel_url: "https://second.example.com/".to_string(),
                claim_proof: None,
                update_secret: Some("us_initial".to_string()),
            })
            .unwrap();
        assert_eq!(update_response.update_secret, "us_rotated");
        assert_eq!(update_response.tunnel_url, "https://second.example.com");

        let stale_service = HostedProxyService::new_with_hooks(
            FileHostedProxyStateStore::new(&path),
            "claim-secret",
            Box::new(move || issued_at + Duration::minutes(4)),
            Box::new(|| "us_unused".to_string()),
        );
        let stale_error = stale_service
            .register(RegisterRequest {
                installation_id: 9,
                tunnel_url: "https://third.example.com".to_string(),
                claim_proof: None,
                update_secret: Some("us_initial".to_string()),
            })
            .unwrap_err();
        assert_eq!(stale_error, HostedProxyError::InvalidUpdateSecret);
    }
}
