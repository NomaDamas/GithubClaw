use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::hosted_proxy_state_path;
use crate::errors::{GithubClawError, Result};

const HOSTED_PROXY_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedProxyRegistration {
    pub installation_id: u64,
    pub tunnel_url: String,
    pub update_secret_verifier: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsumedClaimProof {
    pub installation_id: u64,
    pub claim_proof_verifier: String,
    pub consumed_at: DateTime<Utc>,
}

pub trait HostedProxyStateStore: Send + Sync {
    fn registration(&self, installation_id: u64) -> Result<Option<HostedProxyRegistration>>;

    fn upsert_registration(
        &self,
        installation_id: u64,
        tunnel_url: String,
        update_secret_verifier: String,
        now: DateTime<Utc>,
    ) -> Result<HostedProxyRegistration>;

    fn consumed_claim_proof(
        &self,
        installation_id: u64,
        claim_proof_verifier: &str,
    ) -> Result<Option<ConsumedClaimProof>>;

    fn record_consumed_claim_proof(
        &self,
        installation_id: u64,
        claim_proof_verifier: String,
        consumed_at: DateTime<Utc>,
    ) -> Result<()>;
}

#[derive(Debug)]
pub struct JsonFileHostedProxyStateStore {
    path: PathBuf,
    state: Mutex<HostedProxyStateFile>,
}

impl JsonFileHostedProxyStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = load_state_file(&path)?;
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    pub fn from_default_path() -> Result<Self> {
        Self::new(hosted_proxy_state_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl HostedProxyStateStore for JsonFileHostedProxyStateStore {
    fn registration(&self, installation_id: u64) -> Result<Option<HostedProxyRegistration>> {
        let state = self
            .state
            .lock()
            .map_err(|_| GithubClawError::Config("hosted proxy state lock poisoned".into()))?;

        Ok(state
            .installations
            .get(&installation_id)
            .map(|record| HostedProxyRegistration {
                installation_id,
                tunnel_url: record.tunnel_url.clone(),
                update_secret_verifier: record.update_secret_verifier.clone(),
                created_at: record.created_at,
                updated_at: record.updated_at,
            }))
    }

    fn upsert_registration(
        &self,
        installation_id: u64,
        tunnel_url: String,
        update_secret_verifier: String,
        now: DateTime<Utc>,
    ) -> Result<HostedProxyRegistration> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| GithubClawError::Config("hosted proxy state lock poisoned".into()))?;

        let saved = {
            let record = state
                .installations
                .entry(installation_id)
                .or_insert_with(|| HostedProxyInstallationRecord {
                    tunnel_url: tunnel_url.clone(),
                    update_secret_verifier: update_secret_verifier.clone(),
                    created_at: now,
                    updated_at: now,
                    consumed_claim_proofs: BTreeMap::new(),
                });

            if record.created_at != now
                || record.tunnel_url != tunnel_url
                || record.update_secret_verifier != update_secret_verifier
            {
                record.tunnel_url = tunnel_url;
                record.update_secret_verifier = update_secret_verifier;
                record.updated_at = now;
            }

            if record.created_at > record.updated_at {
                record.updated_at = record.created_at;
            }

            HostedProxyRegistration {
                installation_id,
                tunnel_url: record.tunnel_url.clone(),
                update_secret_verifier: record.update_secret_verifier.clone(),
                created_at: record.created_at,
                updated_at: record.updated_at,
            }
        };

        persist_state_file(&self.path, &state)?;

        Ok(saved)
    }

    fn consumed_claim_proof(
        &self,
        installation_id: u64,
        claim_proof_verifier: &str,
    ) -> Result<Option<ConsumedClaimProof>> {
        let state = self
            .state
            .lock()
            .map_err(|_| GithubClawError::Config("hosted proxy state lock poisoned".into()))?;

        Ok(state
            .installations
            .get(&installation_id)
            .and_then(|record| {
                record
                    .consumed_claim_proofs
                    .get(claim_proof_verifier)
                    .copied()
            })
            .map(|consumed_at| ConsumedClaimProof {
                installation_id,
                claim_proof_verifier: claim_proof_verifier.to_string(),
                consumed_at,
            }))
    }

    fn record_consumed_claim_proof(
        &self,
        installation_id: u64,
        claim_proof_verifier: String,
        consumed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| GithubClawError::Config("hosted proxy state lock poisoned".into()))?;

        let record = state
            .installations
            .get_mut(&installation_id)
            .ok_or_else(|| {
                GithubClawError::Config(format!(
                    "cannot record consumed claim proof for unclaimed installation {}",
                    installation_id
                ))
            })?;

        record
            .consumed_claim_proofs
            .insert(claim_proof_verifier, consumed_at);

        persist_state_file(&self.path, &state)?;
        Ok(())
    }
}

pub fn derive_verifier(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostedProxyStateFile {
    version: u32,
    #[serde(default)]
    installations: BTreeMap<u64, HostedProxyInstallationRecord>,
}

impl Default for HostedProxyStateFile {
    fn default() -> Self {
        Self {
            version: HOSTED_PROXY_STATE_VERSION,
            installations: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostedProxyInstallationRecord {
    #[serde(default)]
    tunnel_url: String,
    #[serde(default)]
    update_secret_verifier: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    consumed_claim_proofs: BTreeMap<String, DateTime<Utc>>,
}

fn load_state_file(path: &Path) -> Result<HostedProxyStateFile> {
    if !path.exists() {
        return Ok(HostedProxyStateFile::default());
    }

    let contents = fs::read_to_string(path)?;
    let state = serde_json::from_str(&contents)?;
    Ok(state)
}

fn persist_state_file(path: &Path, state: &HostedProxyStateFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension("json.tmp");
    let payload = serde_json::to_string_pretty(state)?;
    fs::write(&tmp_path, format!("{payload}\n"))?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn json_store_persists_registration_across_restarts() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hosted_proxy_state.json");
        let store = JsonFileHostedProxyStateStore::new(&path).unwrap();
        let created_at = DateTime::parse_from_rfc3339("2026-03-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let saved = store
            .upsert_registration(
                123456,
                "https://abc123.trycloudflare.com".into(),
                derive_verifier("us_initial_secret"),
                created_at,
            )
            .unwrap();

        assert_eq!(saved.created_at, created_at);
        assert_eq!(saved.updated_at, created_at);

        let reopened = JsonFileHostedProxyStateStore::new(&path).unwrap();
        let reloaded = reopened.registration(123456).unwrap().unwrap();

        assert_eq!(reloaded, saved);
    }

    #[test]
    fn json_store_rotates_registration_but_keeps_created_timestamp() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hosted_proxy_state.json");
        let store = JsonFileHostedProxyStateStore::new(&path).unwrap();
        let created_at = DateTime::parse_from_rfc3339("2026-03-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_at = DateTime::parse_from_rfc3339("2026-03-12T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        store
            .upsert_registration(
                123456,
                "https://abc123.trycloudflare.com".into(),
                derive_verifier("us_initial_secret"),
                created_at,
            )
            .unwrap();

        let rotated = store
            .upsert_registration(
                123456,
                "https://next456.trycloudflare.com".into(),
                derive_verifier("us_replacement_secret"),
                updated_at,
            )
            .unwrap();

        assert_eq!(rotated.created_at, created_at);
        assert_eq!(rotated.updated_at, updated_at);
        assert_eq!(rotated.tunnel_url, "https://next456.trycloudflare.com");
    }

    #[test]
    fn json_store_persists_consumed_claim_proof_replay_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hosted_proxy_state.json");
        let store = JsonFileHostedProxyStateStore::new(&path).unwrap();
        let claimed_at = DateTime::parse_from_rfc3339("2026-03-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let consumed_at = DateTime::parse_from_rfc3339("2026-03-12T00:05:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let verifier = derive_verifier("cp_opaque_server_minted_value");

        store
            .upsert_registration(
                123456,
                "https://abc123.trycloudflare.com".into(),
                derive_verifier("us_initial_secret"),
                claimed_at,
            )
            .unwrap();
        store
            .record_consumed_claim_proof(123456, verifier.clone(), consumed_at)
            .unwrap();

        let reopened = JsonFileHostedProxyStateStore::new(&path).unwrap();
        let replay = reopened
            .consumed_claim_proof(123456, &verifier)
            .unwrap()
            .unwrap();

        assert_eq!(replay.installation_id, 123456);
        assert_eq!(replay.claim_proof_verifier, verifier);
        assert_eq!(replay.consumed_at, consumed_at);
    }

    #[test]
    fn json_store_writes_documented_file_shape() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hosted_proxy_state.json");
        let store = JsonFileHostedProxyStateStore::new(&path).unwrap();
        let created_at = DateTime::parse_from_rfc3339("2026-03-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let verifier = derive_verifier("cp_opaque_server_minted_value");

        store
            .upsert_registration(
                123456,
                "https://abc123.trycloudflare.com".into(),
                derive_verifier("us_initial_secret"),
                created_at,
            )
            .unwrap();
        store
            .record_consumed_claim_proof(123456, verifier.clone(), created_at)
            .unwrap();

        let disk: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(disk["version"], HOSTED_PROXY_STATE_VERSION);
        assert_eq!(
            disk["installations"]["123456"]["tunnel_url"],
            "https://abc123.trycloudflare.com"
        );
        assert_eq!(
            disk["installations"]["123456"]["consumed_claim_proofs"][verifier],
            "2026-03-12T00:00:00Z"
        );
    }
}
