use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const CLAIM_PROOF_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostedProxyData {
    #[serde(default)]
    pub proofs: HashMap<String, ClaimProofRecord>,
    #[serde(default)]
    pub registrations: HashMap<u64, InstallationRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimProofRecord {
    pub installation_id: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationRegistration {
    pub tunnel_url: String,
    pub update_secret: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct HostedProxyStore {
    path: PathBuf,
    data: HostedProxyData,
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

#[derive(Debug, Clone, Serialize)]
pub struct SetupResponse {
    pub status: &'static str,
    pub installation_id: u64,
    pub claim_proof: String,
    pub expires_at: DateTime<Utc>,
    pub register_path: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterSuccessResponse {
    pub status: &'static str,
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret: String,
    pub rotated: bool,
}

#[derive(Debug, Clone)]
pub struct ErrorEnvelope {
    pub status_code: axum::http::StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ErrorEnvelope {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message: message.into(),
        }
    }

    pub fn persistence_failure(message: impl Into<String>) -> Self {
        Self {
            status_code: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            code: "persistence_failure",
            message: message.into(),
        }
    }
}

impl HostedProxyStore {
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let data = if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            serde_json::from_str(&contents).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse hosted proxy state: {err}"),
                )
            })?
        } else {
            HostedProxyData::default()
        };

        Ok(Self { path, data })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn reload(&mut self) -> io::Result<()> {
        let refreshed = Self::load(&self.path)?;
        self.data = refreshed.data;
        Ok(())
    }

    pub fn issue_claim_proof(
        &mut self,
        installation_id: u64,
        now: DateTime<Utc>,
    ) -> Result<SetupResponse, ErrorEnvelope> {
        let proof = format!("cp_{}", Uuid::new_v4().simple());
        let expires_at = now + Duration::minutes(CLAIM_PROOF_TTL_MINUTES);
        let mut next = self.data.clone();
        next.proofs.insert(
            proof.clone(),
            ClaimProofRecord {
                installation_id,
                issued_at: now,
                expires_at,
                used_at: None,
            },
        );

        self.persist(next)?;

        Ok(SetupResponse {
            status: "proof_issued",
            installation_id,
            claim_proof: proof,
            expires_at,
            register_path: "/register",
        })
    }

    pub fn register(
        &mut self,
        request: RegisterRequest,
        now: DateTime<Utc>,
    ) -> Result<(axum::http::StatusCode, RegisterSuccessResponse), ErrorEnvelope> {
        validate_auth_fields(&request)?;
        let normalized_tunnel_url = normalize_tunnel_url(&request.tunnel_url)?;

        match (
            request.claim_proof.as_deref(),
            request.update_secret.as_deref(),
        ) {
            (Some(claim_proof), None) => {
                let mut next = self.data.clone();
                let Some(record) = next.proofs.get_mut(claim_proof) else {
                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::FORBIDDEN,
                        code: "invalid_claim_proof",
                        message: "claim proof is invalid".to_string(),
                    });
                };

                if record.installation_id != request.installation_id {
                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::FORBIDDEN,
                        code: "ownership_mismatch",
                        message: "claim proof belongs to a different installation".to_string(),
                    });
                }

                if record.used_at.is_some() {
                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::FORBIDDEN,
                        code: "claim_proof_already_used",
                        message: "claim proof has already been used".to_string(),
                    });
                }

                if now > record.expires_at {
                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::FORBIDDEN,
                        code: "expired_claim_proof",
                        message: "claim proof has expired".to_string(),
                    });
                }

                if next.registrations.contains_key(&request.installation_id) {
                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::CONFLICT,
                        code: "already_claimed",
                        message: "installation is already claimed".to_string(),
                    });
                }

                let update_secret = generate_update_secret();
                next.registrations.insert(
                    request.installation_id,
                    InstallationRegistration {
                        tunnel_url: normalized_tunnel_url.clone(),
                        update_secret: update_secret.clone(),
                        created_at: now,
                        updated_at: now,
                    },
                );
                record.used_at = Some(now);
                self.persist(next)?;

                Ok((
                    axum::http::StatusCode::CREATED,
                    RegisterSuccessResponse {
                        status: "claimed",
                        installation_id: request.installation_id,
                        tunnel_url: normalized_tunnel_url,
                        update_secret,
                        rotated: false,
                    },
                ))
            }
            (None, Some(update_secret)) => {
                let mut next = self.data.clone();
                let Some(registration) = next.registrations.get_mut(&request.installation_id)
                else {
                    if self
                        .data
                        .registrations
                        .iter()
                        .any(|(_, registration)| registration.update_secret == update_secret)
                    {
                        return Err(ErrorEnvelope {
                            status_code: axum::http::StatusCode::FORBIDDEN,
                            code: "ownership_mismatch",
                            message: "update secret belongs to a different installation"
                                .to_string(),
                        });
                    }

                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::FORBIDDEN,
                        code: "invalid_update_secret",
                        message: "update secret is invalid".to_string(),
                    });
                };

                if registration.update_secret != update_secret {
                    if self
                        .data
                        .registrations
                        .iter()
                        .any(|(installation_id, registration)| {
                            *installation_id != request.installation_id
                                && registration.update_secret == update_secret
                        })
                    {
                        return Err(ErrorEnvelope {
                            status_code: axum::http::StatusCode::FORBIDDEN,
                            code: "ownership_mismatch",
                            message: "update secret belongs to a different installation"
                                .to_string(),
                        });
                    }

                    return Err(ErrorEnvelope {
                        status_code: axum::http::StatusCode::FORBIDDEN,
                        code: "invalid_update_secret",
                        message: "update secret is invalid".to_string(),
                    });
                }

                let next_secret = generate_update_secret();
                registration.tunnel_url = normalized_tunnel_url.clone();
                registration.update_secret = next_secret.clone();
                registration.updated_at = now;
                self.persist(next)?;

                Ok((
                    axum::http::StatusCode::OK,
                    RegisterSuccessResponse {
                        status: "updated",
                        installation_id: request.installation_id,
                        tunnel_url: normalized_tunnel_url,
                        update_secret: next_secret,
                        rotated: true,
                    },
                ))
            }
            _ => Err(ErrorEnvelope::invalid_request(
                "exactly one of claim_proof or update_secret must be present",
            )),
        }
    }

    fn persist(&mut self, next: HostedProxyData) -> Result<(), ErrorEnvelope> {
        persist_state(&self.path, &next).map_err(|err| {
            ErrorEnvelope::persistence_failure(format!(
                "failed to persist hosted proxy state: {err}"
            ))
        })?;
        self.data = next;
        Ok(())
    }
}

fn persist_state(path: &Path, data: &HostedProxyData) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_vec_pretty(data).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize hosted proxy state: {err}"),
        )
    })?;
    std::fs::write(path, payload)
}

fn validate_auth_fields(request: &RegisterRequest) -> Result<(), ErrorEnvelope> {
    if request.installation_id == 0 {
        return Err(ErrorEnvelope::invalid_request(
            "installation_id must be a positive integer",
        ));
    }

    let claim = request
        .claim_proof
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let update = request
        .update_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if (claim.is_some() && update.is_some()) || (claim.is_none() && update.is_none()) {
        return Err(ErrorEnvelope::invalid_request(
            "exactly one of claim_proof or update_secret must be present",
        ));
    }

    Ok(())
}

fn generate_update_secret() -> String {
    format!("us_{}", Uuid::new_v4().simple())
}

fn normalize_tunnel_url(raw: &str) -> Result<String, ErrorEnvelope> {
    let url = Url::parse(raw).map_err(|_| ErrorEnvelope {
        status_code: axum::http::StatusCode::BAD_REQUEST,
        code: "invalid_tunnel_url",
        message: "tunnel_url must be a valid absolute HTTPS origin".to_string(),
    })?;

    if url.scheme() != "https" {
        return Err(ErrorEnvelope {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_tunnel_url",
            message: "tunnel_url must use https".to_string(),
        });
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(ErrorEnvelope {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_tunnel_url",
            message: "tunnel_url must not include embedded credentials".to_string(),
        });
    }

    if url.query().is_some() || url.fragment().is_some() {
        return Err(ErrorEnvelope {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_tunnel_url",
            message: "tunnel_url must not include a query or fragment".to_string(),
        });
    }

    if url.path() != "/" && !url.path().is_empty() {
        return Err(ErrorEnvelope {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "invalid_tunnel_url",
            message: "tunnel_url must be an origin without a path".to_string(),
        });
    }

    let host = url.host_str().ok_or_else(|| ErrorEnvelope {
        status_code: axum::http::StatusCode::BAD_REQUEST,
        code: "invalid_tunnel_url",
        message: "tunnel_url must include a host".to_string(),
    })?;

    if let Ok(ip) = host.parse::<IpAddr>() {
        return normalize_ip_host(ip, url.port());
    }

    if host.eq_ignore_ascii_case("localhost") {
        return Err(ErrorEnvelope {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "unsafe_tunnel_url",
            message: "tunnel_url host must not be localhost".to_string(),
        });
    }

    let host = host.to_ascii_lowercase();
    if let Some(port) = url.port() {
        Ok(format!("https://{host}:{port}"))
    } else {
        Ok(format!("https://{host}"))
    }
}

fn normalize_ip_host(ip: IpAddr, port: Option<u16>) -> Result<String, ErrorEnvelope> {
    if is_unsafe_ip(ip) {
        return Err(ErrorEnvelope {
            status_code: axum::http::StatusCode::BAD_REQUEST,
            code: "unsafe_tunnel_url",
            message: "tunnel_url host must not be loopback or private network space".to_string(),
        });
    }

    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };

    if let Some(port) = port {
        Ok(format!("https://{host}:{port}"))
    } else {
        Ok(format!("https://{host}"))
    }
}

fn is_unsafe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_unspecified()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn register_rejects_private_ip_tunnel_urls() {
        let err = normalize_tunnel_url("https://127.0.0.1:8080").unwrap_err();
        assert_eq!(err.code, "unsafe_tunnel_url");
    }

    #[test]
    fn store_round_trips_proofs_and_registrations() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hosted_proxy.json");
        let mut store = HostedProxyStore::load(&path).unwrap();
        let now = Utc::now();

        let setup = store.issue_claim_proof(42, now).unwrap();
        let (_, result) = store
            .register(
                RegisterRequest {
                    installation_id: 42,
                    tunnel_url: "https://abc123.trycloudflare.com".to_string(),
                    claim_proof: Some(setup.claim_proof),
                    update_secret: None,
                },
                now,
            )
            .unwrap();

        let reloaded = HostedProxyStore::load(&path).unwrap();
        assert_eq!(
            reloaded.data.registrations[&42].tunnel_url,
            result.tunnel_url
        );
        assert_eq!(reloaded.data.proofs.len(), 1);
    }
}
