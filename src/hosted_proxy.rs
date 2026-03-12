use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimProofRecord {
    pub installation_id: u64,
    pub proof_hash: String,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistrationRecord {
    pub tunnel_url: String,
    pub update_secret_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostedProxyPersistence {
    #[serde(default)]
    claim_proofs: HashMap<String, ClaimProofRecord>,
    #[serde(default)]
    registrations: HashMap<String, RegistrationRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedClaimProof {
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSuccess {
    pub status: &'static str,
    pub http_status: u16,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    BadRequest(ErrorBody),
    Forbidden(ErrorBody),
    Conflict(ErrorBody),
    Internal(ErrorBody),
}

impl RegisterError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(body)
            | Self::Forbidden(body)
            | Self::Conflict(body)
            | Self::Internal(body) => body.code,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RegisterRequest {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub claim_proof: Option<String>,
    pub update_secret: Option<String>,
}

#[derive(Debug)]
pub struct HostedProxyService {
    path: PathBuf,
    state: Mutex<HostedProxyPersistence>,
}

impl HostedProxyService {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let state = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
            serde_json::from_str(&raw)
                .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?
        } else {
            HostedProxyPersistence::default()
        };

        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub fn issue_claim_proof(
        &self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<IssuedClaimProof, String> {
        let claim_proof = format!(
            "cp_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let proof_hash = hash_secret(&claim_proof);
        let expires_at = now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES);
        let mut state = self.state.lock().unwrap();
        prune_stale_claim_proofs(&mut state, now);
        state.claim_proofs.insert(
            proof_hash.clone(),
            ClaimProofRecord {
                installation_id,
                proof_hash,
                expires_at,
                used_at: None,
            },
        );
        self.persist_locked(&state)?;
        Ok(IssuedClaimProof {
            claim_proof,
            expires_at,
        })
    }

    pub fn register(
        &self,
        request: RegisterRequest,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        if request.installation_id == 0 {
            return Err(RegisterError::BadRequest(error_body(
                "invalid_request",
                "installation_id must be a positive integer",
            )));
        }

        let has_claim_proof = request.claim_proof.as_ref().is_some_and(|v| !v.is_empty());
        let has_update_secret = request
            .update_secret
            .as_ref()
            .is_some_and(|v| !v.is_empty());
        if has_claim_proof == has_update_secret {
            return Err(RegisterError::BadRequest(error_body(
                "invalid_request",
                "Exactly one of claim_proof or update_secret must be present",
            )));
        }

        let tunnel_url =
            normalize_tunnel_url(&request.tunnel_url).map_err(RegisterError::BadRequest)?;

        let mut state = self.state.lock().unwrap();
        prune_stale_claim_proofs(&mut state, now);

        if let Some(claim_proof) = request.claim_proof.as_ref() {
            return self.register_initial_claim(
                &mut state,
                request.installation_id,
                &tunnel_url,
                claim_proof,
                now,
            );
        }

        self.register_update(
            &mut state,
            request.installation_id,
            &tunnel_url,
            request.update_secret.as_deref().unwrap(),
            now,
        )
    }

    #[cfg(test)]
    pub fn persisted_claim_proof(&self, claim_proof: &str) -> Option<ClaimProofRecord> {
        let hash = hash_secret(claim_proof);
        self.state.lock().unwrap().claim_proofs.get(&hash).cloned()
    }

    #[cfg(test)]
    pub fn persisted_registration(&self, installation_id: u64) -> Option<RegistrationRecord> {
        self.state
            .lock()
            .unwrap()
            .registrations
            .get(&installation_id.to_string())
            .cloned()
    }

    fn register_initial_claim(
        &self,
        state: &mut HostedProxyPersistence,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        let proof_hash = hash_secret(claim_proof);
        let Some(record) = state.claim_proofs.get(&proof_hash).cloned() else {
            return Err(RegisterError::Forbidden(error_body(
                "invalid_claim_proof",
                "claim_proof is not recognized",
            )));
        };

        if record.installation_id != installation_id {
            return Err(RegisterError::Forbidden(error_body(
                "ownership_mismatch",
                "claim_proof is bound to a different installation_id",
            )));
        }

        if record.used_at.is_some() {
            return Err(RegisterError::Forbidden(error_body(
                "claim_proof_already_used",
                "claim_proof has already been consumed",
            )));
        }

        if record.expires_at <= now {
            return Err(RegisterError::Forbidden(error_body(
                "expired_claim_proof",
                "claim_proof has expired",
            )));
        }

        if state
            .registrations
            .contains_key(&installation_id.to_string())
        {
            return Err(RegisterError::Conflict(error_body(
                "already_claimed",
                "installation_id has already been claimed",
            )));
        }

        let update_secret = mint_secret("us");
        let update_secret_hash = hash_secret(&update_secret);
        let registration = RegistrationRecord {
            tunnel_url: tunnel_url.to_string(),
            update_secret_hash,
            created_at: now,
            updated_at: now,
        };
        state
            .registrations
            .insert(installation_id.to_string(), registration);
        if let Some(stored_proof) = state.claim_proofs.get_mut(&proof_hash) {
            stored_proof.used_at = Some(now);
        }
        self.persist_locked(state).map_err(|_| {
            RegisterError::Internal(error_body(
                "persistence_failure",
                "Failed to persist hosted proxy state",
            ))
        })?;

        Ok(RegisterSuccess {
            status: "claimed",
            http_status: 201,
            installation_id,
            tunnel_url: tunnel_url.to_string(),
            update_secret,
            rotated: false,
        })
    }

    fn register_update(
        &self,
        state: &mut HostedProxyPersistence,
        installation_id: u64,
        tunnel_url: &str,
        update_secret: &str,
        now: DateTime<Utc>,
    ) -> Result<RegisterSuccess, RegisterError> {
        let update_secret_hash = hash_secret(update_secret);
        let installation_key = installation_id.to_string();
        let Some(existing) = state.registrations.get(&installation_key).cloned() else {
            if state
                .registrations
                .values()
                .any(|record| record.update_secret_hash == update_secret_hash)
            {
                return Err(RegisterError::Forbidden(error_body(
                    "ownership_mismatch",
                    "update_secret is bound to a different installation_id",
                )));
            }
            return Err(RegisterError::Forbidden(error_body(
                "invalid_update_secret",
                "update_secret is not valid for this installation_id",
            )));
        };

        if existing.update_secret_hash != update_secret_hash {
            if state
                .registrations
                .values()
                .any(|record| record.update_secret_hash == update_secret_hash)
            {
                return Err(RegisterError::Forbidden(error_body(
                    "ownership_mismatch",
                    "update_secret is bound to a different installation_id",
                )));
            }
            return Err(RegisterError::Forbidden(error_body(
                "invalid_update_secret",
                "update_secret is not valid for this installation_id",
            )));
        }

        let replacement_secret = mint_secret("us");
        state.registrations.insert(
            installation_key,
            RegistrationRecord {
                tunnel_url: tunnel_url.to_string(),
                update_secret_hash: hash_secret(&replacement_secret),
                created_at: existing.created_at,
                updated_at: now,
            },
        );
        self.persist_locked(state).map_err(|_| {
            RegisterError::Internal(error_body(
                "secret_rotation_failure",
                "Failed to persist rotated update_secret",
            ))
        })?;

        Ok(RegisterSuccess {
            status: "updated",
            http_status: 200,
            installation_id,
            tunnel_url: tunnel_url.to_string(),
            update_secret: replacement_secret,
            rotated: true,
        })
    }

    fn persist_locked(&self, state: &HostedProxyPersistence) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
        }
        let raw = serde_json::to_vec_pretty(state)
            .map_err(|e| format!("Failed to serialize hosted proxy state: {}", e))?;
        std::fs::write(&self.path, raw)
            .map_err(|e| format!("Failed to write {}: {}", self.path.display(), e))
    }
}

fn mint_secret(prefix: &str) -> String {
    format!(
        "{prefix}_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn error_body(code: &'static str, message: impl Into<String>) -> ErrorBody {
    ErrorBody {
        code,
        message: message.into(),
    }
}

fn hash_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

fn prune_stale_claim_proofs(state: &mut HostedProxyPersistence, now: DateTime<Utc>) {
    state.claim_proofs.retain(|_, record| {
        let keep_unused = record.used_at.is_none()
            && record.expires_at > now - Duration::minutes(CLAIM_PROOF_TTL_MINUTES);
        let keep_used = record.used_at.is_some() && record.expires_at > now - Duration::days(1);
        keep_unused || keep_used
    });
}

pub fn normalize_tunnel_url(raw: &str) -> Result<String, ErrorBody> {
    let url = Url::parse(raw).map_err(|_| {
        error_body(
            "invalid_tunnel_url",
            "tunnel_url must be a valid absolute URL",
        )
    })?;

    if url.scheme() != "https" {
        return Err(error_body(
            "invalid_tunnel_url",
            "tunnel_url must use https",
        ));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(error_body(
            "invalid_tunnel_url",
            "tunnel_url must not include userinfo",
        ));
    }

    if url.query().is_some() || url.fragment().is_some() {
        return Err(error_body(
            "invalid_tunnel_url",
            "tunnel_url must not include query or fragment",
        ));
    }

    if url.host_str().is_none() {
        return Err(error_body(
            "invalid_tunnel_url",
            "tunnel_url must include a host",
        ));
    }

    if url.path() != "/" && !url.path().is_empty() {
        return Err(error_body(
            "invalid_tunnel_url",
            "tunnel_url must be an origin only, without a path",
        ));
    }

    let host = url.host_str().unwrap().to_ascii_lowercase();
    if host == "localhost" {
        return Err(error_body(
            "unsafe_tunnel_url",
            "localhost tunnel URLs are not allowed",
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_unsafe_ip(ip) {
            return Err(error_body(
                "unsafe_tunnel_url",
                "private, loopback, and local network tunnel URLs are not allowed",
            ));
        }
    }

    let mut normalized = format!("https://{host}");
    if let Some(port) = url.port() {
        if port != 443 {
            normalized.push(':');
            normalized.push_str(&port.to_string());
        }
    }

    Ok(normalized)
}

fn is_unsafe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_unsafe_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_unsafe_ipv6(ipv6),
    }
}

fn is_unsafe_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
}

fn is_unsafe_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local() || ip.is_unicast_link_local()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn service(tmp: &TempDir) -> HostedProxyService {
        HostedProxyService::load(tmp.path().join("hosted_proxy_state.json")).unwrap()
    }

    #[test]
    fn issue_claim_proof_persists_hash_and_expiry() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();

        let issued = service.issue_claim_proof(42, now).unwrap();
        let stored = service.persisted_claim_proof(&issued.claim_proof).unwrap();

        assert_eq!(stored.installation_id, 42);
        assert_eq!(stored.expires_at, issued.expires_at);
        assert!(stored.used_at.is_none());
        assert_ne!(stored.proof_hash, issued.claim_proof);
    }

    #[test]
    fn initial_claim_consumes_proof_once() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof(99, now).unwrap();

        let first = service
            .register(
                RegisterRequest {
                    installation_id: 99,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof.clone()),
                    update_secret: None,
                },
                now + Duration::seconds(5),
            )
            .unwrap();
        assert_eq!(first.status, "claimed");

        let second = service
            .register(
                RegisterRequest {
                    installation_id: 99,
                    tunnel_url: "https://next.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::seconds(10),
            )
            .unwrap_err();
        assert_eq!(second.code(), "claim_proof_already_used");
    }

    #[test]
    fn expired_claim_proof_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof(5, now).unwrap();

        let result = service
            .register(
                RegisterRequest {
                    installation_id: 5,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES + 1),
            )
            .unwrap_err();
        assert_eq!(result.code(), "expired_claim_proof");
    }

    #[test]
    fn claim_proof_is_bound_to_installation() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof(7, now).unwrap();

        let result = service
            .register(
                RegisterRequest {
                    installation_id: 8,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::seconds(1),
            )
            .unwrap_err();
        assert_eq!(result.code(), "ownership_mismatch");
    }

    #[test]
    fn update_secret_rotates_on_success() {
        let tmp = TempDir::new().unwrap();
        let service = service(&tmp);
        let now = Utc::now();
        let issued = service.issue_claim_proof(11, now).unwrap();
        let initial = service
            .register(
                RegisterRequest {
                    installation_id: 11,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(issued.claim_proof),
                    update_secret: None,
                },
                now + Duration::seconds(1),
            )
            .unwrap();

        let updated = service
            .register(
                RegisterRequest {
                    installation_id: 11,
                    tunnel_url: "https://next.trycloudflare.com".to_string(),
                    claim_proof: None,
                    update_secret: Some(initial.update_secret.clone()),
                },
                now + Duration::seconds(2),
            )
            .unwrap();
        assert_eq!(updated.status, "updated");
        assert_ne!(updated.update_secret, initial.update_secret);

        let stale = service
            .register(
                RegisterRequest {
                    installation_id: 11,
                    tunnel_url: "https://third.trycloudflare.com".to_string(),
                    claim_proof: None,
                    update_secret: Some(initial.update_secret),
                },
                now + Duration::seconds(3),
            )
            .unwrap_err();
        assert_eq!(stale.code(), "invalid_update_secret");
    }

    #[test]
    fn normalize_tunnel_url_rejects_unsafe_targets() {
        assert_eq!(
            normalize_tunnel_url("https://127.0.0.1:3000")
                .unwrap_err()
                .code,
            "unsafe_tunnel_url"
        );
        assert_eq!(
            normalize_tunnel_url("http://example.com").unwrap_err().code,
            "invalid_tunnel_url"
        );
        assert_eq!(
            normalize_tunnel_url("https://example.com/path")
                .unwrap_err()
                .code,
            "invalid_tunnel_url"
        );
        assert_eq!(
            normalize_tunnel_url("https://Example.com:443").unwrap(),
            "https://example.com"
        );
    }
}
