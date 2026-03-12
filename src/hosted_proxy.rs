use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use uuid::Uuid;

const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug)]
pub struct HostedProxyState {
    inner: Mutex<HostedProxyStore>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedInstallation {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimProofRecord {
    pub proof_digest: String,
    pub installation_id: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedClaimProof {
    pub installation_id: u64,
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSuccess {
    pub status: String,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedProxyError {
    InvalidRequest { message: &'static str },
    InvalidTunnelUrl { message: &'static str },
    UnsafeTunnelUrl { message: &'static str },
    InvalidClaimProof,
    ExpiredClaimProof,
    ClaimProofAlreadyUsed,
    InvalidUpdateSecret,
    OwnershipMismatch,
    AlreadyClaimed,
    PersistenceFailure(String),
}

impl HostedProxyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest { .. } => "invalid_request",
            Self::InvalidTunnelUrl { .. } => "invalid_tunnel_url",
            Self::UnsafeTunnelUrl { .. } => "unsafe_tunnel_url",
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
            Self::InvalidRequest { message }
            | Self::InvalidTunnelUrl { message }
            | Self::UnsafeTunnelUrl { message } => (*message).to_string(),
            Self::InvalidClaimProof => "claim_proof is invalid".to_string(),
            Self::ExpiredClaimProof => "claim_proof has expired".to_string(),
            Self::ClaimProofAlreadyUsed => "claim_proof has already been used".to_string(),
            Self::InvalidUpdateSecret => "update_secret is invalid".to_string(),
            Self::OwnershipMismatch => {
                "credential is bound to a different installation_id".to_string()
            }
            Self::AlreadyClaimed => "installation has already been claimed".to_string(),
            Self::PersistenceFailure(message) => message.clone(),
        }
    }
}

impl fmt::Display for HostedProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for HostedProxyError {}

#[derive(Debug)]
struct HostedProxyStore {
    path: PathBuf,
    installations: HashMap<u64, HostedInstallation>,
    claim_proofs: HashMap<String, ClaimProofRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HostedProxyFile {
    #[serde(default)]
    installations: Vec<HostedInstallation>,
    #[serde(default)]
    claim_proofs: Vec<ClaimProofRecord>,
}

impl HostedProxyState {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, HostedProxyError> {
        let store = HostedProxyStore::load(path.as_ref())
            .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
        Ok(Self {
            inner: Mutex::new(store),
        })
    }

    pub async fn issue_claim_proof(
        &self,
        installation_id: u64,
    ) -> Result<IssuedClaimProof, HostedProxyError> {
        self.issue_claim_proof_at(installation_id, Utc::now()).await
    }

    pub async fn issue_claim_proof_at(
        &self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<IssuedClaimProof, HostedProxyError> {
        if installation_id == 0 {
            return Err(HostedProxyError::InvalidRequest {
                message: "installation_id must be greater than 0",
            });
        }

        let claim_proof = format!("cp_{}", Uuid::new_v4().simple());
        let record = ClaimProofRecord {
            proof_digest: digest_claim_proof(&claim_proof),
            installation_id,
            issued_at: now,
            expires_at: now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES),
            used_at: None,
        };

        let mut store = self.inner.lock().await;
        store
            .claim_proofs
            .insert(record.proof_digest.clone(), record.clone());
        store.persist()?;

        Ok(IssuedClaimProof {
            installation_id,
            claim_proof,
            expires_at: record.expires_at,
        })
    }

    pub async fn register(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: Option<String>,
        update_secret: Option<String>,
    ) -> Result<RegisterSuccess, HostedProxyError> {
        self.register_at(
            installation_id,
            tunnel_url,
            claim_proof,
            update_secret,
            Utc::now(),
        )
        .await
    }

    pub async fn register_at(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: Option<String>,
        update_secret: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, HostedProxyError> {
        if installation_id == 0 {
            return Err(HostedProxyError::InvalidRequest {
                message: "installation_id must be greater than 0",
            });
        }

        let normalized_tunnel_url = canonicalize_tunnel_url(tunnel_url)?;
        match (claim_proof, update_secret) {
            (Some(_), Some(_)) | (None, None) => Err(HostedProxyError::InvalidRequest {
                message: "exactly one of claim_proof or update_secret must be present",
            }),
            (Some(proof), None) => {
                self.claim_installation(installation_id, normalized_tunnel_url, proof, now)
                    .await
            }
            (None, Some(secret)) => {
                self.update_installation(installation_id, normalized_tunnel_url, secret, now)
                    .await
            }
        }
    }

    #[cfg(test)]
    pub async fn get_claim_record(&self, claim_proof: &str) -> Option<ClaimProofRecord> {
        let store = self.inner.lock().await;
        store
            .claim_proofs
            .get(&digest_claim_proof(claim_proof))
            .cloned()
    }

    async fn claim_installation(
        &self,
        installation_id: u64,
        tunnel_url: String,
        claim_proof: String,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, HostedProxyError> {
        let proof_digest = digest_claim_proof(&claim_proof);
        let mut store = self.inner.lock().await;
        let proof_record = store
            .claim_proofs
            .get(&proof_digest)
            .cloned()
            .ok_or(HostedProxyError::InvalidClaimProof)?;

        if proof_record.installation_id != installation_id {
            return Err(HostedProxyError::OwnershipMismatch);
        }
        if proof_record.used_at.is_some() {
            return Err(HostedProxyError::ClaimProofAlreadyUsed);
        }
        if proof_record.expires_at < now {
            return Err(HostedProxyError::ExpiredClaimProof);
        }
        if store.installations.contains_key(&installation_id) {
            return Err(HostedProxyError::AlreadyClaimed);
        }

        let next_secret = new_update_secret();
        store.installations.insert(
            installation_id,
            HostedInstallation {
                installation_id,
                tunnel_url: tunnel_url.clone(),
                update_secret: next_secret.clone(),
                created_at: now,
                updated_at: now,
            },
        );

        if let Some(record) = store.claim_proofs.get_mut(&proof_digest) {
            record.used_at = Some(now);
        }
        store.persist()?;

        Ok(RegisterSuccess {
            status: "claimed".to_string(),
            installation_id,
            tunnel_url,
            update_secret: next_secret,
            rotated: false,
        })
    }

    async fn update_installation(
        &self,
        installation_id: u64,
        tunnel_url: String,
        update_secret: String,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, HostedProxyError> {
        let mut store = self.inner.lock().await;
        if store.installations.iter().any(|(id, installation)| {
            *id != installation_id && installation.update_secret == update_secret
        }) {
            return Err(HostedProxyError::OwnershipMismatch);
        }

        let current = store
            .installations
            .get(&installation_id)
            .cloned()
            .ok_or(HostedProxyError::InvalidUpdateSecret)?;
        if current.update_secret != update_secret {
            return Err(HostedProxyError::InvalidUpdateSecret);
        }

        let next_secret = new_update_secret();
        store.installations.insert(
            installation_id,
            HostedInstallation {
                installation_id,
                tunnel_url: tunnel_url.clone(),
                update_secret: next_secret.clone(),
                created_at: current.created_at,
                updated_at: now,
            },
        );
        store.persist()?;

        Ok(RegisterSuccess {
            status: "updated".to_string(),
            installation_id,
            tunnel_url,
            update_secret: next_secret,
            rotated: true,
        })
    }
}

impl HostedProxyStore {
    fn load(path: &Path) -> std::io::Result<Self> {
        let path = path.to_path_buf();
        if !path.exists() {
            return Ok(Self {
                path,
                installations: HashMap::new(),
                claim_proofs: HashMap::new(),
            });
        }

        let content = std::fs::read_to_string(&path)?;
        let file: HostedProxyFile = serde_json::from_str(&content).map_err(invalid_data)?;
        let installations = file
            .installations
            .into_iter()
            .map(|installation| (installation.installation_id, installation))
            .collect();
        let claim_proofs = file
            .claim_proofs
            .into_iter()
            .map(|record| (record.proof_digest.clone(), record))
            .collect();

        Ok(Self {
            path,
            installations,
            claim_proofs,
        })
    }

    fn persist(&self) -> Result<(), HostedProxyError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
        }

        let mut installations: Vec<_> = self.installations.values().cloned().collect();
        installations.sort_by_key(|installation| installation.installation_id);

        let mut claim_proofs: Vec<_> = self.claim_proofs.values().cloned().collect();
        claim_proofs.sort_by(|left, right| {
            left.issued_at
                .cmp(&right.issued_at)
                .then_with(|| left.proof_digest.cmp(&right.proof_digest))
        });

        let content = serde_json::to_string_pretty(&HostedProxyFile {
            installations,
            claim_proofs,
        })
        .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;

        std::fs::write(&self.path, format!("{content}\n"))
            .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
        Ok(())
    }
}

fn digest_claim_proof(claim_proof: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(claim_proof.as_bytes());
    hex::encode(hasher.finalize())
}

fn new_update_secret() -> String {
    format!("us_{}", Uuid::new_v4().simple())
}

pub fn canonicalize_tunnel_url(input: &str) -> Result<String, HostedProxyError> {
    let trimmed = input.trim();
    let url = Url::parse(trimmed).map_err(|_| HostedProxyError::InvalidTunnelUrl {
        message: "tunnel_url must be a valid absolute URL",
    })?;

    if url.scheme() != "https" {
        return Err(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must use the https scheme",
        });
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must not include embedded credentials",
        });
    }

    if url.query().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must not include a query string",
        });
    }

    if url.fragment().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must not include a fragment",
        });
    }

    if url.path() != "/" {
        return Err(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must be an origin without a path",
        });
    }

    let host = url
        .host_str()
        .ok_or(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must include a host",
        })?
        .trim_end_matches('.')
        .to_ascii_lowercase();

    if host.is_empty() {
        return Err(HostedProxyError::InvalidTunnelUrl {
            message: "tunnel_url must include a host",
        });
    }

    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return Err(HostedProxyError::UnsafeTunnelUrl {
            message: "tunnel_url must not target localhost or an internal host",
        });
    }

    let ip_candidate = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_candidate.parse::<IpAddr>() {
        let message = if is_forbidden_ip(ip) {
            "tunnel_url must not target loopback or private network addresses"
        } else {
            "tunnel_url must use a DNS hostname instead of an IP literal"
        };
        return Err(HostedProxyError::UnsafeTunnelUrl { message });
    }

    let mut canonical = format!("https://{host}");
    if let Some(port) = url.port() {
        if port != 443 {
            canonical.push(':');
            canonical.push_str(&port.to_string());
        }
    }

    Ok(canonical)
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_broadcast()
                || ipv4.is_unspecified()
                || ipv4.octets()[0] == 0
        }
        IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_unique_local()
                || ipv6.is_unicast_link_local()
                || ipv6 == Ipv6Addr::LOCALHOST
        }
    }
}

fn invalid_data(error: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_tunnel_url, HostedProxyError, HostedProxyState};
    use chrono::{Duration, Utc};
    use tempfile::TempDir;

    #[test]
    fn canonicalizes_https_origin_without_default_port() {
        let canonical = canonicalize_tunnel_url("https://AbC123.TryCloudflare.com:443/").unwrap();
        assert_eq!(canonical, "https://abc123.trycloudflare.com");
    }

    #[test]
    fn rejects_path_query_fragment_and_userinfo() {
        for input in [
            "https://example.com/hook",
            "https://example.com?token=1",
            "https://example.com#fragment",
            "https://user@example.com",
        ] {
            let error = canonicalize_tunnel_url(input).unwrap_err();
            assert_eq!(error.code(), "invalid_tunnel_url");
        }
    }

    #[test]
    fn rejects_unsafe_local_and_private_destinations() {
        for input in [
            "http://example.com",
            "https://localhost",
            "https://127.0.0.1",
            "https://10.0.0.8",
            "https://192.168.1.20",
            "https://[::1]",
        ] {
            let result = canonicalize_tunnel_url(input);
            assert!(
                matches!(
                    result,
                    Err(HostedProxyError::InvalidTunnelUrl { .. }
                        | HostedProxyError::UnsafeTunnelUrl { .. })
                ),
                "input {input} unexpectedly returned {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn issues_single_use_claim_proof_bound_to_one_installation() {
        let tmp = TempDir::new().unwrap();
        let state = HostedProxyState::load(tmp.path().join("hosted_proxy_state.json")).unwrap();

        let issued = state.issue_claim_proof_at(42, Utc::now()).await.unwrap();
        let stored = state.get_claim_record(&issued.claim_proof).await.unwrap();

        assert_eq!(issued.installation_id, 42);
        assert!(issued.claim_proof.starts_with("cp_"));
        assert_eq!(stored.installation_id, 42);
        assert_eq!(stored.used_at, None);
        assert_eq!(stored.expires_at, issued.expires_at);
    }

    #[tokio::test]
    async fn rejects_expired_claim_proof() {
        let tmp = TempDir::new().unwrap();
        let state = HostedProxyState::load(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let issued_at = Utc::now() - Duration::minutes(11);
        let issued = state.issue_claim_proof_at(42, issued_at).await.unwrap();

        let error = state
            .register_at(
                42,
                "https://abc.trycloudflare.com",
                Some(issued.claim_proof),
                None,
                Utc::now(),
            )
            .await
            .unwrap_err();

        assert_eq!(error.code(), "expired_claim_proof");
    }

    #[tokio::test]
    async fn initial_claim_consumes_proof_and_replay_fails() {
        let tmp = TempDir::new().unwrap();
        let state = HostedProxyState::load(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let issued = state.issue_claim_proof(42).await.unwrap();

        let created = state
            .register(
                42,
                "https://abc.trycloudflare.com",
                Some(issued.claim_proof.clone()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(created.status, "claimed");
        assert!(!created.rotated);

        let replay = state
            .register(
                42,
                "https://next.trycloudflare.com",
                Some(issued.claim_proof),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(replay.code(), "claim_proof_already_used");
    }

    #[tokio::test]
    async fn rejects_claim_proof_for_the_wrong_installation() {
        let tmp = TempDir::new().unwrap();
        let state = HostedProxyState::load(tmp.path().join("hosted_proxy_state.json")).unwrap();
        let issued = state.issue_claim_proof(42).await.unwrap();

        let error = state
            .register(
                7,
                "https://abc.trycloudflare.com",
                Some(issued.claim_proof),
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(error.code(), "ownership_mismatch");
    }
}
