use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug)]
pub struct RegistrationState {
    secret: String,
    inner: Mutex<RegistrationStore>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallationRegistration {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimProofClaims {
    pub installation_id: u64,
    pub proof_id: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthCredential {
    ClaimProof(String),
    UpdateSecret(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Claimed,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSuccess {
    pub outcome: RegistrationOutcome,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("claim proof is malformed or invalid")]
    InvalidClaimProof,
    #[error("claim proof has expired")]
    ExpiredClaimProof,
    #[error("claim proof has already been used")]
    ClaimProofAlreadyUsed,
    #[error("update secret is invalid")]
    InvalidUpdateSecret,
    #[error("credential belongs to a different installation")]
    OwnershipMismatch,
    #[error("installation has already been claimed")]
    AlreadyClaimed,
    #[error("tunnel_url must be a valid HTTPS origin without userinfo, path, query, or fragment")]
    InvalidTunnelUrl,
    #[error("tunnel_url must not resolve to localhost, loopback, or private network addresses")]
    UnsafeTunnelUrl,
    #[error("failed to persist registration store: {0}")]
    Persistence(String),
    #[error("failed to rotate update secret")]
    SecretRotationFailure,
}

#[derive(Debug)]
struct RegistrationStore {
    path: PathBuf,
    installations: HashMap<u64, StoredInstallation>,
    used_claim_proof_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredInstallation {
    installation_id: u64,
    tunnel_url: String,
    update_secret_verifier: String,
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
        let store = RegistrationStore::load(path.as_ref())
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        Ok(Self {
            secret: secret.into(),
            inner: Mutex::new(store),
        })
    }

    pub fn new_for_tests(
        path: impl AsRef<Path>,
        secret: impl Into<String>,
    ) -> Result<Self, RegistrationError> {
        Self::load(path, secret)
    }

    pub async fn register(
        &self,
        installation_id: u64,
        tunnel_url: &str,
        auth: AuthCredential,
    ) -> Result<RegisterSuccess, RegistrationError> {
        let normalized_tunnel_url = normalize_tunnel_url(tunnel_url)?;
        let mut store = self.inner.lock().await;

        match auth {
            AuthCredential::ClaimProof(claim_proof) => {
                let claims = verify_claim_proof(&claim_proof, &self.secret)?;
                if claims.installation_id != installation_id {
                    return Err(RegistrationError::OwnershipMismatch);
                }
                if store.used_claim_proof_ids.contains(&claims.proof_id) {
                    return Err(RegistrationError::ClaimProofAlreadyUsed);
                }
                if store.installations.contains_key(&installation_id) {
                    return Err(RegistrationError::AlreadyClaimed);
                }

                let update_secret = generate_update_secret();
                let verifier = update_secret_verifier(&update_secret, &self.secret);
                let now = Utc::now();
                let registration = StoredInstallation {
                    installation_id,
                    tunnel_url: normalized_tunnel_url.clone(),
                    update_secret_verifier: verifier,
                    created_at: now,
                    updated_at: now,
                };

                let mut next_installations = store.installations.clone();
                next_installations.insert(installation_id, registration);
                let mut next_used_proofs = store.used_claim_proof_ids.clone();
                next_used_proofs.insert(claims.proof_id);
                store.persist_snapshot(&next_installations, &next_used_proofs)?;
                store.installations = next_installations;
                store.used_claim_proof_ids = next_used_proofs;

                Ok(RegisterSuccess {
                    outcome: RegistrationOutcome::Claimed,
                    installation_id,
                    tunnel_url: normalized_tunnel_url,
                    update_secret,
                })
            }
            AuthCredential::UpdateSecret(update_secret) => {
                let provided_verifier = update_secret_verifier(&update_secret, &self.secret);
                if store
                    .owner_of_update_secret(&provided_verifier, installation_id)
                    .is_some()
                {
                    return Err(RegistrationError::OwnershipMismatch);
                }

                let existing = match store.installations.get(&installation_id).cloned() {
                    Some(existing) => existing,
                    None => return Err(RegistrationError::InvalidUpdateSecret),
                };

                if existing.update_secret_verifier != provided_verifier {
                    return Err(RegistrationError::InvalidUpdateSecret);
                }

                let replacement_secret = generate_update_secret();
                let replacement_verifier =
                    update_secret_verifier(&replacement_secret, &self.secret);
                let updated = StoredInstallation {
                    installation_id,
                    tunnel_url: normalized_tunnel_url.clone(),
                    update_secret_verifier: replacement_verifier,
                    created_at: existing.created_at,
                    updated_at: Utc::now(),
                };

                let mut next_installations = store.installations.clone();
                next_installations.insert(installation_id, updated);
                let next_used_proofs = store.used_claim_proof_ids.clone();
                store.persist_snapshot(&next_installations, &next_used_proofs)?;
                store.installations = next_installations;

                Ok(RegisterSuccess {
                    outcome: RegistrationOutcome::Updated,
                    installation_id,
                    tunnel_url: normalized_tunnel_url,
                    update_secret: replacement_secret,
                })
            }
        }
    }

    pub async fn get(&self, installation_id: u64) -> Option<InstallationRegistration> {
        let store = self.inner.lock().await;
        store
            .installations
            .get(&installation_id)
            .map(|registration| InstallationRegistration {
                installation_id: registration.installation_id,
                tunnel_url: registration.tunnel_url.clone(),
                created_at: registration.created_at,
                updated_at: registration.updated_at,
            })
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

    fn persist_snapshot(
        &self,
        installations: &HashMap<u64, StoredInstallation>,
        used_claim_proof_ids: &HashSet<String>,
    ) -> Result<(), RegistrationError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        }

        let mut stored_installations: Vec<_> = installations.values().cloned().collect();
        stored_installations.sort_by_key(|registration| registration.installation_id);

        let mut stored_proof_ids: Vec<_> = used_claim_proof_ids.iter().cloned().collect();
        stored_proof_ids.sort();

        let payload = RegistrationFile {
            installations: stored_installations,
            used_claim_proof_ids: stored_proof_ids,
        };
        let contents = serde_json::to_string_pretty(&payload)
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        let temp_path = self.path.with_extension("tmp");
        std::fs::write(&temp_path, format!("{contents}\n"))
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        std::fs::rename(&temp_path, &self.path)
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;
        Ok(())
    }

    fn owner_of_update_secret(
        &self,
        verifier: &str,
        requested_installation_id: u64,
    ) -> Option<u64> {
        self.installations
            .iter()
            .find(|(installation_id, registration)| {
                **installation_id != requested_installation_id
                    && registration.update_secret_verifier == verifier
            })
            .map(|(installation_id, _)| *installation_id)
    }
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
    claim_proof: &str,
    secret: &str,
) -> Result<ClaimProofClaims, RegistrationError> {
    let (payload_hex, signature_hex) = claim_proof
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

pub fn load_or_create_registration_secret(path: &Path) -> Result<String, String> {
    if path.exists() {
        return std::fs::read_to_string(path)
            .map(|value| value.trim().to_string())
            .map_err(|err| {
                format!(
                    "Failed to read registration secret from {}: {}",
                    path.display(),
                    err
                )
            });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create {}: {}", parent.display(), err))?;
    }

    let mut buffer = [0u8; 32];
    let mut random = std::fs::File::open("/dev/urandom")
        .map_err(|err| format!("Failed to open /dev/urandom: {}", err))?;
    random
        .read_exact(&mut buffer)
        .map_err(|err| format!("Failed to read random bytes: {}", err))?;
    let secret = hex::encode(buffer);
    std::fs::write(path, &secret).map_err(|err| {
        format!(
            "Failed to write registration secret to {}: {}",
            path.display(),
            err
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(secret)
}

fn generate_update_secret() -> String {
    format!("us_{}", Uuid::new_v4().simple())
}

fn update_secret_verifier(secret: &str, registration_secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(registration_secret.as_bytes());
    hasher.update(b":");
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use tempfile::TempDir;

    const TEST_REGISTRATION_SECRET: &str = "registration-secret";

    fn claim_proof_for(installation_id: u64, proof_id: &str) -> String {
        mint_claim_proof(
            &ClaimProofClaims {
                installation_id,
                proof_id: proof_id.to_string(),
                issued_at: Utc::now(),
                expires_at: Utc::now() + Duration::minutes(10),
            },
            TEST_REGISTRATION_SECRET,
        )
    }

    #[test]
    fn claim_proof_roundtrip_succeeds() {
        let claims = ClaimProofClaims {
            installation_id: 42,
            proof_id: "proof-1".to_string(),
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(10),
        };

        let token = mint_claim_proof(&claims, TEST_REGISTRATION_SECRET);
        let verified = verify_claim_proof(&token, TEST_REGISTRATION_SECRET).unwrap();
        assert_eq!(verified.installation_id, 42);
        assert_eq!(verified.proof_id, "proof-1");
    }

    #[test]
    fn expired_claim_proof_is_rejected() {
        let claims = ClaimProofClaims {
            installation_id: 42,
            proof_id: "proof-1".to_string(),
            issued_at: Utc::now() - Duration::minutes(11),
            expires_at: Utc::now() - Duration::seconds(1),
        };

        let token = mint_claim_proof(&claims, TEST_REGISTRATION_SECRET);
        let err = verify_claim_proof(&token, TEST_REGISTRATION_SECRET).unwrap_err();
        assert_eq!(err, RegistrationError::ExpiredClaimProof);
    }

    #[test]
    fn normalize_tunnel_url_rejects_private_targets() {
        let err = normalize_tunnel_url("https://127.0.0.1").unwrap_err();
        assert_eq!(err, RegistrationError::UnsafeTunnelUrl);
    }

    #[tokio::test]
    async fn first_claim_persists_and_returns_update_secret() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();

        let result = state
            .register(
                7,
                "https://ABC.trycloudflare.com",
                AuthCredential::ClaimProof(claim_proof_for(7, "proof-1")),
            )
            .await
            .unwrap();

        assert_eq!(result.outcome, RegistrationOutcome::Claimed);
        assert_eq!(result.tunnel_url, "https://abc.trycloudflare.com");
        assert!(result.update_secret.starts_with("us_"));

        let stored = state.get(7).await.unwrap();
        assert_eq!(stored.tunnel_url, "https://abc.trycloudflare.com");
    }

    #[tokio::test]
    async fn claim_proof_replay_is_rejected_after_success() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();
        let proof = claim_proof_for(7, "proof-1");

        state
            .register(
                7,
                "https://abc.trycloudflare.com",
                AuthCredential::ClaimProof(proof.clone()),
            )
            .await
            .unwrap();

        let err = state
            .register(
                7,
                "https://next.trycloudflare.com",
                AuthCredential::ClaimProof(proof),
            )
            .await
            .unwrap_err();
        assert_eq!(err, RegistrationError::ClaimProofAlreadyUsed);
    }

    #[tokio::test]
    async fn already_claimed_installation_requires_update_secret() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();

        state
            .register(
                7,
                "https://abc.trycloudflare.com",
                AuthCredential::ClaimProof(claim_proof_for(7, "proof-1")),
            )
            .await
            .unwrap();

        let err = state
            .register(
                7,
                "https://next.trycloudflare.com",
                AuthCredential::ClaimProof(claim_proof_for(7, "proof-2")),
            )
            .await
            .unwrap_err();
        assert_eq!(err, RegistrationError::AlreadyClaimed);
    }

    #[tokio::test]
    async fn update_rotates_secret_and_invalidates_previous_value() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();

        let initial = state
            .register(
                7,
                "https://abc.trycloudflare.com",
                AuthCredential::ClaimProof(claim_proof_for(7, "proof-1")),
            )
            .await
            .unwrap();

        let updated = state
            .register(
                7,
                "https://next.trycloudflare.com",
                AuthCredential::UpdateSecret(initial.update_secret.clone()),
            )
            .await
            .unwrap();

        assert_eq!(updated.outcome, RegistrationOutcome::Updated);
        assert_ne!(updated.update_secret, initial.update_secret);
        assert_eq!(updated.tunnel_url, "https://next.trycloudflare.com");

        let err = state
            .register(
                7,
                "https://again.trycloudflare.com",
                AuthCredential::UpdateSecret(initial.update_secret),
            )
            .await
            .unwrap_err();
        assert_eq!(err, RegistrationError::InvalidUpdateSecret);
    }

    #[tokio::test]
    async fn current_secret_cannot_update_another_installation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();

        let first = state
            .register(
                7,
                "https://first.trycloudflare.com",
                AuthCredential::ClaimProof(claim_proof_for(7, "proof-1")),
            )
            .await
            .unwrap();
        state
            .register(
                8,
                "https://second.trycloudflare.com",
                AuthCredential::ClaimProof(claim_proof_for(8, "proof-2")),
            )
            .await
            .unwrap();

        let err = state
            .register(
                8,
                "https://attacker.trycloudflare.com",
                AuthCredential::UpdateSecret(first.update_secret),
            )
            .await
            .unwrap_err();
        assert_eq!(err, RegistrationError::OwnershipMismatch);
    }

    #[tokio::test]
    async fn failed_validation_does_not_consume_claim_proof_or_rotate_secret() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();
        let proof = claim_proof_for(7, "proof-1");

        let err = state
            .register(
                7,
                "http://127.0.0.1:3000",
                AuthCredential::ClaimProof(proof.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(err, RegistrationError::InvalidTunnelUrl);

        let claimed = state
            .register(
                7,
                "https://abc.trycloudflare.com",
                AuthCredential::ClaimProof(proof),
            )
            .await
            .unwrap();

        let err = state
            .register(
                7,
                "https://bad.trycloudflare.com/path",
                AuthCredential::UpdateSecret(claimed.update_secret.clone()),
            )
            .await
            .unwrap_err();
        assert_eq!(err, RegistrationError::InvalidTunnelUrl);

        let updated = state
            .register(
                7,
                "https://next.trycloudflare.com",
                AuthCredential::UpdateSecret(claimed.update_secret),
            )
            .await
            .unwrap();
        assert_eq!(updated.outcome, RegistrationOutcome::Updated);
    }

    #[tokio::test]
    async fn registration_store_persists_replay_and_current_secret_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();
        let proof = claim_proof_for(7, "proof-1");

        let initial = state
            .register(
                7,
                "https://abc.trycloudflare.com",
                AuthCredential::ClaimProof(proof.clone()),
            )
            .await
            .unwrap();
        let rotated = state
            .register(
                7,
                "https://next.trycloudflare.com",
                AuthCredential::UpdateSecret(initial.update_secret),
            )
            .await
            .unwrap();

        let reloaded = RegistrationState::new_for_tests(&path, TEST_REGISTRATION_SECRET).unwrap();
        let replay_err = reloaded
            .register(
                7,
                "https://third.trycloudflare.com",
                AuthCredential::ClaimProof(proof),
            )
            .await
            .unwrap_err();
        assert_eq!(replay_err, RegistrationError::ClaimProofAlreadyUsed);

        let after_reload = reloaded
            .register(
                7,
                "https://third.trycloudflare.com",
                AuthCredential::UpdateSecret(rotated.update_secret),
            )
            .await
            .unwrap();
        assert_eq!(after_reload.outcome, RegistrationOutcome::Updated);
    }
}
