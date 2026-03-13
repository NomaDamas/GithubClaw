use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RegisterSuccess {
    pub status: &'static str,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
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
    used_claim_proofs: HashMap<String, UsedClaimProof>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallationRegistration {
    tunnel_url: String,
    update_secret_verifier: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsedClaimProof {
    installation_id: u64,
    used_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedClaimProof {
    installation_id: u64,
    issued_at: DateTime<Utc>,
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
        &self,
        installation_id: u64,
        issued_at: DateTime<Utc>,
    ) -> String {
        mint_claim_proof(&self.signing_secret, installation_id, issued_at)
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
        let proof_digest = sha256_hex(claim_proof);
        if self.state.used_claim_proofs.contains_key(&proof_digest) {
            return Err(RegisterError::new(
                "claim_proof_already_used",
                "Claim proof has already been consumed",
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

        let parsed = parse_and_verify_claim_proof(&self.signing_secret, claim_proof)?;
        if parsed.installation_id != installation_id {
            return Err(RegisterError::new(
                "ownership_mismatch",
                "Credential belongs to a different installation",
                403,
            ));
        }

        if now.signed_duration_since(parsed.issued_at) > Duration::minutes(CLAIM_PROOF_TTL_MINUTES)
        {
            return Err(RegisterError::new(
                "expired_claim_proof",
                "Claim proof has expired",
                403,
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
        next_state.used_claim_proofs.insert(
            proof_digest,
            UsedClaimProof {
                installation_id,
                used_at: now,
            },
        );
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
        apply_registration_update(
            &mut next_state,
            installation_id,
            normalized_tunnel_url.clone(),
            replacement_verifier,
            now,
        )?;
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

        let tmp_path = self.path.with_extension("json.tmp");
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
    normalize_tunnel_url_with_resolver(raw, resolve_host_ips)
}

fn normalize_tunnel_url_with_resolver<F>(raw: &str, resolver: F) -> Result<String, RegisterError>
where
    F: Fn(&str) -> Result<Vec<IpAddr>, RegisterError>,
{
    let url = Url::parse(raw).map_err(|_| {
        RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must be a valid absolute URL",
            400,
        )
    })?;

    if url.scheme() != "https" {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must use https",
            400,
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must not include embedded credentials",
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

    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL must be an origin without path, query, or fragment",
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

    let resolved_ips = resolver(host)?;
    if resolved_ips.into_iter().any(is_unsafe_ip) {
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

fn apply_registration_update(
    state: &mut PersistedState,
    installation_id: u64,
    normalized_tunnel_url: String,
    replacement_verifier: String,
    now: DateTime<Utc>,
) -> Result<(), RegisterError> {
    let existing = state
        .registrations
        .get_mut(&installation_id)
        .ok_or_else(|| {
            RegisterError::new("internal_error", "Registration state inconsistency", 500)
        })?;
    existing.tunnel_url = normalized_tunnel_url;
    existing.update_secret_verifier = replacement_verifier;
    existing.updated_at = now;
    Ok(())
}

fn mint_claim_proof(secret: &str, installation_id: u64, issued_at: DateTime<Utc>) -> String {
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let issued_at_ts = issued_at.timestamp();
    let payload = format!("{installation_id}.{issued_at_ts}.{nonce}");
    let signature = sign_with_secret(secret, &payload).expect("HMAC accepts any key size");
    format!("{CLAIM_PROOF_PREFIX}.{payload}.{signature}")
}

fn parse_and_verify_claim_proof(
    secret: &str,
    claim_proof: &str,
) -> Result<ParsedClaimProof, RegisterError> {
    let parts: Vec<&str> = claim_proof.split('.').collect();
    if parts.len() != 5 || parts[0] != CLAIM_PROOF_PREFIX {
        return Err(RegisterError::new(
            "invalid_claim_proof",
            "Claim proof could not be verified",
            403,
        ));
    }

    let installation_id = parts[1].parse::<u64>().map_err(|_| {
        RegisterError::new(
            "invalid_claim_proof",
            "Claim proof installation id is invalid",
            403,
        )
    })?;
    let issued_at_ts = parts[2].parse::<i64>().map_err(|_| {
        RegisterError::new(
            "invalid_claim_proof",
            "Claim proof timestamp is invalid",
            403,
        )
    })?;
    let nonce = parts[3];
    let signature = hex::decode(parts[4]).map_err(|_| {
        RegisterError::new(
            "invalid_claim_proof",
            "Claim proof could not be verified",
            403,
        )
    })?;
    let payload = format!("{installation_id}.{issued_at_ts}.{nonce}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| {
        RegisterError::new(
            "invalid_claim_proof",
            "Claim proof could not be verified",
            403,
        )
    })?;
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature).map_err(|_| {
        RegisterError::new(
            "invalid_claim_proof",
            "Claim proof could not be verified",
            403,
        )
    })?;

    let issued_at = DateTime::<Utc>::from_timestamp(issued_at_ts, 0).ok_or_else(|| {
        RegisterError::new(
            "invalid_claim_proof",
            "Claim proof timestamp is invalid",
            403,
        )
    })?;

    Ok(ParsedClaimProof {
        installation_id,
        issued_at,
    })
}

fn generate_update_secret() -> String {
    format!("{UPDATE_SECRET_PREFIX}_{}", uuid::Uuid::new_v4().simple())
}

fn secret_verifier(secret: &str, value: &str) -> Result<String, RegisterError> {
    sign_with_secret(secret, value).map_err(|_| {
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

fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn resolve_host_ips(host: &str) -> Result<Vec<IpAddr>, RegisterError> {
    let socket_addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host, 443)).map_err(|_| {
        RegisterError::new(
            "invalid_tunnel_url",
            "Tunnel URL host could not be resolved",
            400,
        )
    })?;

    Ok(socket_addrs.map(|addr| addr.ip()).collect())
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

fn is_unsafe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_unsafe_ipv4(v4),
        IpAddr::V6(v6) => is_unsafe_ipv6(v6),
    }
}

fn is_unsafe_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || octets[0] == 0
        || octets[0] >= 224
}

fn is_unsafe_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
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
    fn test_normalize_tunnel_url_rejects_hostname_resolving_to_unsafe_ip() {
        for resolved_ip in [
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 1, 9)),
        ] {
            let resolved = normalize_tunnel_url_with_resolver("https://public.example.com", |_| {
                Ok(vec![resolved_ip])
            });

            assert_eq!(resolved.unwrap_err().code, "unsafe_tunnel_url");
        }
    }

    #[test]
    fn test_normalize_tunnel_url_rejects_unresolvable_hostname() {
        let resolved = normalize_tunnel_url_with_resolver("https://public.example.com", |_| {
            Err(RegisterError::new(
                "invalid_tunnel_url",
                "Tunnel URL host could not be resolved",
                400,
            ))
        });

        let error = resolved.unwrap_err();
        assert_eq!(error.code, "invalid_tunnel_url");
        assert_eq!(error.status_code, 400);
    }

    #[test]
    fn test_claim_then_update_rotates_secret_and_normalizes_url() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let claim_proof = store.mint_claim_proof_for_test(42, now);

        let claimed = store
            .register_with_now(
                request_with_claim(42, "https://EXAMPLE.com", claim_proof),
                now,
            )
            .unwrap();

        assert_eq!(claimed.status, "claimed");
        assert_eq!(claimed.tunnel_url, "https://example.com");
        assert!(!claimed.rotated);

        let updated = store
            .register_with_now(
                request_with_secret(42, "https://www.example.com", claimed.update_secret.clone()),
                now + Duration::minutes(1),
            )
            .unwrap();

        assert_eq!(updated.status, "updated");
        assert_eq!(updated.tunnel_url, "https://www.example.com");
        assert!(updated.rotated);
        assert_ne!(updated.update_secret, claimed.update_secret);

        let stale = store.register_with_now(
            request_with_secret(42, "https://example.com", claimed.update_secret),
            now + Duration::minutes(2),
        );
        assert_eq!(stale.unwrap_err().code, "invalid_update_secret");
    }

    #[test]
    fn test_claim_proof_replay_and_already_claimed_are_distinct() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_100, 0).unwrap();
        let claim_proof = store.mint_claim_proof_for_test(77, now);

        store
            .register_with_now(
                request_with_claim(77, "https://example.com", claim_proof.clone()),
                now,
            )
            .unwrap();

        let replay = store.register_with_now(
            request_with_claim(77, "https://www.example.com", claim_proof),
            now + Duration::seconds(10),
        );
        assert_eq!(replay.unwrap_err().code, "claim_proof_already_used");

        let fresh_proof = store.mint_claim_proof_for_test(77, now + Duration::seconds(20));
        let already_claimed = store.register_with_now(
            request_with_claim(77, "https://www.example.com", fresh_proof),
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
            request_with_claim(55, "https://example.com", expired_proof),
            now,
        );
        assert_eq!(expired.unwrap_err().code, "expired_claim_proof");

        let proof = store.mint_claim_proof_for_test(55, now);
        let mismatched =
            store.register_with_now(request_with_claim(56, "https://example.com", proof), now);
        assert_eq!(mismatched.unwrap_err().code, "ownership_mismatch");
    }

    #[test]
    fn test_rejects_tampered_claim_proof_signature() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_600, 0).unwrap();
        let claim_proof = store.mint_claim_proof_for_test(55, now);
        let mut parts: Vec<String> = claim_proof.split('.').map(ToString::to_string).collect();
        parts[4].replace_range(..2, "00");
        let tampered = parts.join(".");

        let error = parse_and_verify_claim_proof("test-secret", &tampered).unwrap_err();
        assert_eq!(error.code, "invalid_claim_proof");
        assert_eq!(error.status_code, 403);
    }

    #[test]
    fn test_wrong_update_secret_for_other_installation_is_ownership_mismatch() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let now = DateTime::<Utc>::from_timestamp(1_700_000_700, 0).unwrap();

        let first = store
            .register_with_now(
                request_with_claim(
                    1,
                    "https://example.com",
                    store.mint_claim_proof_for_test(1, now),
                ),
                now,
            )
            .unwrap();

        store
            .register_with_now(
                request_with_claim(
                    2,
                    "https://www.example.com",
                    store.mint_claim_proof_for_test(2, now),
                ),
                now,
            )
            .unwrap();

        let mismatch = store.register_with_now(
            request_with_secret(2, "https://example.com", first.update_secret),
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
                    request_with_claim(404, "https://example.com", claim_proof),
                    now,
                )
                .unwrap();
            claimed.update_secret
        };

        let mut reloaded = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let updated = reloaded
            .register_with_now(
                request_with_secret(404, "https://www.example.com", claimed_secret),
                now + Duration::minutes(1),
            )
            .unwrap();

        assert_eq!(updated.status, "updated");
        let (tunnel_url, _, updated_at) = reloaded.registration(404).unwrap();
        assert_eq!(tunnel_url, "https://www.example.com");
        assert_eq!(updated_at, now + Duration::minutes(1));
    }

    #[test]
    fn test_reused_claim_proof_is_still_rejected_after_reload() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let now = DateTime::<Utc>::from_timestamp(1_700_001_100, 0).unwrap();
        let claim_proof = {
            let store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
            store.mint_claim_proof_for_test(505, now)
        };

        {
            let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
            store
                .register_with_now(
                    request_with_claim(505, "https://example.com", claim_proof.clone()),
                    now,
                )
                .unwrap();
        }

        let mut reloaded = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let replay = reloaded.register_with_now(
            request_with_claim(505, "https://www.example.com", claim_proof),
            now + Duration::seconds(10),
        );
        assert_eq!(replay.unwrap_err().code, "claim_proof_already_used");
    }

    #[test]
    fn test_stale_update_secret_is_still_rejected_after_reload() {
        let tmp = TempDir::new().unwrap();
        let store_path = tmp.path().join("hosted_proxy_registrations.json");
        let now = DateTime::<Utc>::from_timestamp(1_700_001_200, 0).unwrap();

        let stale_secret = {
            let mut store = HostedProxyStore::load(&store_path, "test-secret").unwrap();
            let claimed = store
                .register_with_now(
                    request_with_claim(
                        606,
                        "https://example.com",
                        store.mint_claim_proof_for_test(606, now),
                    ),
                    now,
                )
                .unwrap();

            store
                .register_with_now(
                    request_with_secret(
                        606,
                        "https://www.example.com",
                        claimed.update_secret.clone(),
                    ),
                    now + Duration::minutes(1),
                )
                .unwrap();

            claimed.update_secret
        };

        let mut reloaded = HostedProxyStore::load(&store_path, "test-secret").unwrap();
        let replay = reloaded.register_with_now(
            request_with_secret(606, "https://example.com", stale_secret),
            now + Duration::minutes(2),
        );
        assert_eq!(replay.unwrap_err().code, "invalid_update_secret");
    }

    #[test]
    fn test_missing_registration_update_returns_internal_error() {
        let now = DateTime::<Utc>::from_timestamp(1_700_001_300, 0).unwrap();
        let error = apply_registration_update(
            &mut PersistedState::default(),
            707,
            "https://example.com".to_string(),
            "replacement-verifier".to_string(),
            now,
        )
        .unwrap_err();

        assert_eq!(error.code, "internal_error");
        assert_eq!(error.message, "Registration state inconsistency");
        assert_eq!(error.status_code, 500);
    }
}
