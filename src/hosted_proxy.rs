use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug)]
pub struct HostedProxyState {
    proof_secret: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimProofClaims {
    installation_id: u64,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    nonce: String,
}

#[derive(Debug, Clone)]
struct VerifiedClaimProof {
    installation_id: u64,
}

#[derive(Debug)]
struct HostedProxyStore {
    path: PathBuf,
    installations: HashMap<u64, HostedInstallation>,
    used_claim_proofs: HashSet<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HostedProxyFile {
    #[serde(default)]
    installations: Vec<HostedInstallation>,
    #[serde(default)]
    used_claim_proofs: Vec<String>,
}

impl HostedProxyState {
    pub fn load(
        path: impl AsRef<Path>,
        proof_secret: impl Into<String>,
    ) -> Result<Self, HostedProxyError> {
        let store = HostedProxyStore::load(path.as_ref())
            .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
        Ok(Self {
            proof_secret: proof_secret.into(),
            inner: Mutex::new(store),
        })
    }

    pub async fn register(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: Option<String>,
        update_secret: Option<String>,
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
                self.claim_installation(installation_id, normalized_tunnel_url, proof)
                    .await
            }
            (None, Some(secret)) => {
                self.update_installation(installation_id, normalized_tunnel_url, secret)
                    .await
            }
        }
    }

    pub async fn get_installation(&self, installation_id: u64) -> Option<HostedInstallation> {
        let store = self.inner.lock().await;
        store.installations.get(&installation_id).cloned()
    }

    async fn claim_installation(
        &self,
        installation_id: u64,
        tunnel_url: String,
        claim_proof: String,
    ) -> Result<RegisterSuccess, HostedProxyError> {
        let verified = verify_claim_proof(&claim_proof, &self.proof_secret)?;
        if verified.installation_id != installation_id {
            return Err(HostedProxyError::OwnershipMismatch);
        }

        let mut store = self.inner.lock().await;
        if store.installations.contains_key(&installation_id) {
            return Err(HostedProxyError::AlreadyClaimed);
        }
        if store.used_claim_proofs.contains(&claim_proof) {
            return Err(HostedProxyError::ClaimProofAlreadyUsed);
        }

        let now = Utc::now();
        let next_secret = new_update_secret();
        let installation = HostedInstallation {
            installation_id,
            tunnel_url: tunnel_url.clone(),
            update_secret: next_secret.clone(),
            created_at: now,
            updated_at: now,
        };

        let mut installations = store.installations.clone();
        installations.insert(installation_id, installation);
        let mut used_claim_proofs = store.used_claim_proofs.clone();
        used_claim_proofs.insert(claim_proof);
        store.persist_snapshot(&installations, &used_claim_proofs)?;
        store.installations = installations;
        store.used_claim_proofs = used_claim_proofs;

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
    ) -> Result<RegisterSuccess, HostedProxyError> {
        let mut store = self.inner.lock().await;
        if store.installations.iter().any(|(id, installation)| {
            *id != installation_id && installation.update_secret == update_secret
        }) {
            return Err(HostedProxyError::OwnershipMismatch);
        }

        let current = if let Some(current) = store.installations.get(&installation_id) {
            current.clone()
        } else {
            return Err(HostedProxyError::InvalidUpdateSecret);
        };

        if current.update_secret != update_secret {
            return Err(HostedProxyError::InvalidUpdateSecret);
        }

        let now = Utc::now();
        let next_secret = new_update_secret();
        let updated = HostedInstallation {
            installation_id,
            tunnel_url: tunnel_url.clone(),
            update_secret: next_secret.clone(),
            created_at: current.created_at,
            updated_at: now,
        };

        let mut installations = store.installations.clone();
        installations.insert(installation_id, updated);
        let used_claim_proofs = store.used_claim_proofs.clone();
        store.persist_snapshot(&installations, &used_claim_proofs)?;
        store.installations = installations;

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
                used_claim_proofs: HashSet::new(),
            });
        }

        let content = std::fs::read_to_string(&path)?;
        let file: HostedProxyFile = serde_json::from_str(&content).map_err(invalid_data)?;
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

    fn persist_snapshot(
        &self,
        installations: &HashMap<u64, HostedInstallation>,
        used_claim_proofs: &HashSet<String>,
    ) -> Result<(), HostedProxyError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
        }

        let mut installations_list: Vec<_> = installations.values().cloned().collect();
        installations_list.sort_by_key(|installation| installation.installation_id);

        let mut proofs_list: Vec<_> = used_claim_proofs.iter().cloned().collect();
        proofs_list.sort();

        let content = serde_json::to_string_pretty(&HostedProxyFile {
            installations: installations_list,
            used_claim_proofs: proofs_list,
        })
        .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;

        std::fs::write(&self.path, format!("{content}\n"))
            .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
        Ok(())
    }
}

pub fn mint_claim_proof(
    installation_id: u64,
    proof_secret: &str,
) -> Result<String, HostedProxyError> {
    let now = Utc::now();
    let claims = ClaimProofClaims {
        installation_id,
        issued_at: now,
        expires_at: now + Duration::minutes(10),
        nonce: Uuid::new_v4().simple().to_string(),
    };
    encode_claim_proof(&claims, proof_secret)
}

fn verify_claim_proof(
    claim_proof: &str,
    proof_secret: &str,
) -> Result<VerifiedClaimProof, HostedProxyError> {
    let (payload_hex, signature_hex) = claim_proof
        .strip_prefix("cp_")
        .and_then(|value| value.split_once('.'))
        .ok_or(HostedProxyError::InvalidClaimProof)?;
    let payload = hex::decode(payload_hex).map_err(|_| HostedProxyError::InvalidClaimProof)?;
    let signature = hex::decode(signature_hex).map_err(|_| HostedProxyError::InvalidClaimProof)?;

    let mut mac =
        HmacSha256::new_from_slice(proof_secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| HostedProxyError::InvalidClaimProof)?;

    let claims: ClaimProofClaims =
        serde_json::from_slice(&payload).map_err(|_| HostedProxyError::InvalidClaimProof)?;
    if claims.expires_at < Utc::now() {
        return Err(HostedProxyError::ExpiredClaimProof);
    }

    Ok(VerifiedClaimProof {
        installation_id: claims.installation_id,
    })
}

fn encode_claim_proof(
    claims: &ClaimProofClaims,
    proof_secret: &str,
) -> Result<String, HostedProxyError> {
    let payload = serde_json::to_vec(claims)
        .map_err(|err| HostedProxyError::PersistenceFailure(err.to_string()))?;
    let mut mac =
        HmacSha256::new_from_slice(proof_secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();
    Ok(format!(
        "cp_{}.{}",
        hex::encode(payload),
        hex::encode(signature)
    ))
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
    use super::{
        canonicalize_tunnel_url, encode_claim_proof, mint_claim_proof, ClaimProofClaims,
        HostedProxyError, HostedProxyState,
    };
    use chrono::{Duration, Utc};
    use std::path::PathBuf;
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

    #[test]
    fn rejects_expired_claim_proof() {
        let claims = ClaimProofClaims {
            installation_id: 42,
            issued_at: Utc::now() - Duration::minutes(20),
            expires_at: Utc::now() - Duration::seconds(1),
            nonce: "expired".to_string(),
        };
        let proof = encode_claim_proof(&claims, "proof-secret").unwrap();
        let state = HostedProxyState::load(
            PathBuf::from("/tmp/nonexistent-hosted-proxy.json"),
            "proof-secret",
        )
        .unwrap();
        let error = tokio_test::block_on(async {
            state
                .register(42, "https://abc.trycloudflare.com", Some(proof), None)
                .await
                .unwrap_err()
        });
        assert_eq!(error.code(), "expired_claim_proof");
    }

    #[tokio::test]
    async fn initial_claim_consumes_proof_and_duplicate_claim_fails() {
        let tmp = TempDir::new().unwrap();
        let state =
            HostedProxyState::load(tmp.path().join("hosted_proxy.json"), "proof-secret").unwrap();
        let proof = mint_claim_proof(42, "proof-secret").unwrap();

        let created = state
            .register(
                42,
                "https://abc.trycloudflare.com",
                Some(proof.clone()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(created.status, "claimed");
        assert!(!created.rotated);

        let duplicate = state
            .register(42, "https://def.trycloudflare.com", Some(proof), None)
            .await
            .unwrap_err();
        assert_eq!(duplicate.code(), "already_claimed");
    }

    #[tokio::test]
    async fn invalid_claim_proof_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let state =
            HostedProxyState::load(tmp.path().join("hosted_proxy.json"), "proof-secret").unwrap();

        let error = state
            .register(
                42,
                "https://abc.trycloudflare.com",
                Some("cp_invalid".to_string()),
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(error.code(), "invalid_claim_proof");
    }

    #[tokio::test]
    async fn wrong_and_stale_update_secret_are_rejected() {
        let tmp = TempDir::new().unwrap();
        let state =
            HostedProxyState::load(tmp.path().join("hosted_proxy.json"), "proof-secret").unwrap();
        let proof = mint_claim_proof(42, "proof-secret").unwrap();

        let first = state
            .register(42, "https://abc.trycloudflare.com", Some(proof), None)
            .await
            .unwrap();

        let wrong_secret = state
            .register(
                42,
                "https://next.trycloudflare.com",
                None,
                Some("us_wrong_secret".to_string()),
            )
            .await
            .unwrap_err();
        assert_eq!(wrong_secret.code(), "invalid_update_secret");

        let second = state
            .register(
                42,
                "https://next.trycloudflare.com",
                None,
                Some(first.update_secret.clone()),
            )
            .await
            .unwrap();
        assert_eq!(second.status, "updated");
        assert!(second.rotated);

        let stale_secret = state
            .register(
                42,
                "https://third.trycloudflare.com",
                None,
                Some(first.update_secret),
            )
            .await
            .unwrap_err();
        assert_eq!(stale_secret.code(), "invalid_update_secret");
    }

    #[tokio::test]
    async fn state_persists_and_blocks_cross_installation_overwrite_after_restart() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hosted_proxy.json");
        let state = HostedProxyState::load(&path, "proof-secret").unwrap();
        let proof_a = mint_claim_proof(42, "proof-secret").unwrap();
        let proof_b = mint_claim_proof(99, "proof-secret").unwrap();

        let first = state
            .register(42, "https://abc.trycloudflare.com", Some(proof_a), None)
            .await
            .unwrap();

        state
            .register(99, "https://def.trycloudflare.com", Some(proof_b), None)
            .await
            .unwrap();

        let reloaded = HostedProxyState::load(&path, "proof-secret").unwrap();
        let installation = reloaded.get_installation(42).await.unwrap();
        assert_eq!(installation.tunnel_url, "https://abc.trycloudflare.com");

        let overwrite = reloaded
            .register(
                99,
                "https://evil.trycloudflare.com",
                None,
                Some(first.update_secret.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(overwrite.code(), "ownership_mismatch");

        let replayed_claim = reloaded
            .register(
                42,
                "https://abc.trycloudflare.com",
                Some(mint_claim_proof(42, "proof-secret").unwrap()),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(replayed_claim.code(), "already_claimed");

        let update_after_restart = reloaded
            .register(
                42,
                "https://next.trycloudflare.com",
                None,
                Some(first.update_secret),
            )
            .await
            .unwrap();
        assert_eq!(update_after_restart.status, "updated");
    }
}
