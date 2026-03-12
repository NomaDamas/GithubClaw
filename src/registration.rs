use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};

use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Mutex;

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
    pub updated_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationClaims {
    pub installation_id: u64,
    pub issued_at: chrono::DateTime<Utc>,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationOutcome {
    Created,
    Updated,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("registration token is malformed")]
    InvalidToken,
    #[error("registration token signature is invalid")]
    InvalidTokenSignature,
    #[error("registration token has expired")]
    ExpiredToken,
    #[error("installation_id does not match registration token")]
    InstallationMismatch,
    #[error("tunnel_url must be a valid HTTPS origin without userinfo, path, query, or fragment")]
    InvalidTunnelUrl,
    #[error("tunnel_url must not resolve to localhost, loopback, or private network addresses")]
    UnsafeTunnelUrl,
    #[error("failed to persist registration store: {0}")]
    Persistence(String),
}

#[derive(Debug)]
struct RegistrationStore {
    path: PathBuf,
    mappings: HashMap<u64, InstallationRegistration>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistrationFile {
    #[serde(default)]
    installations: Vec<InstallationRegistration>,
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
        registration_token: &str,
    ) -> Result<RegistrationOutcome, RegistrationError> {
        let claims = verify_registration_token(registration_token, &self.secret)?;
        if claims.installation_id != installation_id {
            return Err(RegistrationError::InstallationMismatch);
        }

        let normalized_url = normalize_tunnel_url(tunnel_url)?;
        let mut store = self.inner.lock().await;
        let outcome = if store.mappings.contains_key(&installation_id) {
            RegistrationOutcome::Updated
        } else {
            RegistrationOutcome::Created
        };

        store.mappings.insert(
            installation_id,
            InstallationRegistration {
                installation_id,
                tunnel_url: normalized_url,
                updated_at: Utc::now(),
            },
        );
        store
            .persist()
            .map_err(|err| RegistrationError::Persistence(err.to_string()))?;

        Ok(outcome)
    }

    pub async fn get(&self, installation_id: u64) -> Option<InstallationRegistration> {
        let store = self.inner.lock().await;
        store.mappings.get(&installation_id).cloned()
    }
}

impl RegistrationStore {
    fn load(path: &Path) -> std::io::Result<Self> {
        let path = path.to_path_buf();
        if !path.exists() {
            return Ok(Self {
                path,
                mappings: HashMap::new(),
            });
        }

        let contents = std::fs::read_to_string(&path)?;
        let file: RegistrationFile = serde_json::from_str(&contents).unwrap_or(RegistrationFile {
            installations: Vec::new(),
        });
        let mappings = file
            .installations
            .into_iter()
            .map(|registration| (registration.installation_id, registration))
            .collect();

        Ok(Self { path, mappings })
    }

    fn persist(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut installations: Vec<_> = self.mappings.values().cloned().collect();
        installations.sort_by_key(|registration| registration.installation_id);
        let file = RegistrationFile { installations };
        let contents = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.path, format!("{contents}\n"))
    }
}

pub fn mint_registration_token(claims: &RegistrationClaims, secret: &str) -> String {
    let payload = serde_json::to_vec(claims).expect("registration claims serialize");
    let payload_hex = hex::encode(&payload);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(&payload);
    let signature_hex = hex::encode(mac.finalize().into_bytes());
    format!("{payload_hex}.{signature_hex}")
}

pub fn verify_registration_token(
    token: &str,
    secret: &str,
) -> Result<RegistrationClaims, RegistrationError> {
    let (payload_hex, signature_hex) = token
        .split_once('.')
        .ok_or(RegistrationError::InvalidToken)?;
    let payload = hex::decode(payload_hex).map_err(|_| RegistrationError::InvalidToken)?;
    let signature = hex::decode(signature_hex).map_err(|_| RegistrationError::InvalidToken)?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC key");
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| RegistrationError::InvalidTokenSignature)?;

    let claims: RegistrationClaims =
        serde_json::from_slice(&payload).map_err(|_| RegistrationError::InvalidToken)?;
    if claims.expires_at < Utc::now() {
        return Err(RegistrationError::ExpiredToken);
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use tempfile::TempDir;

    #[test]
    fn token_roundtrip_succeeds() {
        let claims = RegistrationClaims {
            installation_id: 42,
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(10),
        };

        let token = mint_registration_token(&claims, "secret");
        let verified = verify_registration_token(&token, "secret").unwrap();
        assert_eq!(verified.installation_id, 42);
    }

    #[test]
    fn expired_token_is_rejected() {
        let claims = RegistrationClaims {
            installation_id: 42,
            issued_at: Utc::now() - Duration::minutes(10),
            expires_at: Utc::now() - Duration::seconds(1),
        };

        let token = mint_registration_token(&claims, "secret");
        let err = verify_registration_token(&token, "secret").unwrap_err();
        assert_eq!(err, RegistrationError::ExpiredToken);
    }

    #[test]
    fn normalize_tunnel_url_rejects_private_targets() {
        let err = normalize_tunnel_url("https://127.0.0.1").unwrap_err();
        assert_eq!(err, RegistrationError::UnsafeTunnelUrl);
    }

    #[tokio::test]
    async fn registration_store_persists_to_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("registration").join("installations.json");
        let state = RegistrationState::new_for_tests(&path, "secret").unwrap();
        let claims = RegistrationClaims {
            installation_id: 7,
            issued_at: Utc::now(),
            expires_at: Utc::now() + Duration::minutes(5),
        };
        let token = mint_registration_token(&claims, "secret");

        let outcome = state
            .register(7, "https://abc.trycloudflare.com", &token)
            .await
            .unwrap();
        assert_eq!(outcome, RegistrationOutcome::Created);

        let reloaded = RegistrationState::new_for_tests(&path, "secret").unwrap();
        let registration = reloaded.get(7).await.unwrap();
        assert_eq!(registration.tunnel_url, "https://abc.trycloudflare.com");
    }
}
