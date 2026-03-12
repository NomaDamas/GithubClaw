use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

pub struct HostedProxyService {
    state_path: PathBuf,
    state: Mutex<HostedProxyState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HostedProxyState {
    pub claims: HashMap<String, ClaimProofRecord>,
    pub installations: HashMap<u64, InstallationRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimProofRecord {
    pub installation_id: u64,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationRecord {
    pub tunnel_url: String,
    pub update_secret: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupCallbackResponse {
    pub status: String,
    pub installation_id: u64,
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRequest {
    pub installation_id: u64,
    pub tunnel_url: String,
    #[serde(default)]
    pub claim_proof: Option<String>,
    #[serde(default)]
    pub update_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub status: String,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedProxyError {
    InvalidRequest(&'static str),
    InvalidTunnelUrl(&'static str),
    UnsafeTunnelUrl(&'static str),
    InvalidClaimProof,
    ExpiredClaimProof,
    ClaimProofAlreadyUsed,
    InvalidUpdateSecret,
    OwnershipMismatch,
    AlreadyClaimed,
    PersistenceFailure(String),
}

impl HostedProxyService {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let state_path = path.as_ref().to_path_buf();
        let state = if state_path.exists() {
            let raw = std::fs::read_to_string(&state_path)
                .map_err(|e| format!("Failed to read hosted proxy state: {e}"))?;
            serde_json::from_str(&raw)
                .map_err(|e| format!("Failed to parse hosted proxy state: {e}"))?
        } else {
            HostedProxyState::default()
        };

        Ok(Self {
            state_path,
            state: Mutex::new(state),
        })
    }

    pub async fn issue_claim_proof_at(
        &self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<SetupCallbackResponse, HostedProxyError> {
        if installation_id == 0 {
            return Err(HostedProxyError::InvalidRequest(
                "installation_id must be positive",
            ));
        }

        let mut state = self.state.lock().await;
        if state.installations.contains_key(&installation_id) {
            return Err(HostedProxyError::AlreadyClaimed);
        }

        if let Some((claim_proof, claim)) = state.claims.iter().find(|(_, claim)| {
            claim.installation_id == installation_id
                && claim.consumed_at.is_none()
                && claim.expires_at > now
        }) {
            return Ok(SetupCallbackResponse {
                status: "issued".to_string(),
                installation_id,
                claim_proof: claim_proof.clone(),
                expires_at: claim.expires_at,
            });
        }

        state
            .claims
            .retain(|_, claim| claim.expires_at > now || claim.consumed_at.is_some());

        let claim_proof = format!("cp_{}", Uuid::new_v4().simple());
        let expires_at = now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES);
        state.claims.insert(
            claim_proof.clone(),
            ClaimProofRecord {
                installation_id,
                expires_at,
                consumed_at: None,
            },
        );
        self.persist(&state)
            .await
            .map_err(HostedProxyError::PersistenceFailure)?;

        Ok(SetupCallbackResponse {
            status: "issued".to_string(),
            installation_id,
            claim_proof,
            expires_at,
        })
    }

    pub async fn issue_claim_proof(
        &self,
        installation_id: u64,
    ) -> Result<SetupCallbackResponse, HostedProxyError> {
        self.issue_claim_proof_at(installation_id, Utc::now()).await
    }

    pub async fn register_at(
        &self,
        request: RegisterRequest,
        now: DateTime<Utc>,
    ) -> Result<RegisterResponse, HostedProxyError> {
        validate_request_shape(&request)?;
        let normalized_tunnel_url = normalize_tunnel_url(&request.tunnel_url)?;

        let mut state = self.state.lock().await;

        if let Some(claim_proof) = request.claim_proof.as_deref() {
            let Some(claim) = state.claims.get(claim_proof) else {
                if state.installations.contains_key(&request.installation_id) {
                    return Err(HostedProxyError::AlreadyClaimed);
                }
                return Err(HostedProxyError::InvalidClaimProof);
            };

            if claim.installation_id != request.installation_id {
                return Err(HostedProxyError::OwnershipMismatch);
            }

            if claim.consumed_at.is_some() {
                return Err(HostedProxyError::ClaimProofAlreadyUsed);
            }

            if claim.expires_at <= now {
                return Err(HostedProxyError::ExpiredClaimProof);
            }

            if state.installations.contains_key(&request.installation_id) {
                return Err(HostedProxyError::AlreadyClaimed);
            }

            let update_secret = new_update_secret();
            state.installations.insert(
                request.installation_id,
                InstallationRecord {
                    tunnel_url: normalized_tunnel_url.clone(),
                    update_secret: update_secret.clone(),
                    created_at: now,
                    updated_at: now,
                },
            );

            if let Some(claim) = state.claims.get_mut(claim_proof) {
                claim.consumed_at = Some(now);
            }

            self.persist(&state)
                .await
                .map_err(HostedProxyError::PersistenceFailure)?;

            return Ok(RegisterResponse {
                status: "claimed".to_string(),
                installation_id: request.installation_id,
                tunnel_url: normalized_tunnel_url,
                update_secret,
                rotated: false,
            });
        }

        let update_secret =
            request
                .update_secret
                .as_deref()
                .ok_or(HostedProxyError::InvalidRequest(
                    "update_secret is required for update",
                ))?;

        let Some(current) = state.installations.get(&request.installation_id) else {
            if state
                .installations
                .values()
                .any(|record| record.update_secret == update_secret)
            {
                return Err(HostedProxyError::OwnershipMismatch);
            }
            return Err(HostedProxyError::InvalidUpdateSecret);
        };

        if current.update_secret != update_secret {
            if state
                .installations
                .values()
                .any(|record| record.update_secret == update_secret)
            {
                return Err(HostedProxyError::OwnershipMismatch);
            }
            return Err(HostedProxyError::InvalidUpdateSecret);
        }

        let replacement_secret = new_update_secret();
        let record = state
            .installations
            .get_mut(&request.installation_id)
            .expect("installation checked above");
        record.tunnel_url = normalized_tunnel_url.clone();
        record.update_secret = replacement_secret.clone();
        record.updated_at = now;

        self.persist(&state)
            .await
            .map_err(HostedProxyError::PersistenceFailure)?;

        Ok(RegisterResponse {
            status: "updated".to_string(),
            installation_id: request.installation_id,
            tunnel_url: normalized_tunnel_url,
            update_secret: replacement_secret,
            rotated: true,
        })
    }

    pub async fn register(
        &self,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, HostedProxyError> {
        self.register_at(request, Utc::now()).await
    }

    async fn persist(&self, state: &HostedProxyState) -> Result<(), String> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create hosted proxy state dir: {e}"))?;
        }

        let tmp_path = self.state_path.with_extension("tmp");
        let raw = serde_json::to_vec_pretty(state)
            .map_err(|e| format!("Failed to encode hosted proxy state: {e}"))?;
        std::fs::write(&tmp_path, raw)
            .map_err(|e| format!("Failed to write hosted proxy state: {e}"))?;
        std::fs::rename(&tmp_path, &self.state_path)
            .map_err(|e| format!("Failed to persist hosted proxy state: {e}"))?;
        Ok(())
    }
}

impl Display for HostedProxyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid_request: {message}"),
            Self::InvalidTunnelUrl(message) => write!(f, "invalid_tunnel_url: {message}"),
            Self::UnsafeTunnelUrl(message) => write!(f, "unsafe_tunnel_url: {message}"),
            Self::InvalidClaimProof => write!(f, "invalid_claim_proof: claim proof is invalid"),
            Self::ExpiredClaimProof => write!(f, "expired_claim_proof: claim proof has expired"),
            Self::ClaimProofAlreadyUsed => {
                write!(f, "claim_proof_already_used: claim proof already consumed")
            }
            Self::InvalidUpdateSecret => {
                write!(f, "invalid_update_secret: update secret is invalid")
            }
            Self::OwnershipMismatch => write!(
                f,
                "ownership_mismatch: credential is scoped to a different installation"
            ),
            Self::AlreadyClaimed => write!(f, "already_claimed: installation already claimed"),
            Self::PersistenceFailure(message) => write!(f, "persistence_failure: {message}"),
        }
    }
}

impl std::error::Error for HostedProxyError {}

impl HostedProxyError {
    pub fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::InvalidRequest(_) | Self::InvalidTunnelUrl(_) | Self::UnsafeTunnelUrl(_) => {
                axum::http::StatusCode::BAD_REQUEST
            }
            Self::InvalidClaimProof
            | Self::ExpiredClaimProof
            | Self::ClaimProofAlreadyUsed
            | Self::InvalidUpdateSecret
            | Self::OwnershipMismatch => axum::http::StatusCode::FORBIDDEN,
            Self::AlreadyClaimed => axum::http::StatusCode::CONFLICT,
            Self::PersistenceFailure(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidTunnelUrl(_) => "invalid_tunnel_url",
            Self::UnsafeTunnelUrl(_) => "unsafe_tunnel_url",
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
            Self::InvalidRequest(message)
            | Self::InvalidTunnelUrl(message)
            | Self::UnsafeTunnelUrl(message) => (*message).to_string(),
            Self::InvalidClaimProof => "Claim proof is invalid.".to_string(),
            Self::ExpiredClaimProof => "Claim proof has expired.".to_string(),
            Self::ClaimProofAlreadyUsed => "Claim proof was already used.".to_string(),
            Self::InvalidUpdateSecret => "Update secret is invalid.".to_string(),
            Self::OwnershipMismatch => {
                "Credential belongs to a different installation.".to_string()
            }
            Self::AlreadyClaimed => "Installation is already claimed.".to_string(),
            Self::PersistenceFailure(message) => message.clone(),
        }
    }
}

fn validate_request_shape(request: &RegisterRequest) -> Result<(), HostedProxyError> {
    if request.installation_id == 0 {
        return Err(HostedProxyError::InvalidRequest(
            "installation_id must be positive",
        ));
    }

    let has_claim = request.claim_proof.is_some();
    let has_update = request.update_secret.is_some();
    match (has_claim, has_update) {
        (true, true) => Err(HostedProxyError::InvalidRequest(
            "claim_proof and update_secret are mutually exclusive",
        )),
        (false, false) => Err(HostedProxyError::InvalidRequest(
            "exactly one of claim_proof or update_secret is required",
        )),
        _ => Ok(()),
    }
}

fn normalize_tunnel_url(raw: &str) -> Result<String, HostedProxyError> {
    let parsed = reqwest::Url::parse(raw).map_err(|_| {
        HostedProxyError::InvalidTunnelUrl("tunnel_url must be a valid absolute URL")
    })?;

    if parsed.scheme() != "https" {
        return Err(HostedProxyError::InvalidTunnelUrl(
            "tunnel_url must use https",
        ));
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl(
            "tunnel_url must not contain embedded credentials",
        ));
    }

    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(HostedProxyError::InvalidTunnelUrl(
            "tunnel_url must not include query or fragment",
        ));
    }

    if parsed.path() != "/" {
        return Err(HostedProxyError::InvalidTunnelUrl(
            "tunnel_url must be an origin without a path",
        ));
    }

    let host = parsed.host_str().ok_or(HostedProxyError::InvalidTunnelUrl(
        "tunnel_url host is required",
    ))?;

    if is_unsafe_host(host) {
        return Err(HostedProxyError::UnsafeTunnelUrl(
            "tunnel_url host is not allowed",
        ));
    }

    let mut normalized = format!("https://{host}");
    if let Some(port) = parsed.port() {
        normalized.push(':');
        normalized.push_str(&port.to_string());
    }

    Ok(normalized)
}

fn is_unsafe_host(host: &str) -> bool {
    let normalized = host.trim_matches('.').to_ascii_lowercase();
    if normalized == "localhost" || normalized.ends_with(".localhost") {
        return true;
    }

    if let Ok(ip) = normalized.parse::<IpAddr>() {
        return is_unsafe_ip(ip);
    }

    false
}

fn is_unsafe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_unsafe_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_unsafe_ipv6(ipv6),
    }
}

fn is_unsafe_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || octets[0] == 0
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

fn is_unsafe_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
}

fn new_update_secret() -> String {
    format!("us_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn service(tmp: &TempDir) -> HostedProxyService {
        HostedProxyService::load(tmp.path().join("hosted_proxy_state.json")).unwrap()
    }

    #[tokio::test]
    async fn setup_callback_issues_claim_proof_for_installation() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();

        let response = service.issue_claim_proof_at(123, now).await.unwrap();

        assert_eq!(response.status, "issued");
        assert_eq!(response.installation_id, 123);
        assert!(response.claim_proof.starts_with("cp_"));
        assert_eq!(
            response.expires_at,
            now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES)
        );
    }

    #[tokio::test]
    async fn initial_claim_succeeds_and_replay_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof_at(123, now).await.unwrap();

        let claimed = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof.clone()),
                    update_secret: None,
                },
                now,
            )
            .await
            .unwrap();

        assert_eq!(claimed.status, "claimed");
        assert!(!claimed.update_secret.is_empty());

        let replay = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://next456.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::minutes(1),
            )
            .await
            .unwrap_err();

        assert_eq!(replay, HostedProxyError::ClaimProofAlreadyUsed);
    }

    #[tokio::test]
    async fn expired_claim_proof_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof_at(123, now).await.unwrap();

        let result = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES + 1),
            )
            .await
            .unwrap_err();

        assert_eq!(result, HostedProxyError::ExpiredClaimProof);
    }

    #[tokio::test]
    async fn failed_validation_does_not_consume_claim_proof() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof_at(123, now).await.unwrap();

        let first_attempt = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "http://localhost:8080".to_string(),
                    claim_proof: Some(issued.claim_proof.clone()),
                    update_secret: None,
                },
                now,
            )
            .await
            .unwrap_err();

        assert!(matches!(
            first_attempt,
            HostedProxyError::InvalidTunnelUrl(_) | HostedProxyError::UnsafeTunnelUrl(_)
        ));

        let second_attempt = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::minutes(1),
            )
            .await
            .unwrap();

        assert_eq!(second_attempt.status, "claimed");
    }

    #[tokio::test]
    async fn update_rotates_secret_and_rejects_stale_secret() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof_at(123, now).await.unwrap();
        let claimed = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now,
            )
            .await
            .unwrap();

        let updated = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://next456.trycloudflare.com".to_string(),
                    claim_proof: None,
                    update_secret: Some(claimed.update_secret.clone()),
                },
                now + Duration::minutes(1),
            )
            .await
            .unwrap();

        assert_eq!(updated.status, "updated");
        assert_ne!(updated.update_secret, claimed.update_secret);
        assert!(updated.rotated);

        let stale = service
            .register_at(
                RegisterRequest {
                    installation_id: 123,
                    tunnel_url: "https://third789.trycloudflare.com".to_string(),
                    claim_proof: None,
                    update_secret: Some(claimed.update_secret),
                },
                now + Duration::minutes(2),
            )
            .await
            .unwrap_err();

        assert_eq!(stale, HostedProxyError::InvalidUpdateSecret);
    }
}
