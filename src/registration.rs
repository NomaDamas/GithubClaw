use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;
type SecretFactory = dyn Fn() -> Result<String, RegistrationError> + Send + Sync + 'static;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimProofClaims {
    pub proof_id: String,
    pub installation_id: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationRegistration {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterResponse {
    Claimed {
        registration: InstallationRegistration,
        update_secret: String,
    },
    Updated {
        registration: InstallationRegistration,
        update_secret: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("request must include exactly one of claim_proof or update_secret")]
    InvalidRequest,
    #[error("claim proof is malformed")]
    InvalidClaimProof,
    #[error("claim proof is expired")]
    ExpiredClaimProof,
    #[error("claim proof has already been used")]
    ClaimProofAlreadyUsed,
    #[error("update secret is invalid")]
    InvalidUpdateSecret,
    #[error("credential belongs to a different installation")]
    OwnershipMismatch,
    #[error("installation has already been claimed")]
    AlreadyClaimed,
    #[error("failed to rotate update secret")]
    SecretRotationFailure,
    #[error("tunnel_url must be a valid HTTPS origin without userinfo, path, query, or fragment")]
    InvalidTunnelUrl,
    #[error("tunnel_url must not resolve to localhost, loopback, or private network addresses")]
    UnsafeTunnelUrl,
    #[error("failed to persist registration store: {0}")]
    Persistence(String),
}

impl RegistrationError {
    pub fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::InvalidRequest | Self::InvalidTunnelUrl | Self::UnsafeTunnelUrl => {
                axum::http::StatusCode::BAD_REQUEST
            }
            Self::InvalidClaimProof
            | Self::ExpiredClaimProof
            | Self::ClaimProofAlreadyUsed
            | Self::InvalidUpdateSecret
            | Self::OwnershipMismatch => axum::http::StatusCode::FORBIDDEN,
            Self::AlreadyClaimed => axum::http::StatusCode::CONFLICT,
            Self::Persistence(_) | Self::SecretRotationFailure => {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidClaimProof => "invalid_claim_proof",
            Self::ExpiredClaimProof => "expired_claim_proof",
            Self::ClaimProofAlreadyUsed => "claim_proof_already_used",
            Self::InvalidUpdateSecret => "invalid_update_secret",
            Self::OwnershipMismatch => "ownership_mismatch",
            Self::AlreadyClaimed => "already_claimed",
            Self::SecretRotationFailure => "secret_rotation_failure",
            Self::InvalidTunnelUrl => "invalid_tunnel_url",
            Self::UnsafeTunnelUrl => "unsafe_tunnel_url",
            Self::Persistence(_) => "persistence_failure",
        }
    }
}

pub struct RegistrationState {
    secret: String,
    inner: Mutex<RegistrationStore>,
    secret_factory: Arc<SecretFactory>,
}

#[derive(Debug)]
struct RegistrationStore {
    path: PathBuf,
    installations: HashMap<u64, StoredInstallation>,
    used_claim_proof_ids: HashSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredInstallation {
    installation_id: u64,
    tunnel_url: String,
    update_secret_hash: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistrationFile {
    #[serde(default)]
    installations: Vec<StoredInstallation>,
    #[serde(default)]
    used_claim_proof_ids: Vec<String>,
}

impl RegistrationState {
    pub fn load(
        path: impl AsRef<Path>,
        secret: impl Into<String>,
    ) -> Result<Self, RegistrationError> {
        Self::load_with_factory(path, secret, || Ok(generate_update_secret()))
    }

    fn load_with_factory<F>(
        path: impl AsRef<Path>,
        secret: impl Into<String>,
        secret_factory: F,
    ) -> Result<Self, RegistrationError>
    where
        F: Fn() -> Result<String, RegistrationError> + Send + Sync + 'static,
    {
        let store = RegistrationStore::load(path.as_ref())
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        Ok(Self {
            secret: secret.into(),
            inner: Mutex::new(store),
            secret_factory: Arc::new(secret_factory),
        })
    }

    pub fn new_for_tests(
        path: impl AsRef<Path>,
        secret: impl Into<String>,
    ) -> Result<Self, RegistrationError> {
        Self::load(path, secret)
    }

    #[cfg(test)]
    pub fn new_for_tests_with_secrets<I, S>(
        path: impl AsRef<Path>,
        secret: impl Into<String>,
        secrets: I,
    ) -> Result<Self, RegistrationError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let remaining = std::sync::Mutex::new(
            secrets
                .into_iter()
                .map(Into::into)
                .collect::<std::collections::VecDeque<_>>(),
        );
        Self::load_with_factory(path, secret, move || {
            remaining
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(RegistrationError::SecretRotationFailure)
        })
    }

    pub async fn register_with_claim_proof(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: &str,
    ) -> Result<RegisterResponse, RegistrationError> {
        let normalized_url = normalize_tunnel_url(tunnel_url)?;
        let claims = verify_claim_proof(claim_proof, &self.secret)?;
        if claims.installation_id != installation_id {
            return Err(RegistrationError::OwnershipMismatch);
        }

        let mut store = self.inner.lock().await;

        if store.used_claim_proof_ids.contains(&claims.proof_id) {
            return Err(RegistrationError::ClaimProofAlreadyUsed);
        }
        if store.installations.contains_key(&installation_id) {
            return Err(RegistrationError::AlreadyClaimed);
        }
        let next_secret = (self.secret_factory)()?;

        let now = Utc::now();
        let stored = StoredInstallation {
            installation_id,
            tunnel_url: normalized_url.clone(),
            update_secret_hash: hash_secret(&next_secret),
            created_at: now,
            updated_at: now,
        };

        let mut next_installations = store.installations.clone();
        next_installations.insert(installation_id, stored.clone());
        let mut next_used_claims = store.used_claim_proof_ids.clone();
        next_used_claims.insert(claims.proof_id);
        store.persist_replacement(next_installations, next_used_claims)?;

        Ok(RegisterResponse::Claimed {
            registration: stored.into_public(),
            update_secret: next_secret,
        })
    }

    pub async fn register_with_update_secret(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        update_secret: &str,
    ) -> Result<RegisterResponse, RegistrationError> {
        let normalized_url = normalize_tunnel_url(tunnel_url)?;
        let current_hash = hash_secret(update_secret);
        let mut store = self.inner.lock().await;

        if let Some(existing) = store.installations.get(&installation_id) {
            if existing.update_secret_hash != current_hash {
                if store
                    .installations
                    .values()
                    .any(|registration| registration.update_secret_hash == current_hash)
                {
                    return Err(RegistrationError::OwnershipMismatch);
                }
                return Err(RegistrationError::InvalidUpdateSecret);
            }
        } else if store
            .installations
            .values()
            .any(|registration| registration.update_secret_hash == current_hash)
        {
            return Err(RegistrationError::OwnershipMismatch);
        } else {
            return Err(RegistrationError::InvalidUpdateSecret);
        }
        let next_secret = (self.secret_factory)()?;

        let existing = store
            .installations
            .get(&installation_id)
            .cloned()
            .expect("installation exists after validation");
        let updated = StoredInstallation {
            installation_id,
            tunnel_url: normalized_url.clone(),
            update_secret_hash: hash_secret(&next_secret),
            created_at: existing.created_at,
            updated_at: Utc::now(),
        };

        let mut next_installations = store.installations.clone();
        next_installations.insert(installation_id, updated.clone());
        let next_used_claims = store.used_claim_proof_ids.clone();
        store.persist_replacement(next_installations, next_used_claims)?;

        Ok(RegisterResponse::Updated {
            registration: updated.into_public(),
            update_secret: next_secret,
        })
    }

    pub async fn get(&self, installation_id: u64) -> Option<InstallationRegistration> {
        self.inner
            .lock()
            .await
            .installations
            .get(&installation_id)
            .cloned()
            .map(StoredInstallation::into_public)
    }
}

impl RegistrationStore {
    fn load(path: &Path) -> std::io::Result<Self> {
        let path = path.to_path_buf();
        if !path.exists() {
            return Ok(Self {
                path,
                installations: HashMap::new(),
                used_claim_proof_ids: HashSet::new(),
            });
        }

        let contents = std::fs::read_to_string(&path)?;
        let file: RegistrationFile = serde_json::from_str(&contents).unwrap_or(RegistrationFile {
            installations: Vec::new(),
            used_claim_proof_ids: Vec::new(),
        });
        let installations = file
            .installations
            .into_iter()
            .map(|registration| (registration.installation_id, registration))
            .collect();
        let used_claim_proof_ids = file.used_claim_proof_ids.into_iter().collect();

        Ok(Self {
            path,
            installations,
            used_claim_proof_ids,
        })
    }

    fn persist_replacement(
        &mut self,
        installations: HashMap<u64, StoredInstallation>,
        used_claim_proof_ids: HashSet<String>,
    ) -> Result<(), RegistrationError> {
        persist_registration_store(&self.path, &installations, &used_claim_proof_ids)
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        self.installations = installations;
        self.used_claim_proof_ids = used_claim_proof_ids;
        Ok(())
    }
}

impl StoredInstallation {
    fn into_public(self) -> InstallationRegistration {
        InstallationRegistration {
            installation_id: self.installation_id,
            tunnel_url: self.tunnel_url,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

fn persist_registration_store(
    path: &Path,
    installations: &HashMap<u64, StoredInstallation>,
    used_claim_proof_ids: &HashSet<String>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut installations = installations.values().cloned().collect::<Vec<_>>();
    installations.sort_by_key(|registration| registration.installation_id);
    let mut used_claim_proof_ids = used_claim_proof_ids.iter().cloned().collect::<Vec<_>>();
    used_claim_proof_ids.sort();

    let file = RegistrationFile {
        installations,
        used_claim_proof_ids,
    };
    let contents = serde_json::to_string_pretty(&file)?;
    std::fs::write(path, format!("{contents}\n"))
}

pub fn mint_claim_proof(claims: &ClaimProofClaims, secret: &str) -> String {
    let payload = serde_json::to_vec(claims).expect("claim proof claims serialize");
    let payload_hex = hex::encode(&payload);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(&payload);
    let signature_hex = hex::encode(mac.finalize().into_bytes());
    format!("{payload_hex}.{signature_hex}")
}

pub fn verify_claim_proof(
    proof: &str,
    secret: &str,
) -> Result<ClaimProofClaims, RegistrationError> {
    let (payload_hex, signature_hex) = proof
        .split_once('.')
        .ok_or(RegistrationError::InvalidClaimProof)?;
    let payload = hex::decode(payload_hex).map_err(|_| RegistrationError::InvalidClaimProof)?;
    let signature = hex::decode(signature_hex).map_err(|_| RegistrationError::InvalidClaimProof)?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| RegistrationError::InvalidClaimProof)?;

    let claims: ClaimProofClaims =
        serde_json::from_slice(&payload).map_err(|_| RegistrationError::InvalidClaimProof)?;
    if claims.expires_at < Utc::now() {
        return Err(RegistrationError::ExpiredClaimProof);
    }

    Ok(claims)
}

pub fn normalize_tunnel_url(raw: &str) -> Result<String, RegistrationError> {
    let trimmed = raw.trim();
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| RegistrationError::InvalidTunnelUrl)?;

    if parsed.scheme() != "https" {
        return Err(RegistrationError::InvalidTunnelUrl);
    }
    if parsed.host_str().is_none() {
        return Err(RegistrationError::InvalidTunnelUrl);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RegistrationError::InvalidTunnelUrl);
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(RegistrationError::InvalidTunnelUrl);
    }

    let host = parsed.host_str().unwrap();
    if host.eq_ignore_ascii_case("localhost") {
        return Err(RegistrationError::UnsafeTunnelUrl);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            return Err(RegistrationError::UnsafeTunnelUrl);
        }
    }

    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!("https://{}{port}", host.to_ascii_lowercase()))
}

fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_multicast()
                || ipv4.is_broadcast()
                || ipv4.is_unspecified()
                || ipv4.octets()[0] == 0
        }
        IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || ipv6.is_multicast()
                || ipv6.is_unique_local()
                || ipv6.is_unicast_link_local()
                || ipv6 == Ipv6Addr::LOCALHOST
        }
    }
}

fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

fn generate_update_secret() -> String {
    format!("us_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use tempfile::TempDir;

    fn claim_proof(secret: &str, installation_id: u64, proof_id: &str) -> String {
        mint_claim_proof(
            &ClaimProofClaims {
                proof_id: proof_id.to_string(),
                installation_id,
                issued_at: Utc::now(),
                expires_at: Utc::now() + Duration::minutes(10),
            },
            secret,
        )
    }

    #[test]
    fn claim_proof_roundtrip_succeeds() {
        let proof = claim_proof("secret", 42, "proof-1");
        let verified = verify_claim_proof(&proof, "secret").unwrap();
        assert_eq!(verified.installation_id, 42);
        assert_eq!(verified.proof_id, "proof-1");
    }

    #[test]
    fn expired_claim_proof_is_rejected() {
        let proof = mint_claim_proof(
            &ClaimProofClaims {
                proof_id: "proof-1".to_string(),
                installation_id: 42,
                issued_at: Utc::now() - Duration::minutes(11),
                expires_at: Utc::now() - Duration::seconds(1),
            },
            "secret",
        );

        let err = verify_claim_proof(&proof, "secret").unwrap_err();
        assert_eq!(err, RegistrationError::ExpiredClaimProof);
    }

    #[test]
    fn normalize_tunnel_url_rejects_private_targets() {
        let err = normalize_tunnel_url("https://127.0.0.1").unwrap_err();
        assert_eq!(err, RegistrationError::UnsafeTunnelUrl);
    }

    #[tokio::test]
    async fn registration_store_persists_claim_and_rotation_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests_with_secrets(
            &path,
            "secret",
            ["us_first", "us_second"],
        )
        .unwrap();

        let claimed = state
            .register_with_claim_proof(
                7,
                "https://abc.trycloudflare.com",
                &claim_proof("secret", 7, "proof-1"),
            )
            .await
            .unwrap();
        assert_eq!(
            claimed,
            RegisterResponse::Claimed {
                registration: InstallationRegistration {
                    installation_id: 7,
                    tunnel_url: "https://abc.trycloudflare.com".to_string(),
                    created_at: state.get(7).await.unwrap().created_at,
                    updated_at: state.get(7).await.unwrap().updated_at,
                },
                update_secret: "us_first".to_string(),
            }
        );

        let updated = state
            .register_with_update_secret(7, "https://next.trycloudflare.com", "us_first")
            .await
            .unwrap();
        match updated {
            RegisterResponse::Updated {
                registration,
                update_secret,
            } => {
                assert_eq!(registration.tunnel_url, "https://next.trycloudflare.com");
                assert_eq!(update_secret, "us_second");
            }
            other => panic!("expected updated response, got {other:?}"),
        }

        let reloaded = RegistrationState::new_for_tests_with_secrets(
            &path,
            "secret",
            ["us_third", "us_fourth"],
        )
        .unwrap();
        let registration = reloaded.get(7).await.unwrap();
        assert_eq!(registration.tunnel_url, "https://next.trycloudflare.com");

        let stale_secret_error = reloaded
            .register_with_update_secret(7, "https://ignored.trycloudflare.com", "us_first")
            .await
            .unwrap_err();
        assert_eq!(stale_secret_error, RegistrationError::InvalidUpdateSecret);

        let replay_error = reloaded
            .register_with_claim_proof(
                7,
                "https://ignored.trycloudflare.com",
                &claim_proof("secret", 7, "proof-1"),
            )
            .await
            .unwrap_err();
        assert_eq!(replay_error, RegistrationError::ClaimProofAlreadyUsed);
    }

    #[tokio::test]
    async fn duplicate_and_cross_installation_writes_are_rejected_deterministically() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests_with_secrets(
            &path,
            "secret",
            ["us_first", "us_second", "us_third", "us_fourth"],
        )
        .unwrap();

        let first_secret = match state
            .register_with_claim_proof(
                1,
                "https://one.trycloudflare.com",
                &claim_proof("secret", 1, "proof-1"),
            )
            .await
            .unwrap()
        {
            RegisterResponse::Claimed { update_secret, .. } => update_secret,
            other => panic!("expected claimed response, got {other:?}"),
        };
        let second_secret = match state
            .register_with_claim_proof(
                2,
                "https://two.trycloudflare.com",
                &claim_proof("secret", 2, "proof-2"),
            )
            .await
            .unwrap()
        {
            RegisterResponse::Claimed { update_secret, .. } => update_secret,
            other => panic!("expected claimed response, got {other:?}"),
        };

        let already_claimed = state
            .register_with_claim_proof(
                1,
                "https://other.trycloudflare.com",
                &claim_proof("secret", 1, "proof-3"),
            )
            .await
            .unwrap_err();
        assert_eq!(already_claimed, RegistrationError::AlreadyClaimed);

        let ownership_mismatch = state
            .register_with_update_secret(2, "https://blocked.trycloudflare.com", &first_secret)
            .await
            .unwrap_err();
        assert_eq!(ownership_mismatch, RegistrationError::OwnershipMismatch);

        let updated_secret = match state
            .register_with_update_secret(1, "https://one-b.trycloudflare.com", &first_secret)
            .await
            .unwrap()
        {
            RegisterResponse::Updated { update_secret, .. } => update_secret,
            other => panic!("expected updated response, got {other:?}"),
        };
        assert_eq!(updated_secret, "us_third");

        let stale_retry = state
            .register_with_update_secret(1, "https://one-c.trycloudflare.com", &first_secret)
            .await
            .unwrap_err();
        assert_eq!(stale_retry, RegistrationError::InvalidUpdateSecret);

        let second_installation_still_works = state
            .register_with_update_secret(2, "https://two-b.trycloudflare.com", &second_secret)
            .await
            .unwrap();
        match second_installation_still_works {
            RegisterResponse::Updated { update_secret, .. } => {
                assert_eq!(update_secret, "us_fourth");
            }
            other => panic!("expected updated response, got {other:?}"),
        }
    }
}
