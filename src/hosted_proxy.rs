use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

const CLAIM_PROOF_TTL_MINUTES: i64 = 10;
const CLAIM_PROOF_PREFIX: &str = "cp_v1";
const UPDATE_SECRET_PREFIX: &str = "us";

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRequest {
    pub installation_id: u64,
    pub tunnel_url: String,
    #[serde(default)]
    pub claim_proof: Option<String>,
    #[serde(default)]
    pub update_secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetupClaimProofQuery {
    pub installation_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisterSuccess {
    pub status: &'static str,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClaimProofIssued {
    pub installation_id: u64,
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorDetail {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterError {
    pub code: &'static str,
    pub message: String,
    pub status_code: u16,
}

impl RegisterError {
    fn new(code: &'static str, message: impl Into<String>, status_code: u16) -> Self {
        Self {
            code,
            message: message.into(),
            status_code,
        }
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message.clone(),
            },
        }
    }
}

impl fmt::Display for RegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RegisterError {}

#[derive(Debug, Clone)]
pub struct HostedProxyStore {
    path: PathBuf,
    signing_secret: String,
    state: PersistedState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    registrations: HashMap<u64, InstallationRegistration>,
    #[serde(default)]
    claim_proofs: HashMap<String, ClaimProofRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallationRegistration {
    tunnel_url: String,
    update_secret_verifier: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClaimProofRecord {
    installation_id: u64,
    verifier: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedClaimProof {
    proof_id: String,
    verifier_input: String,
}

impl HostedProxyStore {
    pub fn load(
        path: impl AsRef<Path>,
        signing_secret: impl Into<String>,
    ) -> Result<Self, RegisterError> {
        let path = path.as_ref().to_path_buf();
        let signing_secret = signing_secret.into();

        let state = if path.exists() {
            let content = std::fs::read_to_string(&path).map_err(|e| {
                RegisterError::new(
                    "persistence_failure",
                    format!("Failed to read hosted proxy store: {e}"),
                    500,
                )
            })?;
            serde_json::from_str::<PersistedState>(&content).map_err(|e| {
                RegisterError::new(
                    "persistence_failure",
                    format!("Failed to parse hosted proxy store: {e}"),
                    500,
                )
            })?
        } else {
            PersistedState::default()
        };

        Ok(Self {
            path,
            signing_secret,
            state,
        })
    }

    pub fn issue_claim_proof_with_now(
        &mut self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<ClaimProofIssued, RegisterError> {
        if installation_id == 0 {
            return Err(RegisterError::new(
                "invalid_request",
                "installation_id must be a positive integer",
                400,
            ));
        }

        let proof_id = uuid::Uuid::new_v4().simple().to_string();
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let claim_proof = format!("{CLAIM_PROOF_PREFIX}.{proof_id}.{secret}");
        let expires_at = now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES);
        let verifier = secret_verifier(&self.signing_secret, &claim_proof)?;

        let mut next_state = self.state.clone();
        next_state.claim_proofs.insert(
            proof_id,
            ClaimProofRecord {
                installation_id,
                verifier,
                issued_at: now,
                expires_at,
                consumed_at: None,
            },
        );
        self.persist(next_state)?;

        Ok(ClaimProofIssued {
            installation_id,
            claim_proof,
            expires_at,
        })
    }

    pub fn register_with_now(
        &mut self,
        request: RegisterRequest,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        validate_auth_shape(&request)?;
        let normalized_tunnel_url = normalize_tunnel_url(&request.tunnel_url)?;

        if let Some(claim_proof) = request.claim_proof.as_deref() {
            return self.claim_installation(
                request.installation_id,
                normalized_tunnel_url,
                claim_proof,
                now,
            );
        }

        let update_secret = request.update_secret.as_deref().ok_or_else(|| {
            RegisterError::new(
                "invalid_request",
                "Exactly one of claim_proof or update_secret must be present",
                400,
            )
        })?;

        self.update_installation(
            request.installation_id,
            normalized_tunnel_url,
            update_secret,
            now,
        )
    }

    pub fn mint_claim_proof_for_test(
        &mut self,
        installation_id: u64,
        issued_at: DateTime<Utc>,
    ) -> String {
        self.issue_claim_proof_with_now(installation_id, issued_at)
            .expect("test proof issuance should succeed")
            .claim_proof
    }

    pub fn registration(
        &self,
        installation_id: u64,
    ) -> Option<(String, DateTime<Utc>, DateTime<Utc>)> {
        self.state
            .registrations
            .get(&installation_id)
            .map(|registration| {
                (
                    registration.tunnel_url.clone(),
                    registration.created_at,
                    registration.updated_at,
                )
            })
    }

    fn claim_installation(
        &mut self,
        installation_id: u64,
        normalized_tunnel_url: String,
        claim_proof: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        let parsed = parse_claim_proof(claim_proof)?;
        let record = self
            .state
            .claim_proofs
            .get(&parsed.proof_id)
            .ok_or_else(|| {
                RegisterError::new("invalid_claim_proof", "Claim proof is not recognized", 403)
            })?;

        let expected_verifier = secret_verifier(&self.signing_secret, &parsed.verifier_input)
            .map_err(|_| {
                RegisterError::new(
                    "invalid_claim_proof",
                    "Claim proof could not be verified",
                    403,
                )
            })?;
        if record.verifier != expected_verifier {
            return Err(RegisterError::new(
                "invalid_claim_proof",
                "Claim proof could not be verified",
                403,
            ));
        }

        if record.installation_id != installation_id {
            return Err(RegisterError::new(
                "ownership_mismatch",
                "Credential belongs to a different installation",
                403,
            ));
        }

        if record.consumed_at.is_some() {
            return Err(RegisterError::new(
                "claim_proof_already_used",
                "Claim proof has already been consumed",
                403,
            ));
        }

        if now > record.expires_at {
            return Err(RegisterError::new(
                "expired_claim_proof",
                "Claim proof has expired",
                403,
            ));
        }

        if self.state.registrations.contains_key(&installation_id) {
            return Err(RegisterError::new(
                "already_claimed",
                "Installation has already been claimed",
                409,
            ));
        }

        let update_secret = generate_update_secret();
        let update_secret_verifier = secret_verifier(&self.signing_secret, &update_secret)?;

        let mut next_state = self.state.clone();
        next_state.registrations.insert(
            installation_id,
            InstallationRegistration {
                tunnel_url: normalized_tunnel_url.clone(),
                update_secret_verifier,
                created_at: now,
                updated_at: now,
            },
        );
        if let Some(proof) = next_state.claim_proofs.get_mut(&parsed.proof_id) {
            proof.consumed_at = Some(now);
        }
        self.persist(next_state)?;

        Ok(RegisterSuccess {
            status: "claimed",
            installation_id,
            tunnel_url: normalized_tunnel_url,
            update_secret,
            rotated: false,
        })
    }

    fn update_installation(
        &mut self,
        installation_id: u64,
        normalized_tunnel_url: String,
        update_secret: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        let provided_verifier = secret_verifier(&self.signing_secret, update_secret)?;
        let registration = self
            .state
            .registrations
            .get(&installation_id)
            .ok_or_else(|| {
                RegisterError::new(
                    "invalid_update_secret",
                    "Update secret is not valid for this installation",
                    403,
                )
            })?;

        if registration.update_secret_verifier != provided_verifier {
            let owned_elsewhere =
                self.state
                    .registrations
                    .iter()
                    .any(|(candidate_id, candidate)| {
                        *candidate_id != installation_id
                            && candidate.update_secret_verifier == provided_verifier
                    });

            return Err(RegisterError::new(
                if owned_elsewhere {
                    "ownership_mismatch"
                } else {
                    "invalid_update_secret"
                },
                if owned_elsewhere {
                    "Credential belongs to a different installation"
                } else {
                    "Update secret is not valid for this installation"
                },
                403,
            ));
        }

        let replacement_secret = generate_update_secret();
        let replacement_verifier = secret_verifier(&self.signing_secret, &replacement_secret)
            .map_err(|_| {
                RegisterError::new(
                    "secret_rotation_failure",
                    "Failed to rotate update secret",
                    500,
                )
            })?;

        let mut next_state = self.state.clone();
        let existing = next_state
            .registrations
            .get_mut(&installation_id)
            .expect("registration exists");
        existing.tunnel_url = normalized_tunnel_url.clone();
        existing.update_secret_verifier = replacement_verifier;
        existing.updated_at = now;
        self.persist(next_state)?;

        Ok(RegisterSuccess {
            status: "updated",
            installation_id,
            tunnel_url: normalized_tunnel_url,
            update_secret: replacement_secret,
            rotated: true,
        })
    }

    fn persist(&mut self, next_state: PersistedState) -> Result<(), RegisterError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RegisterError::new(
                    "persistence_failure",
                    format!("Failed to create hosted proxy store directory: {e}"),
                    500,
                )
            })?;
        }

        let serialized = serde_json::to_vec_pretty(&next_state).map_err(|e| {
            RegisterError::new(
                "persistence_failure",
                format!("Failed to serialize hosted proxy store: {e}"),
                500,
            )
        })?;

        let tmp_path = self.path.with_extension("tmp");
        std::fs::write(&tmp_path, serialized).map_err(|e| {
            RegisterError::new(
                "persistence_failure",
                format!("Failed to write hosted proxy store: {e}"),
                500,
            )
        })?;
        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            RegisterError::new(
                "persistence_failure",
                format!("Failed to replace hosted proxy store: {e}"),
                500,
            )
        })?;
        self.state = next_state;
        Ok(())
    }
}

pub fn normalize_tunnel_url(raw: &str) -> Result<String, RegisterError> {
    let url = Url::parse(raw).map_err(|_| {
        RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must be a valid absolute HTTPS URL",
            400,
        )
    })?;

    if url.scheme() != "https" {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must use HTTPS",
            400,
        ));
    }

    if url.host_str().is_none() {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must include a host",
            400,
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must not include user credentials",
            400,
        ));
    }

    if url.query().is_some() || url.fragment().is_some() {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must not include query or fragment components",
            400,
        ));
    }

    if url.path() != "/" {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must be an origin without a path",
            400,
        ));
    }

    let host = url.host_str().expect("validated host exists");
    if is_unsafe_host(host) {
        return Err(RegisterError::new(
            "unsafe_tunnel_url",
            "Tunnel URL host points to a loopback or private-network destination",
            400,
        ));
    }

    let normalized_host = host.to_ascii_lowercase();
    let normalized = match url.port() {
        Some(port) => format!("https://{normalized_host}:{port}"),
        None => format!("https://{normalized_host}"),
    };

    Ok(normalized)
}

fn validate_auth_shape(request: &RegisterRequest) -> Result<(), RegisterError> {
    if request.installation_id == 0 {
        return Err(RegisterError::new(
            "invalid_request",
            "installation_id must be a positive integer",
            400,
        ));
    }

    match (&request.claim_proof, &request.update_secret) {
        (Some(_), Some(_)) | (None, None) => Err(RegisterError::new(
            "invalid_request",
            "Exactly one of claim_proof or update_secret must be present",
            400,
        )),
        _ => Ok(()),
    }
}

fn parse_claim_proof(claim_proof: &str) -> Result<ParsedClaimProof, RegisterError> {
    let parts: Vec<&str> = claim_proof.split('.').collect();
    if parts.len() != 3 || parts[0] != CLAIM_PROOF_PREFIX {
        return Err(RegisterError::new(
            "invalid_claim_proof",
            "Claim proof format is invalid",
            403,
        ));
    }

    Ok(ParsedClaimProof {
        proof_id: parts[1].to_string(),
        verifier_input: claim_proof.to_string(),
    })
}

fn generate_update_secret() -> String {
    format!("{UPDATE_SECRET_PREFIX}_{}", uuid::Uuid::new_v4().simple())
}

fn secret_verifier(signing_secret: &str, secret: &str) -> Result<String, RegisterError> {
    sign_with_secret(signing_secret, secret).map_err(|_| {
        RegisterError::new(
            "secret_rotation_failure",
            "Failed to derive secret verifier",
            500,
        )
    })
}

fn sign_with_secret(secret: &str, payload: &str) -> Result<String, hmac::digest::InvalidLength> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn is_unsafe_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return true;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => is_unsafe_ipv4(v4),
            IpAddr::V6(v6) => is_unsafe_ipv6(v6),
        };
    }

    false
}

fn is_unsafe_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || octets[0] == 0
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
        || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
        || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
}

fn is_unsafe_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.segments()[0] == 0x2001 && ip.segments()[1] == 0x0db8
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn request_with_claim(
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: String,
    ) -> RegisterRequest {
        RegisterRequest {
            installation_id,
            tunnel_url: tunnel_url.to_string(),
            claim_proof: Some(claim_proof),
            update_secret: None,
        }
    }

    fn request_with_secret(
        installation_id: u64,
        tunnel_url: &str,
        update_secret: String,
    ) -> RegisterRequest {
        RegisterRequest {
            installation_id,
            tunnel_url: tunnel_url.to_string(),
            claim_proof: None,
            update_secret: Some(update_secret),
        }
    }

    #[test]
    fn test_issue_claim_proof_persists_and_reports_expiry() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();

        let issued = store.issue_claim_proof_with_now(42, now).unwrap();

        assert_eq!(issued.installation_id, 42);
        assert!(issued.claim_proof.starts_with("cp_v1."));
        assert_eq!(issued.expires_at, now + Duration::minutes(10));

        let on_disk = std::fs::read_to_string(&store_path).unwrap();
        assert!(on_disk.contains("\"claim_proofs\""));
        assert!(on_disk.contains("\"installation_id\": 42"));
    }

    #[test]
    fn test_normalize_tunnel_url_accepts_https_origin_only() {
        let normalized = normalize_tunnel_url("https://Example.COM:8443").unwrap();
        assert_eq!(normalized, "https://example.com:8443");
    }

    #[test]
    fn test_normalize_tunnel_url_rejects_private_and_non_origin_hosts() {
        let private = normalize_tunnel_url("https://127.0.0.1");
        assert_eq!(private.unwrap_err().code, "unsafe_tunnel_url");

        let with_path = normalize_tunnel_url("https://example.com/hook");
        assert_eq!(with_path.unwrap_err().code, "invalid_tunnel_url");
    }

    #[test]
    fn test_claim_then_update_rotates_secret_and_normalizes_url() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_100, 0).unwrap();
        let claim_proof = store.mint_claim_proof_for_test(42, now);

        let claimed = store
            .register_with_now(
                request_with_claim(42, "https://Tunnel.EXAMPLE.com", claim_proof),
                now,
            )
            .unwrap();

        assert_eq!(claimed.status, "claimed");
        assert_eq!(claimed.tunnel_url, "https://tunnel.example.com");
        assert!(!claimed.rotated);

        let updated = store
            .register_with_now(
                request_with_secret(
                    42,
                    "https://next.example.com",
                    claimed.update_secret.clone(),
                ),
                now + Duration::minutes(1),
            )
            .unwrap();

        assert_eq!(updated.status, "updated");
        assert_eq!(updated.tunnel_url, "https://next.example.com");
        assert!(updated.rotated);
        assert_ne!(updated.update_secret, claimed.update_secret);

        let stale = store.register_with_now(
            request_with_secret(42, "https://another.example.com", claimed.update_secret),
            now + Duration::minutes(2),
        );
        assert_eq!(stale.unwrap_err().code, "invalid_update_secret");
    }

    #[test]
    fn test_claim_proof_replay_and_already_claimed_are_distinct() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_200, 0).unwrap();
        let claim_proof = store.mint_claim_proof_for_test(77, now);

        store
            .register_with_now(
                request_with_claim(77, "https://first.example.com", claim_proof.clone()),
                now,
            )
            .unwrap();

        let replay = store.register_with_now(
            request_with_claim(77, "https://second.example.com", claim_proof),
            now + Duration::seconds(10),
        );
        assert_eq!(replay.unwrap_err().code, "claim_proof_already_used");

        let fresh_proof = store.mint_claim_proof_for_test(77, now + Duration::seconds(20));
        let already_claimed = store.register_with_now(
            request_with_claim(77, "https://second.example.com", fresh_proof),
            now + Duration::seconds(20),
        );
        assert_eq!(already_claimed.unwrap_err().code, "already_claimed");
    }

    #[test]
    fn test_rejects_expired_and_mismatched_claim_proof() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_500, 0).unwrap();
        let expired_proof = store.mint_claim_proof_for_test(55, now - Duration::minutes(11));

        let expired = store.register_with_now(
            request_with_claim(55, "https://valid.example.com", expired_proof),
            now,
        );
        assert_eq!(expired.unwrap_err().code, "expired_claim_proof");

        let proof = store.mint_claim_proof_for_test(55, now);
        let mismatched = store.register_with_now(
            request_with_claim(56, "https://valid.example.com", proof),
            now,
        );
        assert_eq!(mismatched.unwrap_err().code, "ownership_mismatch");
    }

    #[test]
    fn test_wrong_update_secret_for_other_installation_is_ownership_mismatch() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_800, 0).unwrap();

        let first_proof = store.mint_claim_proof_for_test(1, now);
        let second_proof = store.mint_claim_proof_for_test(2, now);
        let first = store
            .register_with_now(
                request_with_claim(1, "https://one.example.com", first_proof),
                now,
            )
            .unwrap();
        store
            .register_with_now(
                request_with_claim(2, "https://two.example.com", second_proof),
                now,
            )
            .unwrap();

        let mismatch = store.register_with_now(
            request_with_secret(2, "https://swap.example.com", first.update_secret),
            now + Duration::seconds(5),
        );
        assert_eq!(mismatch.unwrap_err().code, "ownership_mismatch");
    }

    #[test]
    fn test_persistence_survives_reload() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp
            .path()
            .join("nested")
            .join("hosted_proxy_registrations.json");
        let now = DateTime::<Utc>::from_timestamp(1_700_001_000, 0).unwrap();

        let claimed_secret = {
            let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
            let claim_proof = store.mint_claim_proof_for_test(404, now);
            let claimed = store
                .register_with_now(
                    request_with_claim(404, "https://persist.example.com", claim_proof),
                    now,
                )
                .unwrap();
            claimed.update_secret
        };

        let mut reloaded = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let updated = reloaded
            .register_with_now(
                request_with_secret(404, "https://persist-2.example.com", claimed_secret),
                now + Duration::minutes(1),
            )
            .unwrap();

        assert_eq!(updated.status, "updated");
        let (tunnel_url, _, updated_at) = reloaded.registration(404).unwrap();
        assert_eq!(tunnel_url, "https://persist-2.example.com");
        assert_eq!(updated_at, now + Duration::minutes(1));
    }
}
