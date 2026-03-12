//! Webhook server — axum HTTP entry point.
//!
//! Receives GitHub webhook events, verifies HMAC signatures, applies the fork
//! PR gate, and routes events into per-repo disk-persisted queues.
//!
//! Translated from the Python `server.py`.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::agents::parser::load_agent_definition;
use crate::agents::prompt_assembler::PromptAssembler;
use crate::agents::spawner::AgentSpawner;
use crate::orchestrator::schema::Action;
use crate::orchestrator::session::OrchestratorSession;
use crate::process_manager::{check_fork_pr_gate, ProcessManager};
use crate::queue::DiskPersistedQueue;
use crate::scheduler::ScheduledEventManager;
use crate::signature::verify_webhook_signature;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// All shared state for the running webhook server.
pub struct ServerState {
    pub webhook_secret: String,
    pub registry: RwLock<HashMap<String, RegistryEntry>>,
    pub started_repos: RwLock<HashSet<String>>,
    pub queues: Mutex<HashMap<String, DiskPersistedQueue>>,
    pub githubclaw_home: PathBuf,
    pub process_manager: Arc<ProcessManager>,
    pub scheduler: Mutex<ScheduledEventManager>,
    pub rate_limiter: Arc<crate::rate_limiter::RateLimiter>,
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Which CLI backend to use for the orchestrator (codex or claude-code).
    pub orchestrator_backend: crate::orchestrator::session::OrchestratorBackend,
    /// Per-repo orchestrator sessions (created on demand in the drain loop).
    pub orchestrators: Mutex<HashMap<String, OrchestratorSession>>,
    pub registration_store: Mutex<RegistrationStore>,
}

/// A single entry in `registry.json`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct RegistryEntry {
    pub local_path: String,
    #[serde(default)]
    pub socket_path: String,
}

/// Top-level shape of `registry.json` when it uses the nested `{ "repos": { ... } }` format.
#[derive(Debug, serde::Deserialize)]
pub struct RegistryFile {
    #[serde(default)]
    pub repos: HashMap<String, RegistryEntry>,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistrationStoreFile {
    #[serde(default)]
    pub installations: HashMap<String, InstallationRegistration>,
    #[serde(default)]
    pub claim_proofs: HashMap<String, ClaimProofRecord>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InstallationRegistration {
    pub tunnel_url: String,
    pub update_secret_hash: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClaimProofRecord {
    pub installation_id: u64,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub used_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct RegistrationStore {
    path: PathBuf,
    installations: HashMap<String, InstallationRegistration>,
    claim_proofs: HashMap<String, ClaimProofRecord>,
}

#[derive(Debug, serde::Deserialize)]
struct RegisterRequest {
    installation_id: u64,
    tunnel_url: String,
    #[serde(default)]
    claim_proof: Option<String>,
    #[serde(default)]
    update_secret: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct RegisterSuccessResponse {
    status: &'static str,
    installation_id: u64,
    tunnel_url: String,
    update_secret: String,
    rotated: bool,
}

#[derive(Debug, Clone)]
enum RegisterCredential {
    ClaimProof(String),
    UpdateSecret(String),
}

#[derive(Debug, Clone)]
enum RegisterError {
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
    SecretRotationFailure,
}

#[derive(Debug, Clone)]
struct ValidRegisterRequest {
    installation_id: u64,
    tunnel_url: String,
    credential: RegisterCredential,
}

impl RegistrationStore {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self {
                path: path.to_path_buf(),
                installations: HashMap::new(),
                claim_proofs: HashMap::new(),
            });
        }

        let content = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "Failed to read registration store {}: {}",
                path.display(),
                e
            )
        })?;
        let file: RegistrationStoreFile = serde_json::from_str(&content).map_err(|e| {
            format!(
                "Failed to parse registration store {}: {}",
                path.display(),
                e
            )
        })?;

        Ok(Self {
            path: path.to_path_buf(),
            installations: file.installations,
            claim_proofs: file.claim_proofs,
        })
    }

    pub fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create registration store directory {}: {}",
                    parent.display(),
                    e
                )
            })?;
        }

        let file = RegistrationStoreFile {
            installations: self.installations.clone(),
            claim_proofs: self.claim_proofs.clone(),
        };
        let data = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("Failed to serialize registration store: {}", e))?;
        let tmp_path = self.path.with_extension("tmp");
        std::fs::write(&tmp_path, data)
            .map_err(|e| format!("Failed to write temp registration store: {}", e))?;
        std::fs::rename(&tmp_path, &self.path)
            .map_err(|e| format!("Failed to finalize registration store write: {}", e))?;
        Ok(())
    }

    fn claim(
        &mut self,
        installation_id: u64,
        tunnel_url: &str,
        claim_proof: &str,
    ) -> Result<String, RegisterError> {
        let installation_key = installation_id.to_string();
        let proof_hash = hash_secret(claim_proof);
        let now = Utc::now();

        let record = self
            .claim_proofs
            .get(&proof_hash)
            .cloned()
            .ok_or(RegisterError::InvalidClaimProof)?;

        if record.installation_id != installation_id {
            return Err(RegisterError::OwnershipMismatch);
        }
        if record.used_at.is_some() {
            return Err(RegisterError::ClaimProofAlreadyUsed);
        }
        if record.expires_at < now {
            return Err(RegisterError::ExpiredClaimProof);
        }
        if self.installations.contains_key(&installation_key) {
            return Err(RegisterError::AlreadyClaimed);
        }

        let update_secret =
            generate_update_secret().map_err(|_| RegisterError::SecretRotationFailure)?;
        let update_secret_hash = hash_secret(&update_secret);
        let registration = InstallationRegistration {
            tunnel_url: tunnel_url.to_string(),
            update_secret_hash,
            created_at: now,
            updated_at: now,
        };

        self.installations.insert(installation_key, registration);
        if let Some(proof) = self.claim_proofs.get_mut(&proof_hash) {
            proof.used_at = Some(now);
        }

        self.persist().map_err(RegisterError::PersistenceFailure)?;
        Ok(update_secret)
    }

    fn update(
        &mut self,
        installation_id: u64,
        tunnel_url: &str,
        update_secret: &str,
    ) -> Result<String, RegisterError> {
        let installation_key = installation_id.to_string();
        let supplied_hash = hash_secret(update_secret);
        let secret_belongs_elsewhere = self.installations.iter().any(|(key, entry)| {
            key != &installation_key && entry.update_secret_hash == supplied_hash
        });

        let registration =
            self.installations
                .get_mut(&installation_key)
                .ok_or(if secret_belongs_elsewhere {
                    RegisterError::OwnershipMismatch
                } else {
                    RegisterError::InvalidUpdateSecret
                })?;

        if registration.update_secret_hash != supplied_hash {
            return Err(if secret_belongs_elsewhere {
                RegisterError::OwnershipMismatch
            } else {
                RegisterError::InvalidUpdateSecret
            });
        }

        let next_secret =
            generate_update_secret().map_err(|_| RegisterError::SecretRotationFailure)?;
        registration.tunnel_url = tunnel_url.to_string();
        registration.update_secret_hash = hash_secret(&next_secret);
        registration.updated_at = Utc::now();

        self.persist().map_err(RegisterError::PersistenceFailure)?;
        Ok(next_secret)
    }

    #[cfg(test)]
    fn seed_claim_proof_for_test(
        &mut self,
        proof: &str,
        installation_id: u64,
        expires_at: DateTime<Utc>,
    ) {
        self.claim_proofs.insert(
            hash_secret(proof),
            ClaimProofRecord {
                installation_id,
                expires_at,
                used_at: None,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Configuration helpers
// ---------------------------------------------------------------------------

/// Read the webhook secret from a file and trim whitespace.
pub fn load_webhook_secret(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .map_err(|e| {
            format!(
                "Failed to read webhook secret from {}: {}",
                path.display(),
                e
            )
        })
}

/// Load the repo registry mapping `full_name -> RegistryEntry`.
///
/// Supports two formats:
/// - Nested: `{ "repos": { "owner/repo": { "local_path": "..." } } }`
/// - Flat: `{ "owner/repo": { "local_path": "..." } }`
pub fn load_registry(path: &Path) -> HashMap<String, RegistryEntry> {
    if !path.exists() {
        warn!(
            "Registry file not found at {} -- no repos registered",
            path.display()
        );
        return HashMap::new();
    }

    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to read registry file {}: {}", path.display(), e);
            return HashMap::new();
        }
    };

    // Try nested format first: { "repos": { ... } }
    if let Ok(nested) = serde_json::from_str::<RegistryFile>(&content) {
        if !nested.repos.is_empty() {
            return nested.repos;
        }
    }

    // Try flat format: { "owner/repo": { ... } }
    match serde_json::from_str::<HashMap<String, RegistryEntry>>(&content) {
        Ok(flat) => flat,
        Err(e) => {
            warn!("Failed to parse registry.json: {}", e);
            HashMap::new()
        }
    }
}

fn hash_secret(value: &str) -> String {
    use sha2::Digest;

    let mut hasher = sha2::Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_update_secret() -> Result<String, String> {
    use uuid::Uuid;

    Ok(format!("us_{}", Uuid::new_v4().simple()))
}

pub fn registration_store_path(githubclaw_home: &Path) -> PathBuf {
    githubclaw_home.join("hosted_proxy_registrations.json")
}

fn register_error_response(error: RegisterError) -> (StatusCode, Json<Value>) {
    let (status, code, message) = match error {
        RegisterError::InvalidRequest(message) => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            message.to_string(),
        ),
        RegisterError::InvalidTunnelUrl(message) => (
            StatusCode::BAD_REQUEST,
            "invalid_tunnel_url",
            message.to_string(),
        ),
        RegisterError::UnsafeTunnelUrl(message) => (
            StatusCode::BAD_REQUEST,
            "unsafe_tunnel_url",
            message.to_string(),
        ),
        RegisterError::InvalidClaimProof => (
            StatusCode::FORBIDDEN,
            "invalid_claim_proof",
            "Claim proof was not recognized.".to_string(),
        ),
        RegisterError::ExpiredClaimProof => (
            StatusCode::FORBIDDEN,
            "expired_claim_proof",
            "Claim proof has expired.".to_string(),
        ),
        RegisterError::ClaimProofAlreadyUsed => (
            StatusCode::FORBIDDEN,
            "claim_proof_already_used",
            "Claim proof was already used.".to_string(),
        ),
        RegisterError::InvalidUpdateSecret => (
            StatusCode::FORBIDDEN,
            "invalid_update_secret",
            "Update secret is invalid.".to_string(),
        ),
        RegisterError::OwnershipMismatch => (
            StatusCode::FORBIDDEN,
            "ownership_mismatch",
            "Credential belongs to a different installation.".to_string(),
        ),
        RegisterError::AlreadyClaimed => (
            StatusCode::CONFLICT,
            "already_claimed",
            "Installation is already claimed.".to_string(),
        ),
        RegisterError::PersistenceFailure(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_failure",
            message,
        ),
        RegisterError::SecretRotationFailure => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "secret_rotation_failure",
            "Failed to rotate update secret.".to_string(),
        ),
    };

    (
        status,
        Json(serde_json::json!({
            "error": {
                "code": code,
                "message": message,
            }
        })),
    )
}

fn normalize_tunnel_url(raw: &str) -> Result<String, RegisterError> {
    let parsed = Url::parse(raw)
        .map_err(|_| RegisterError::InvalidTunnelUrl("Tunnel URL must be a valid absolute URL."))?;

    if parsed.scheme() != "https" {
        return Err(RegisterError::InvalidTunnelUrl(
            "Tunnel URL must use https.",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(RegisterError::InvalidTunnelUrl(
            "Tunnel URL must not include userinfo.",
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(RegisterError::InvalidTunnelUrl(
            "Tunnel URL must not include query or fragment.",
        ));
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(RegisterError::InvalidTunnelUrl(
            "Tunnel URL must be an origin only, without a path.",
        ));
    }

    let host = parsed.host_str().ok_or(RegisterError::InvalidTunnelUrl(
        "Tunnel URL must include a host.",
    ))?;

    if host.eq_ignore_ascii_case("localhost") {
        return Err(RegisterError::UnsafeTunnelUrl(
            "Tunnel URL must not target localhost.",
        ));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        let unsafe_ip = match ip {
            IpAddr::V4(ipv4) => {
                ipv4.is_private()
                    || ipv4.is_loopback()
                    || ipv4.is_link_local()
                    || ipv4.is_multicast()
                    || ipv4.is_unspecified()
            }
            IpAddr::V6(ipv6) => {
                ipv6.is_loopback()
                    || ipv6.is_multicast()
                    || ipv6.is_unspecified()
                    || ipv6.is_unique_local()
                    || ipv6.is_unicast_link_local()
            }
        };

        if unsafe_ip {
            return Err(RegisterError::UnsafeTunnelUrl(
                "Tunnel URL must not target a loopback or private-network address.",
            ));
        }
    }

    Ok(parsed.origin().ascii_serialization())
}

fn validate_register_request(
    request: RegisterRequest,
) -> Result<ValidRegisterRequest, RegisterError> {
    if request.installation_id == 0 {
        return Err(RegisterError::InvalidRequest(
            "installation_id must be a positive integer.",
        ));
    }

    let tunnel_url = normalize_tunnel_url(&request.tunnel_url)?;
    let credential = match (request.claim_proof, request.update_secret) {
        (Some(_), Some(_)) => {
            return Err(RegisterError::InvalidRequest(
                "Exactly one of claim_proof or update_secret must be present.",
            ))
        }
        (None, None) => {
            return Err(RegisterError::InvalidRequest(
                "Exactly one of claim_proof or update_secret must be present.",
            ))
        }
        (Some(claim_proof), None) => RegisterCredential::ClaimProof(claim_proof),
        (None, Some(update_secret)) => RegisterCredential::UpdateSecret(update_secret),
    };

    Ok(ValidRegisterRequest {
        installation_id: request.installation_id,
        tunnel_url,
        credential,
    })
}

// ---------------------------------------------------------------------------
// Fork PR gate
// ---------------------------------------------------------------------------

/// Add a `_githubclaw_fork_unapproved` flag to the payload when the event is
/// a fork PR without the `githubclaw-approved` label.
fn annotate_fork_status(mut payload: Value) -> Value {
    let is_fork = payload
        .pointer("/pull_request/head/repo/fork")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !is_fork {
        return payload;
    }

    let has_approved_label = payload
        .pointer("/pull_request/labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .any(|l| l.get("name").and_then(Value::as_str) == Some("githubclaw-approved"))
        })
        .unwrap_or(false);

    if !has_approved_label {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("_githubclaw_fork_unapproved".to_string(), Value::Bool(true));
        }
        let pr_number = payload
            .pointer("/pull_request/number")
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        let fork_name = payload
            .pointer("/pull_request/head/repo/full_name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        info!(
            "Fork PR #{} from {} queued as unapproved",
            pr_number, fork_name
        );
    }

    payload
}

// ---------------------------------------------------------------------------
// Queue helper
// ---------------------------------------------------------------------------

/// Return (or create) the disk-persisted queue for a repo.
///
/// Must be called with the queues mutex already locked.
fn get_or_create_queue<'a>(
    queues: &'a mut HashMap<String, DiskPersistedQueue>,
    registry: &HashMap<String, RegistryEntry>,
    githubclaw_home: &Path,
    repo_full_name: &str,
) -> std::io::Result<&'a mut DiskPersistedQueue> {
    if !queues.contains_key(repo_full_name) {
        let queue_dir = if let Some(entry) = registry.get(repo_full_name) {
            if !entry.local_path.is_empty() {
                PathBuf::from(&entry.local_path)
                    .join(".githubclaw")
                    .join("queue")
            } else {
                fallback_queue_dir(githubclaw_home, repo_full_name)
            }
        } else {
            fallback_queue_dir(githubclaw_home, repo_full_name)
        };

        let q = DiskPersistedQueue::new(&queue_dir, crate::constants::DEFAULT_QUEUE_MAX_RETRY)?;
        queues.insert(repo_full_name.to_string(), q);
    }
    Ok(queues.get_mut(repo_full_name).unwrap())
}

/// Public wrapper around `get_or_create_queue` for use by the scheduler firing loop.
pub fn get_or_create_queue_pub<'a>(
    queues: &'a mut HashMap<String, DiskPersistedQueue>,
    registry: &HashMap<String, RegistryEntry>,
    githubclaw_home: &Path,
    repo_full_name: &str,
) -> std::io::Result<&'a mut DiskPersistedQueue> {
    get_or_create_queue(queues, registry, githubclaw_home, repo_full_name)
}

async fn registry_snapshot(state: &Arc<ServerState>) -> HashMap<String, RegistryEntry> {
    state.registry.read().await.clone()
}

pub async fn start_repo_processing(
    state: Arc<ServerState>,
    repo_name: &str,
    entry: &RegistryEntry,
) {
    {
        let mut started = state.started_repos.write().await;
        if !started.insert(repo_name.to_string()) {
            return;
        }
    }

    let state_clone = Arc::clone(&state);
    let repo = repo_name.to_string();
    let entry_clone = entry.clone();
    tokio::spawn(async move {
        event_drain_loop(&state_clone, &repo, &entry_clone).await;
    });
}

async fn ensure_repo_registered(
    state: &Arc<ServerState>,
    repo_full_name: &str,
) -> Option<RegistryEntry> {
    if let Some(entry) = state.registry.read().await.get(repo_full_name).cloned() {
        return Some(entry);
    }

    let registry_path = state.githubclaw_home.join("registry.json");
    let latest = load_registry(&registry_path);
    let entry = latest.get(repo_full_name).cloned()?;

    {
        let mut registry = state.registry.write().await;
        *registry = latest;
    }

    if let Err(e) = bootstrap_repo(state, repo_full_name, &entry, false).await {
        warn!(
            "Bootstrap failed for {} after registry refresh: {}",
            repo_full_name, e
        );
    }
    start_repo_processing(Arc::clone(state), repo_full_name, &entry).await;
    info!(
        "Hot-registered repo {} from refreshed registry",
        repo_full_name
    );

    Some(entry)
}

fn fallback_queue_dir(githubclaw_home: &Path, repo_full_name: &str) -> PathBuf {
    let slug = repo_full_name.replace('/', "_");
    let dir = githubclaw_home.join("queues").join(slug).join("queue");
    warn!(
        "No local_path in registry for {}, falling back to {}",
        repo_full_name,
        dir.display()
    );
    dir
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn webhook_handler(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<(StatusCode, String), (StatusCode, String)> {
    // 1. Signature verification
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if signature.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            "Missing X-Hub-Signature-256 header".to_string(),
        ));
    }

    if !verify_webhook_signature(&body, signature, &state.webhook_secret) {
        warn!("Invalid webhook signature");
        return Err((StatusCode::FORBIDDEN, "Invalid signature".to_string()));
    }

    // 2. Parse JSON payload
    let payload: Value = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid JSON payload: {}", e),
        )
    })?;

    // 3. Registry check
    let repo_full_name = payload
        .pointer("/repository/full_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if repo_full_name.is_empty() {
        debug!("Discarding event for unregistered repo: {}", repo_full_name);
        return Ok((StatusCode::OK, "Ignored: repo not registered".to_string()));
    }

    if ensure_repo_registered(&state, &repo_full_name)
        .await
        .is_none()
    {
        debug!(
            "Discarding event for unregistered repo after refresh: {}",
            repo_full_name
        );
        return Ok((StatusCode::OK, "Ignored: repo not registered".to_string()));
    }

    // 4. Fork PR annotation
    let payload = annotate_fork_status(payload);

    // 5. Build event label
    let event_type = headers
        .get("X-Github-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    let action = payload.get("action").and_then(Value::as_str).unwrap_or("");

    let event_label = if action.is_empty() {
        event_type.to_string()
    } else {
        format!("{}_{}", event_type, action)
    };

    // 6. Enqueue
    let mut queues = state.queues.lock().await;
    let registry = registry_snapshot(&state).await;
    let queue = get_or_create_queue(
        &mut queues,
        &registry,
        &state.githubclaw_home,
        &repo_full_name,
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Queue error: {}", e),
        )
    })?;

    queue.enqueue(payload, &event_label).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Enqueue error: {}", e),
        )
    })?;

    let queue_size = queue.size();
    info!(
        "Queued event {} for {} (queue_size={})",
        event_label, repo_full_name, queue_size
    );

    Ok((StatusCode::ACCEPTED, "Event queued".to_string()))
}

async fn health_handler(State(state): State<Arc<ServerState>>) -> Json<Value> {
    Json(serde_json::json!({
        "status": "ok",
        "registered_repos": state.registry.read().await.len(),
    }))
}

async fn register_handler(
    State(state): State<Arc<ServerState>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let request: RegisterRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return register_error_response(RegisterError::InvalidRequest(
                "Request body must be valid JSON.",
            ));
        }
    };

    let request = match validate_register_request(request) {
        Ok(request) => request,
        Err(error) => return register_error_response(error),
    };

    let mut store = state.registration_store.lock().await;
    let result = match request.credential {
        RegisterCredential::ClaimProof(claim_proof) => store
            .claim(request.installation_id, &request.tunnel_url, &claim_proof)
            .map(|update_secret| {
                (
                    StatusCode::CREATED,
                    RegisterSuccessResponse {
                        status: "claimed",
                        installation_id: request.installation_id,
                        tunnel_url: request.tunnel_url.clone(),
                        update_secret,
                        rotated: false,
                    },
                )
            }),
        RegisterCredential::UpdateSecret(update_secret) => store
            .update(request.installation_id, &request.tunnel_url, &update_secret)
            .map(|next_secret| {
                (
                    StatusCode::OK,
                    RegisterSuccessResponse {
                        status: "updated",
                        installation_id: request.installation_id,
                        tunnel_url: request.tunnel_url.clone(),
                        update_secret: next_secret,
                        rotated: true,
                    },
                )
            }),
    };

    match result {
        Ok((status, response)) => match serde_json::to_value(response) {
            Ok(value) => (status, Json(value)),
            Err(_) => register_error_response(RegisterError::PersistenceFailure(
                "Failed to serialize registration response.".to_string(),
            )),
        },
        Err(error) => register_error_response(error),
    }
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

/// Bootstrap a repo by scanning existing open issues and PRs.
///
/// When `force` is false, skips if the queue already has events.
pub async fn bootstrap_repo(
    state: &Arc<ServerState>,
    repo_name: &str,
    _entry: &RegistryEntry,
    force: bool,
) -> Result<(), String> {
    // Check if queue already has events
    if !force {
        let mut queues = state.queues.lock().await;
        let registry = registry_snapshot(state).await;
        let queue = get_or_create_queue(&mut queues, &registry, &state.githubclaw_home, repo_name)
            .map_err(|e| format!("Queue creation error: {}", e))?;

        if queue.size() > 0 {
            info!(
                "Skipping bootstrap for {} (queue already has {} events)",
                repo_name,
                queue.size()
            );
            return Ok(());
        }
    }

    // Scan existing open issues
    let issue_output = tokio::process::Command::new("gh")
        .args([
            "issue",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,labels",
            "--limit",
            "100",
            "--repo",
            repo_name,
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run gh issue list: {}", e))?;

    if issue_output.status.success() {
        let issues: Vec<serde_json::Value> =
            serde_json::from_slice(&issue_output.stdout).unwrap_or_default();
        if !issues.is_empty() {
            let mut queues = state.queues.lock().await;
            let registry = registry_snapshot(state).await;
            let queue =
                get_or_create_queue(&mut queues, &registry, &state.githubclaw_home, repo_name)
                    .map_err(|e| format!("Queue creation error: {}", e))?;

            for issue in &issues {
                queue
                    .enqueue(
                        serde_json::json!({
                            "type": "virtual_bootstrap",
                            "source": "bootstrap",
                            "item_type": "issue",
                            "data": issue
                        }),
                        "virtual_bootstrap",
                    )
                    .map_err(|e| format!("Enqueue error: {}", e))?;
            }
            info!(
                "Bootstrapped {} open issues for {}",
                issues.len(),
                repo_name
            );
        }
    } else {
        warn!(
            "gh issue list failed for {}: {}",
            repo_name,
            String::from_utf8_lossy(&issue_output.stderr).trim()
        );
    }

    // Scan existing open PRs
    let pr_output = tokio::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,labels",
            "--limit",
            "100",
            "--repo",
            repo_name,
        ])
        .output()
        .await
        .map_err(|e| format!("Failed to run gh pr list: {}", e))?;

    if pr_output.status.success() {
        let prs: Vec<serde_json::Value> =
            serde_json::from_slice(&pr_output.stdout).unwrap_or_default();
        if !prs.is_empty() {
            let mut queues = state.queues.lock().await;
            let registry = registry_snapshot(state).await;
            let queue =
                get_or_create_queue(&mut queues, &registry, &state.githubclaw_home, repo_name)
                    .map_err(|e| format!("Queue creation error: {}", e))?;

            for pr in &prs {
                queue
                    .enqueue(
                        serde_json::json!({
                            "type": "virtual_bootstrap",
                            "source": "bootstrap",
                            "item_type": "pull_request",
                            "data": pr
                        }),
                        "virtual_bootstrap",
                    )
                    .map_err(|e| format!("Enqueue error: {}", e))?;
            }
            info!("Bootstrapped {} open PRs for {}", prs.len(), repo_name);
        }
    } else {
        warn!(
            "gh pr list failed for {}: {}",
            repo_name,
            String::from_utf8_lossy(&pr_output.stderr).trim()
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the axum router with shared state.
pub fn create_router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/webhook", post(webhook_handler))
        .route("/register", post(register_handler))
        .route("/health", get(health_handler))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Dispatch execution
// ---------------------------------------------------------------------------

/// Dispatch action extracted from the orchestrator's `Action::Dispatch` variant.
pub struct DispatchAction {
    pub agent_type: String,
    pub issue_ref: String,
    pub task_context: String,
}

/// Execute a single dispatch action: validate agent, check fork gate, assemble
/// prompt, build command, and spawn the agent subprocess.
async fn execute_dispatch(
    state: &Arc<ServerState>,
    dispatch: &DispatchAction,
    event_payload: &serde_json::Value,
    repo_full_name: &str,
) -> Result<(), String> {
    // 0. Check rate limiter before dispatching.
    if state.rate_limiter.is_dispatch_paused() {
        return Err("Rate limited: dispatch paused".into());
    }

    // 1. Resolve the repo's local path from the registry.
    let registry = state.registry.read().await;
    let entry = registry
        .get(repo_full_name)
        .ok_or_else(|| format!("Repo {} not in registry", repo_full_name))?;
    let repo_root = Path::new(&entry.local_path);

    // 2. Validate agent_type against .githubclaw/agents/ (or built-in defaults).
    let agent_def = load_agent_definition(repo_root, &dispatch.agent_type)?;

    // 3. Check the fork PR gate — block execution-capable agents on unapproved fork PRs.
    if !check_fork_pr_gate(event_payload, &dispatch.agent_type) {
        return Err(format!(
            "Fork PR gate blocked dispatch of agent '{}' for {}",
            dispatch.agent_type, repo_full_name,
        ));
    }

    // 4. Check concurrency capacity — return a special error so the drain
    //    loop knows to wait instead of nacking.
    if !state.process_manager.has_capacity().await {
        return Err(format!(
            "CAPACITY_FULL: No concurrency capacity for agent '{}' — {} agents already running",
            dispatch.agent_type, state.process_manager.max_concurrent_agents,
        ));
    }

    // 5. Assemble the 4-layer prompt.
    //    IMPORTANT: We must NOT drop the assembler here because its Drop impl
    //    deletes the temp file. The agent subprocess reads the file asynchronously
    //    after we spawn it. We leak the assembler and schedule cleanup after the
    //    agent process exits.
    let mut assembler = PromptAssembler::new(repo_root);
    let prompt_file = assembler
        .assemble(&agent_def, &dispatch.task_context)
        .map_err(|e| format!("Prompt assembly failed: {}", e))?;
    let prompt_file_for_cleanup = prompt_file.clone();
    // Prevent Drop from deleting the temp file — we'll clean up after agent exits.
    std::mem::forget(assembler);

    // 6. Build command + env via AgentSpawner.
    let spawner = AgentSpawner::new(repo_root, crate::constants::DEFAULT_AGENT_MAX_TURNS);
    let cmd_parts = spawner.build_command(&agent_def, &prompt_file, &dispatch.task_context)?;
    let env_map = spawner.build_env(&agent_def, &prompt_file, &dispatch.task_context, None);

    // 7. Spawn via tokio::process::Command.
    if cmd_parts.is_empty() {
        return Err("build_command returned empty command".into());
    }
    let program = &cmd_parts[0];
    let args = &cmd_parts[1..];

    info!(
        agent_type = %dispatch.agent_type,
        repo = %repo_full_name,
        issue_ref = %dispatch.issue_ref,
        "Spawning agent: {} {}",
        program,
        args.join(" "),
    );

    let child = tokio::process::Command::new(program)
        .args(args)
        .envs(&env_map)
        .current_dir(repo_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn agent '{}': {}", dispatch.agent_type, e))?;

    let pid = child.id().unwrap_or(0);
    info!(
        pid,
        agent_type = %dispatch.agent_type,
        repo = %repo_full_name,
        "Agent spawned successfully",
    );

    // 8. Register with ProcessManager for monitoring.
    let label = format!("{}/{}", dispatch.agent_type, dispatch.issue_ref);
    state
        .process_manager
        .register(
            pid,
            crate::process_manager::ProcessKind::Worker,
            repo_full_name,
            &label,
            crate::constants::DEFAULT_PROCESS_TIMEOUT_SECONDS,
        )
        .await;

    info!(
        pid,
        agent_type = %dispatch.agent_type,
        "Agent registered with process manager (pid={})",
        pid,
    );

    let process_manager = Arc::clone(&state.process_manager);
    let agent_type = dispatch.agent_type.clone();
    let repo_name = repo_full_name.to_string();
    tokio::spawn(async move {
        match child.wait_with_output().await {
            Ok(output) => {
                let exit_code = output.status.code().unwrap_or(1);
                process_manager.report_exit(pid, exit_code).await;

                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

                if output.status.success() {
                    info!(
                        pid,
                        agent_type = %agent_type,
                        repo = %repo_name,
                        stdout = %stdout,
                        "Agent exited successfully",
                    );
                } else {
                    warn!(
                        pid,
                        agent_type = %agent_type,
                        repo = %repo_name,
                        exit_code,
                        stdout = %stdout,
                        stderr = %stderr,
                        "Agent exited with failure",
                    );
                }
            }
            Err(e) => {
                process_manager.report_exit(pid, 1).await;
                warn!(
                    pid,
                    agent_type = %agent_type,
                    repo = %repo_name,
                    error = %e,
                    "Failed while waiting for agent process",
                );
            }
        }
        // Clean up the temp prompt file now that the agent has exited.
        if let Err(e) = std::fs::remove_file(&prompt_file_for_cleanup) {
            debug!(
                "Failed to clean up prompt file {:?}: {}",
                prompt_file_for_cleanup, e
            );
        }
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Event drain loop
// ---------------------------------------------------------------------------

/// Start background event-processing tasks for every registered repo.
///
/// Call this once after constructing the server state. Each repo gets its own
/// `tokio::spawn`'d drain loop.
pub async fn start_event_processing(state: Arc<ServerState>) {
    // Start the rate limiter recovery probe in the background.
    let rate_limiter = state.rate_limiter.clone();
    tokio::spawn(async move {
        rate_limiter.start_recovery_probe().await;
    });

    let registry = registry_snapshot(&state).await;
    for (repo_name, entry) in &registry {
        start_repo_processing(state.clone(), repo_name, entry).await;
    }
    info!("Event processing started for {} repos", registry.len());
}

/// Per-repo drain loop: peek the queue, send to orchestrator, execute actions.
async fn event_drain_loop(state: &Arc<ServerState>, repo_name: &str, _entry: &RegistryEntry) {
    let _repo_slug = repo_name.replace('/', "-");
    info!(repo = %repo_name, "Starting event drain loop");

    loop {
        if state.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            info!(
                "Shutdown signal received, stopping drain loop for {}",
                repo_name
            );
            break;
        }
        // 1. Peek the queue for the next event.
        let peeked = {
            let mut queues = state.queues.lock().await;
            let registry = registry_snapshot(state).await;
            let queue = match get_or_create_queue(
                &mut queues,
                &registry,
                &state.githubclaw_home,
                repo_name,
            ) {
                Ok(q) => q,
                Err(e) => {
                    error!(repo = %repo_name, "Failed to get queue: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            match queue.peek() {
                Ok(Some(event)) => Some(event),
                Ok(None) => None,
                Err(e) => {
                    error!(repo = %repo_name, "Failed to peek queue: {}", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    continue;
                }
            }
        };

        let event = match peeked {
            Some(ev) => ev,
            None => {
                // Queue is empty — sleep and try again.
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }
        };

        // 2. Check rate limiter before sending to orchestrator.
        if state.rate_limiter.is_orchestrator_paused() {
            tracing::warn!("Rate limited (orchestrator paused), sleeping...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            continue;
        }

        // 3. Send event to orchestrator (in-process).
        let event_json = serde_json::to_string(&event.payload).unwrap_or_default();

        // Get or create the orchestrator session for this repo.
        let response_text = {
            let mut orchestrators = state.orchestrators.lock().await;
            let registry = state.registry.read().await;
            let session = orchestrators
                .entry(repo_name.to_string())
                .or_insert_with(|| {
                    let entry = registry.get(repo_name).unwrap();
                    OrchestratorSession::new(
                        repo_name,
                        &entry.local_path,
                        state.orchestrator_backend.clone(),
                        None,
                        None,
                    )
                });
            match session.process_event(&event_json).await {
                Ok(text) => text,
                Err(e) => {
                    warn!(
                        repo = %repo_name,
                        seq = event.sequence,
                        "Orchestrator failed to process event: {}. Will nack and retry.",
                        e,
                    );
                    state.rate_limiter.report_orchestrator_rate_limit();
                    // Drop the lock before sleeping.
                    drop(orchestrators);
                    nack_head_event(state, repo_name, &event.filename).await;
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            }
        };

        // 4. Parse the ActionList response.
        let action_list = OrchestratorSession::extract_action_list(&response_text);
        if let Err(e) = action_list.validate() {
            warn!(
                repo = %repo_name,
                seq = event.sequence,
                "Invalid ActionList from orchestrator: {}",
                e,
            );
            nack_head_event(state, repo_name, &event.filename).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            continue;
        }

        // 4. Execute each action.
        let mut all_ok = true;
        let mut capacity_wait = false;
        for action in &action_list.actions {
            match action {
                Action::Dispatch {
                    agent_type,
                    issue_ref,
                    task_context,
                } => {
                    let dispatch = DispatchAction {
                        agent_type: agent_type.clone(),
                        issue_ref: issue_ref.clone(),
                        task_context: task_context.clone(),
                    };
                    if let Err(e) =
                        execute_dispatch(state, &dispatch, &event.payload, repo_name).await
                    {
                        if e.starts_with("CAPACITY_FULL:") {
                            // Don't nack — just wait and retry later.
                            info!(
                                repo = %repo_name,
                                agent_type = %agent_type,
                                "Concurrency full, will retry after slot frees up",
                            );
                            capacity_wait = true;
                            break;
                        }
                        error!(
                            repo = %repo_name,
                            agent_type = %agent_type,
                            "Dispatch failed: {}",
                            e,
                        );
                        all_ok = false;
                    }
                }
                Action::ScheduleEvent {
                    event_id: _,
                    trigger_at,
                    repo,
                    payload,
                    context,
                } => match chrono::DateTime::parse_from_rfc3339(trigger_at) {
                    Ok(dt) => {
                        let mut scheduler = state.scheduler.lock().await;
                        let id = scheduler.create_event(
                            repo,
                            dt.with_timezone(&chrono::Utc),
                            payload.clone(),
                            true,
                            None,
                            context,
                        );
                        info!(
                            repo = %repo_name,
                            event_id = %id,
                            trigger_at = %trigger_at,
                            "Scheduled event created",
                        );
                    }
                    Err(e) => {
                        error!(
                            repo = %repo_name,
                            "Invalid trigger_at '{}': {}",
                            trigger_at,
                            e,
                        );
                        all_ok = false;
                    }
                },
                Action::CancelEvent { event_id } => {
                    let mut scheduler = state.scheduler.lock().await;
                    let cancelled = scheduler.cancel_event(event_id);
                    if cancelled {
                        info!(
                            repo = %repo_name,
                            event_id = %event_id,
                            "Scheduled event cancelled",
                        );
                    } else {
                        warn!(
                            repo = %repo_name,
                            event_id = %event_id,
                            "Cancel requested but event not found",
                        );
                    }
                }
                Action::NoAction { reasoning } => {
                    info!(
                        repo = %repo_name,
                        seq = event.sequence,
                        "No action: {}",
                        reasoning,
                    );
                }
            }
        }

        // 5. Handle result.
        if capacity_wait {
            // Concurrency full — don't dequeue, don't nack. Just sleep and
            // the event stays at the head of the queue for the next loop.
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
            continue;
        }

        if all_ok {
            let mut queues = state.queues.lock().await;
            let registry = registry_snapshot(state).await;
            if let Ok(q) =
                get_or_create_queue(&mut queues, &registry, &state.githubclaw_home, repo_name)
            {
                if let Err(e) = q.dequeue() {
                    error!(repo = %repo_name, "Failed to dequeue after success: {}", e);
                }
            }
        } else {
            nack_head_event(state, repo_name, &event.filename).await;
        }
    }
}

/// Helper: dequeue the head event and nack it (re-enqueue with incremented retry
/// or move to dead-letter if max retries exceeded).
async fn nack_head_event(state: &Arc<ServerState>, repo_name: &str, _filename: &str) {
    let mut queues = state.queues.lock().await;
    let registry = registry_snapshot(state).await;
    let queue = match get_or_create_queue(&mut queues, &registry, &state.githubclaw_home, repo_name)
    {
        Ok(q) => q,
        Err(e) => {
            error!(repo = %repo_name, "Failed to get queue for nack: {}", e);
            return;
        }
    };
    match queue.dequeue() {
        Ok(Some(mut event)) => {
            if let Err(e) = queue.nack(&mut event, "event") {
                error!(
                    repo = %repo_name,
                    seq = event.sequence,
                    "Failed to nack event: {}",
                    e,
                );
            }
        }
        Ok(None) => {
            warn!(repo = %repo_name, "Tried to nack but queue was empty");
        }
        Err(e) => {
            error!(repo = %repo_name, "Failed to dequeue for nack: {}", e);
        }
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use hmac::{Hmac, Mac};
    use http_body_util::BodyExt;
    use sha2::Sha256;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const TEST_SECRET: &str = "test-webhook-secret";

    /// Compute a valid HMAC SHA-256 signature for testing.
    fn sign_payload(payload: &[u8], secret: &str) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
        mac.update(payload);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    /// Build a ServerState with a single registered repo for testing.
    fn make_test_state(tmp: &TempDir) -> Arc<ServerState> {
        let mut registry = HashMap::new();
        registry.insert(
            "owner/repo".to_string(),
            RegistryEntry {
                local_path: tmp.path().to_string_lossy().to_string(),
                socket_path: String::new(),
            },
        );
        let scheduler_path = tmp.path().join(".githubclaw").join("scheduled.json");
        Arc::new(ServerState {
            webhook_secret: TEST_SECRET.to_string(),
            registry: RwLock::new(registry),
            started_repos: RwLock::new(HashSet::new()),
            queues: Mutex::new(HashMap::new()),
            githubclaw_home: tmp.path().to_path_buf(),
            process_manager: Arc::new(ProcessManager::new(
                crate::constants::DEFAULT_MAX_CONCURRENT_AGENTS,
            )),
            scheduler: Mutex::new(ScheduledEventManager::new(scheduler_path)),
            rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::default()),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            orchestrator_backend: crate::orchestrator::session::OrchestratorBackend::Codex,
            orchestrators: Mutex::new(HashMap::new()),
            registration_store: Mutex::new(
                RegistrationStore::load(&registration_store_path(tmp.path())).unwrap(),
            ),
        })
    }

    /// Build a minimal webhook payload JSON for a given repo.
    fn make_payload(repo_full_name: &str, action: &str) -> Value {
        serde_json::json!({
            "action": action,
            "repository": {
                "full_name": repo_full_name,
            },
        })
    }

    /// Helper: read the full response body as a string.
    async fn body_string(response: axum::http::Response<Body>) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn seed_claim_proof(
        state: &Arc<ServerState>,
        proof: &str,
        installation_id: u64,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) {
        let mut store = state.registration_store.lock().await;
        store.seed_claim_proof_for_test(proof, installation_id, expires_at);
        store.persist().unwrap();
    }

    // ---------------------------------------------------------------
    // 1. Health endpoint returns 200 with status ok
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_health_returns_200_with_status_ok() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["registered_repos"], 1);
    }

    // ---------------------------------------------------------------
    // 2. Webhook without signature returns 403
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_without_signature_returns_403() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---------------------------------------------------------------
    // 3. Webhook with invalid signature returns 403
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_with_invalid_signature_returns_403() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header(
                        "X-Hub-Signature-256",
                        "sha256=0000000000000000000000000000000000000000000000000000000000000000",
                    )
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    // ---------------------------------------------------------------
    // 4. Webhook with valid signature for registered repo returns 202
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_valid_signature_registered_repo_returns_202() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();
        let signature = sign_payload(&payload, TEST_SECRET);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header("X-Hub-Signature-256", &signature)
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let text = body_string(response).await;
        assert_eq!(text, "Event queued");
    }

    // ---------------------------------------------------------------
    // 5. Webhook for unregistered repo returns 200 (ignored)
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_unregistered_repo_returns_200() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("other/repo", "opened")).unwrap();
        let signature = sign_payload(&payload, TEST_SECRET);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header("X-Hub-Signature-256", &signature)
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let text = body_string(response).await;
        assert!(text.contains("Ignored"));
    }

    // ---------------------------------------------------------------
    // 6. annotate_fork_status adds flag for unapproved fork PR
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_adds_flag_for_unapproved_fork() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "labels": [],
                "head": {
                    "repo": {
                        "fork": true,
                        "full_name": "attacker/repo",
                    }
                }
            }
        });

        let result = annotate_fork_status(payload);
        assert_eq!(result["_githubclaw_fork_unapproved"], true);
    }

    // ---------------------------------------------------------------
    // 7. annotate_fork_status does NOT add flag for non-fork PR
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_no_flag_for_non_fork() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 10,
                "labels": [],
                "head": {
                    "repo": {
                        "fork": false,
                        "full_name": "owner/repo",
                    }
                }
            }
        });

        let result = annotate_fork_status(payload);
        assert!(result.get("_githubclaw_fork_unapproved").is_none());
    }

    // ---------------------------------------------------------------
    // 8. annotate_fork_status does NOT add flag when label present
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_no_flag_when_approved_label() {
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 42,
                "labels": [
                    { "name": "githubclaw-approved" }
                ],
                "head": {
                    "repo": {
                        "fork": true,
                        "full_name": "contributor/repo",
                    }
                }
            }
        });

        let result = annotate_fork_status(payload);
        assert!(result.get("_githubclaw_fork_unapproved").is_none());
    }

    // ---------------------------------------------------------------
    // 9. load_registry parses nested format
    // ---------------------------------------------------------------
    #[test]
    fn test_load_registry_parses_nested_format() {
        let tmp = TempDir::new().unwrap();
        let registry_path = tmp.path().join("registry.json");
        std::fs::write(
            &registry_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "repos": {
                    "owner/repo": {
                        "local_path": "/home/user/repo",
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let registry = load_registry(&registry_path);
        assert_eq!(registry.len(), 1);
        assert!(registry.contains_key("owner/repo"));
        assert_eq!(registry["owner/repo"].local_path, "/home/user/repo");
    }

    // ---------------------------------------------------------------
    // 9b. load_registry parses flat format
    // ---------------------------------------------------------------
    #[test]
    fn test_load_registry_parses_flat_format() {
        let tmp = TempDir::new().unwrap();
        let registry_path = tmp.path().join("registry.json");
        std::fs::write(
            &registry_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "owner/repo": {
                    "local_path": "/home/user/repo",
                },
                "org/other": {
                    "local_path": "/home/user/other",
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let registry = load_registry(&registry_path);
        assert_eq!(registry.len(), 2);
        assert!(registry.contains_key("owner/repo"));
        assert!(registry.contains_key("org/other"));
    }

    // ---------------------------------------------------------------
    // 9c. load_registry returns empty for missing file
    // ---------------------------------------------------------------
    #[test]
    fn test_load_registry_returns_empty_for_missing_file() {
        let registry = load_registry(Path::new("/nonexistent/registry.json"));
        assert!(registry.is_empty());
    }

    // ---------------------------------------------------------------
    // 10. load_webhook_secret reads and trims file
    // ---------------------------------------------------------------
    #[test]
    fn test_load_webhook_secret_reads_and_trims() {
        let tmp = TempDir::new().unwrap();
        let secret_path = tmp.path().join("webhook_secret");
        std::fs::write(&secret_path, "  my-secret-value\n  ").unwrap();

        let secret = load_webhook_secret(&secret_path).unwrap();
        assert_eq!(secret, "my-secret-value");
    }

    // ---------------------------------------------------------------
    // 10b. load_webhook_secret returns error for missing file
    // ---------------------------------------------------------------
    #[test]
    fn test_load_webhook_secret_error_for_missing_file() {
        let result = load_webhook_secret(Path::new("/nonexistent/secret"));
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // 11. Webhook enqueues event with correct label
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_webhook_enqueues_with_correct_event_label() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let state_clone = Arc::clone(&state);
        let app = create_router(state);

        let payload = serde_json::to_vec(&make_payload("owner/repo", "opened")).unwrap();
        let signature = sign_payload(&payload, TEST_SECRET);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .header("X-Github-Event", "issues")
                    .header("X-Hub-Signature-256", &signature)
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);

        // Verify the queue has one event
        let queues = state_clone.queues.lock().await;
        let queue = queues.get("owner/repo").unwrap();
        assert_eq!(queue.size(), 1);
    }

    #[tokio::test]
    async fn test_register_rejects_missing_auth_fields() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://agent.example.com"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn test_register_rejects_both_auth_fields_present() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://agent.example.com",
                            "claim_proof": "cp_test",
                            "update_secret": "us_test"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "invalid_request");
    }

    #[tokio::test]
    async fn test_register_rejects_unsafe_tunnel_urls() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        seed_claim_proof(
            &state,
            "cp_test",
            123,
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .await;
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "http://localhost:8080/path",
                            "claim_proof": "cp_test"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "invalid_tunnel_url");
    }

    #[tokio::test]
    async fn test_register_claim_returns_201_and_persists_mapping() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        seed_claim_proof(
            &state,
            "cp_claim_ok",
            123,
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .await;
        let app = create_router(Arc::clone(&state));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://Agent.Example.com:8443/",
                            "claim_proof": "cp_claim_ok"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["status"], "claimed");
        assert_eq!(body["installation_id"], 123);
        assert_eq!(body["tunnel_url"], "https://agent.example.com:8443");
        assert_eq!(body["rotated"], false);
        assert!(body["update_secret"].as_str().unwrap().starts_with("us_"));

        let persisted = RegistrationStore::load(&registration_store_path(tmp.path())).unwrap();
        let record = persisted.installations.get("123").unwrap();
        assert_eq!(record.tunnel_url, "https://agent.example.com:8443");
        let claim = persisted
            .claim_proofs
            .get(&hash_secret("cp_claim_ok"))
            .unwrap();
        assert!(claim.used_at.is_some());
    }

    #[tokio::test]
    async fn test_register_rejects_reused_claim_proof_after_success() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        seed_claim_proof(
            &state,
            "cp_once",
            123,
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .await;
        let app = create_router(state);

        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://one.example.com",
                            "claim_proof": "cp_once"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);

        let second = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://two.example.com",
                            "claim_proof": "cp_once"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(second.status(), StatusCode::FORBIDDEN);
        let body: Value = serde_json::from_str(&body_string(second).await).unwrap();
        assert_eq!(body["error"]["code"], "claim_proof_already_used");
    }

    #[tokio::test]
    async fn test_register_rejects_expired_claim_proof() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        seed_claim_proof(
            &state,
            "cp_expired",
            123,
            chrono::Utc::now() - chrono::Duration::seconds(1),
        )
        .await;
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://expired.example.com",
                            "claim_proof": "cp_expired"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "expired_claim_proof");
    }

    #[tokio::test]
    async fn test_register_update_rotates_secret_and_invalidates_old_secret() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        seed_claim_proof(
            &state,
            "cp_rotate",
            123,
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .await;
        let app = create_router(state);

        let claim = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://first.example.com",
                            "claim_proof": "cp_rotate"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let claim_body: Value = serde_json::from_str(&body_string(claim).await).unwrap();
        let first_secret = claim_body["update_secret"].as_str().unwrap().to_string();

        let update = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://second.example.com",
                            "update_secret": first_secret
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(update.status(), StatusCode::OK);
        let update_body: Value = serde_json::from_str(&body_string(update).await).unwrap();
        assert_eq!(update_body["status"], "updated");
        assert_eq!(update_body["rotated"], true);
        let second_secret = update_body["update_secret"].as_str().unwrap().to_string();

        let stale = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://third.example.com",
                            "update_secret": first_secret
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(stale.status(), StatusCode::FORBIDDEN);
        let stale_body: Value = serde_json::from_str(&body_string(stale).await).unwrap();
        assert_eq!(stale_body["error"]["code"], "invalid_update_secret");
        assert_ne!(second_secret, "");
    }

    #[tokio::test]
    async fn test_register_returns_ownership_mismatch_for_wrong_installation_secret() {
        let tmp = TempDir::new().unwrap();
        let state = make_test_state(&tmp);
        seed_claim_proof(
            &state,
            "cp_owner",
            123,
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .await;
        let app = create_router(state);

        let claim = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 123,
                            "tunnel_url": "https://owned.example.com",
                            "claim_proof": "cp_owner"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let claim_body: Value = serde_json::from_str(&body_string(claim).await).unwrap();
        let secret = claim_body["update_secret"].as_str().unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/register")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "installation_id": 999,
                            "tunnel_url": "https://wrong.example.com",
                            "update_secret": secret
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: Value = serde_json::from_str(&body_string(response).await).unwrap();
        assert_eq!(body["error"]["code"], "ownership_mismatch");
    }

    // ---------------------------------------------------------------
    // 12. annotate_fork_status passes through non-PR payloads
    // ---------------------------------------------------------------
    #[test]
    fn test_annotate_fork_status_passes_through_non_pr_payload() {
        let payload = serde_json::json!({
            "action": "created",
            "issue": { "number": 5 },
        });

        let result = annotate_fork_status(payload.clone());
        assert_eq!(result, payload);
        assert!(result.get("_githubclaw_fork_unapproved").is_none());
    }

    // ---------------------------------------------------------------
    // 13. execute_dispatch returns error for unknown agent_type
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_execute_dispatch_rejects_unknown_agent_type() {
        let tmp = TempDir::new().unwrap();
        // Create the .githubclaw/agents dir but don't add agent defs.
        std::fs::create_dir_all(tmp.path().join(".githubclaw").join("agents")).unwrap();

        let state = make_test_state(&tmp);
        let dispatch = DispatchAction {
            agent_type: "nonexistent_agent".to_string(),
            issue_ref: "owner/repo#1".to_string(),
            task_context: "Do something".to_string(),
        };

        let event_payload = serde_json::json!({
            "action": "opened",
            "repository": { "full_name": "owner/repo" },
        });

        let result = execute_dispatch(&state, &dispatch, &event_payload, "owner/repo").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("No agent definition found"),
            "Expected 'No agent definition found' but got: {}",
            err,
        );
    }

    // ---------------------------------------------------------------
    // 14. execute_dispatch blocks unapproved fork PR
    // ---------------------------------------------------------------
    #[tokio::test]
    async fn test_execute_dispatch_blocks_unapproved_fork_pr() {
        let tmp = TempDir::new().unwrap();
        // Create a valid agent definition so we get past the agent validation step.
        let agents_dir = tmp.path().join(".githubclaw").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("coder.md"),
            "---\nbackend: codex\n---\n\nDo work.\n",
        )
        .unwrap();

        let state = make_test_state(&tmp);
        let dispatch = DispatchAction {
            agent_type: "coder".to_string(),
            issue_ref: "owner/repo#42".to_string(),
            task_context: "Fix the bug".to_string(),
        };

        // Fork PR without approved label — should be blocked.
        let event_payload = serde_json::json!({
            "action": "opened",
            "repository": { "full_name": "owner/repo" },
            "pull_request": {
                "number": 42,
                "labels": [],
                "head": {
                    "repo": {
                        "fork": true,
                        "full_name": "attacker/repo",
                    }
                }
            }
        });

        let result = execute_dispatch(&state, &dispatch, &event_payload, "owner/repo").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("Fork PR gate blocked"),
            "Expected 'Fork PR gate blocked' but got: {}",
            err,
        );
    }
}
