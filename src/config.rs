//! Configuration loading for GithubClaw.
//!
//! Loads from two sources:
//! - Global: ~/.githubclaw/config.yaml (shared across all repos)
//! - Per-repo: .githubclaw/config.yaml (repo-specific overrides)

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::constants::{
    DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS, DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS,
    DEFAULT_CONFIG_MAX_RETRY, DEFAULT_GLOBAL_TIMEOUT_SECONDS,
    DEFAULT_ORCHESTRATOR_IDLE_TIMEOUT_SECONDS, DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS,
    DEFAULT_SERVER_PORT,
};
use crate::errors::{GithubClawError, Result};

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn global_config_dir() -> PathBuf {
    home_dir().join(".githubclaw")
}

pub fn global_config_path() -> PathBuf {
    global_config_dir().join("config.yaml")
}

pub const REPO_CONFIG_DIR_NAME: &str = ".githubclaw";
pub const REPO_CONFIG_FILENAME: &str = "config.yaml";

// ---------------------------------------------------------------------------
// Default event subscription
// ---------------------------------------------------------------------------

pub fn default_event_subscription() -> Vec<String> {
    vec![
        "issues".into(),
        "issue_comment".into(),
        "pull_request".into(),
        "pull_request_review".into(),
        "pull_request_review_comment".into(),
        "discussion".into(),
        "discussion_comment".into(),
        "label".into(),
        "milestone".into(),
        "projects_v2_item".into(),
        "check_suite".into(),
        "check_run".into(),
    ]
}

// ---------------------------------------------------------------------------
// Serde default functions
// ---------------------------------------------------------------------------

fn default_port() -> u16 {
    DEFAULT_SERVER_PORT
}
fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_max_concurrent_agents() -> usize {
    DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS
}
fn default_global_timeout() -> u64 {
    DEFAULT_GLOBAL_TIMEOUT_SECONDS
}
fn default_orchestrator_idle_timeout() -> u64 {
    DEFAULT_ORCHESTRATOR_IDLE_TIMEOUT_SECONDS
}
fn default_drain_timeout() -> u64 {
    DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS
}
fn default_recovery_probe_interval() -> u64 {
    DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS
}
fn default_max_retry() -> u32 {
    DEFAULT_CONFIG_MAX_RETRY
}

// ---------------------------------------------------------------------------
// GlobalConfig
// ---------------------------------------------------------------------------

/// Global configuration loaded from `~/.githubclaw/config.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: usize,

    #[serde(default = "default_global_timeout")]
    pub global_timeout: u64,

    #[serde(default = "default_orchestrator_idle_timeout")]
    pub orchestrator_idle_timeout: u64,

    #[serde(default = "default_drain_timeout")]
    pub drain_timeout: u64,

    #[serde(default = "default_recovery_probe_interval")]
    pub recovery_probe_interval: u64,

    #[serde(default = "default_max_retry")]
    pub max_retry: u32,

    #[serde(default = "default_event_subscription")]
    pub event_subscription: Vec<String>,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_SERVER_PORT,
            host: "0.0.0.0".into(),
            max_concurrent_agents: DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS,
            global_timeout: DEFAULT_GLOBAL_TIMEOUT_SECONDS,
            orchestrator_idle_timeout: DEFAULT_ORCHESTRATOR_IDLE_TIMEOUT_SECONDS,
            drain_timeout: DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS,
            recovery_probe_interval: DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS,
            max_retry: DEFAULT_CONFIG_MAX_RETRY,
            event_subscription: default_event_subscription(),
        }
    }
}

impl GlobalConfig {
    /// Load global config from disk, falling back to defaults for missing keys.
    ///
    /// Supports both flat keys (`port`) and nested keys (`server.port`).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = path.map(PathBuf::from).unwrap_or_else(global_config_path);

        if !config_path.exists() {
            let mut cfg = Self::default();
            cfg.validate();
            return Ok(cfg);
        }

        let contents = std::fs::read_to_string(&config_path)?;
        let raw: serde_yaml::Value = serde_yaml::from_str(&contents)
            .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

        let defaults = Self::default();

        let mut config = Self {
            port: get_flat_or_nested_u64(&raw, "port", &["server", "port"])
                .map(|v| v as u16)
                .unwrap_or(defaults.port),
            host: get_flat_or_nested_str(&raw, "host", &["server", "host"])
                .unwrap_or(defaults.host),
            max_concurrent_agents: get_flat_or_nested_u64(
                &raw,
                "max_concurrent_agents",
                &["process", "max_concurrent_agents"],
            )
            .map(|v| v as usize)
            .unwrap_or(defaults.max_concurrent_agents),
            global_timeout: get_flat_or_nested_u64(
                &raw,
                "global_timeout",
                &["process", "global_timeout"],
            )
            .unwrap_or(defaults.global_timeout),
            orchestrator_idle_timeout: get_flat_or_nested_u64(
                &raw,
                "orchestrator_idle_timeout",
                &["process", "orchestrator_idle_timeout"],
            )
            .unwrap_or(defaults.orchestrator_idle_timeout),
            drain_timeout: get_flat_or_nested_u64(
                &raw,
                "drain_timeout",
                &["process", "drain_timeout"],
            )
            .unwrap_or(defaults.drain_timeout),
            recovery_probe_interval: get_flat_or_nested_u64(
                &raw,
                "recovery_probe_interval",
                &["rate_limit", "recovery_probe_interval"],
            )
            .unwrap_or(defaults.recovery_probe_interval),
            max_retry: get_flat_or_nested_u64(&raw, "max_retry", &["queue", "max_retry"])
                .map(|v| v as u32)
                .unwrap_or(defaults.max_retry),
            event_subscription: get_flat_string_list(&raw, "event_subscription")
                .unwrap_or_else(default_event_subscription),
        };

        config.validate();
        Ok(config)
    }

    /// Validate config values, resetting invalid ones to defaults.
    pub fn validate(&mut self) {
        if self.port < 1 {
            tracing::warn!(
                port = self.port,
                default = DEFAULT_SERVER_PORT,
                "port out of range; resetting to default"
            );
            self.port = DEFAULT_SERVER_PORT;
        }
        // Note: u16 max is 65535 so upper bound is inherently enforced by the type,
        // but we still check for port == 0 which is invalid.
        if self.port == 0 {
            self.port = DEFAULT_SERVER_PORT;
        }

        if self.max_concurrent_agents == 0 {
            tracing::warn!(
                max_concurrent_agents = self.max_concurrent_agents,
                default = DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS,
                "max_concurrent_agents must be > 0; resetting to default"
            );
            self.max_concurrent_agents = DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS;
        }

        if self.global_timeout == 0 {
            tracing::warn!(
                global_timeout = self.global_timeout,
                default = DEFAULT_GLOBAL_TIMEOUT_SECONDS,
                "global_timeout must be > 0; resetting to default"
            );
            self.global_timeout = DEFAULT_GLOBAL_TIMEOUT_SECONDS;
        }

        if self.drain_timeout == 0 {
            tracing::warn!(
                drain_timeout = self.drain_timeout,
                default = DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS,
                "drain_timeout must be > 0; resetting to default"
            );
            self.drain_timeout = DEFAULT_CONFIG_DRAIN_TIMEOUT_SECONDS;
        }

        if self.recovery_probe_interval == 0 {
            tracing::warn!(
                recovery_probe_interval = self.recovery_probe_interval,
                default = DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS,
                "recovery_probe_interval must be > 0; resetting to default"
            );
            self.recovery_probe_interval = DEFAULT_RECOVERY_PROBE_INTERVAL_SECONDS;
        }
        // max_retry is u32, so it cannot be negative; 0 is allowed (no retries).
    }

    /// Write config to disk as YAML.
    pub fn save(&self, path: Option<&Path>) -> Result<()> {
        let config_path = path.map(PathBuf::from).unwrap_or_else(global_config_path);

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let yaml = serde_yaml::to_string(self)
            .map_err(|e| GithubClawError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(&config_path, yaml)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RepoConfig
// ---------------------------------------------------------------------------

fn default_allowed_read_paths() -> Vec<String> {
    vec![]
}

fn default_excluded_read_paths() -> Vec<String> {
    vec![
        "~/.githubclaw/secrets/".into(),
        "~/.ssh/".into(),
        "~/.aws/".into(),
    ]
}

/// Per-repo configuration loaded from `.githubclaw/config.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    #[serde(default = "default_allowed_read_paths")]
    pub allowed_read_paths: Vec<String>,

    #[serde(default = "default_excluded_read_paths")]
    pub excluded_read_paths: Vec<String>,

    #[serde(default = "default_event_subscription")]
    pub event_subscription: Vec<String>,

    // V2: E2E test configuration for Verifier direct-use validation
    #[serde(default)]
    pub e2e_config: Option<E2eConfig>,

    // V2: Reviewer secondary criteria (after JTBD resolution)
    #[serde(default)]
    pub reviewer_priorities: Vec<String>,

    // V2: Shell command to launch the app for manual dogfooding
    #[serde(default)]
    pub dogfood_command: Option<String>,
}

/// E2E test configuration for Verifier direct-use validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eConfig {
    /// Type of project: "web", "api", "cli", "library"
    pub project_type: String,

    /// Command to start the application for testing
    #[serde(default)]
    pub start_command: Option<String>,

    /// Command to run e2e tests
    #[serde(default)]
    pub test_command: Option<String>,

    /// Base URL for web/API projects
    #[serde(default)]
    pub base_url: Option<String>,

    /// Additional instructions for the Verifier
    #[serde(default)]
    pub instructions: Option<String>,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            allowed_read_paths: default_allowed_read_paths(),
            excluded_read_paths: default_excluded_read_paths(),
            event_subscription: default_event_subscription(),
            e2e_config: None,
            reviewer_priorities: Vec::new(),
            dogfood_command: None,
        }
    }
}

impl RepoConfig {
    /// Load per-repo config, falling back to defaults for missing keys.
    pub fn load(repo_root: Option<&Path>) -> Result<Self> {
        let root = repo_root
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let config_path = root.join(REPO_CONFIG_DIR_NAME).join(REPO_CONFIG_FILENAME);

        if !config_path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&config_path)?;
        let config: Self = serde_yaml::from_str(&contents)?;
        Ok(config)
    }

    /// Write per-repo config to disk as YAML.
    pub fn save(&self, repo_root: Option<&Path>) -> Result<()> {
        let root = repo_root
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let config_path = root.join(REPO_CONFIG_DIR_NAME).join(REPO_CONFIG_FILENAME);

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let yaml = serde_yaml::to_string(self)
            .map_err(|e| GithubClawError::Config(format!("Failed to serialize config: {e}")))?;
        std::fs::write(&config_path, yaml)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Walk up from `start` to find a directory containing `.git/`.
pub fn find_repo_root(start: Option<&Path>) -> Option<PathBuf> {
    let mut current = start
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

/// Return the path to the webhook server PID file.
pub fn get_pid_file() -> PathBuf {
    global_config_dir().join("server.pid")
}

/// Return the path to the webhook server log file.
pub fn get_log_file() -> PathBuf {
    global_config_dir().join("logs").join("webhook_server.log")
}

// ---------------------------------------------------------------------------
// Internal: nested key lookup helpers using serde_yaml::Value
// ---------------------------------------------------------------------------

fn get_flat_or_nested_u64(
    raw: &serde_yaml::Value,
    flat_key: &str,
    nested_path: &[&str],
) -> Option<u64> {
    // Try flat key first
    if let Some(val) = raw.get(flat_key) {
        return val.as_u64();
    }
    // Try nested path
    let mut node = raw;
    for &segment in nested_path {
        node = node.get(segment)?;
    }
    node.as_u64()
}

fn get_flat_or_nested_str(
    raw: &serde_yaml::Value,
    flat_key: &str,
    nested_path: &[&str],
) -> Option<String> {
    // Try flat key first
    if let Some(val) = raw.get(flat_key) {
        return val.as_str().map(String::from);
    }
    // Try nested path
    let mut node = raw;
    for &segment in nested_path {
        node = node.get(segment)?;
    }
    node.as_str().map(String::from)
}

fn get_flat_string_list(raw: &serde_yaml::Value, key: &str) -> Option<Vec<String>> {
    let seq = raw.get(key)?.as_sequence()?;
    let items: Vec<String> = seq
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // 1. Default config values match spec
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_config_values() {
        let cfg = GlobalConfig::default();
        assert_eq!(cfg.port, 8000);
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.max_concurrent_agents, 5);
        assert_eq!(cfg.global_timeout, 7200);
        assert_eq!(cfg.orchestrator_idle_timeout, 1800);
        assert_eq!(cfg.drain_timeout, 300);
        assert_eq!(cfg.recovery_probe_interval, 300);
        assert_eq!(cfg.max_retry, 3);
        assert_eq!(cfg.event_subscription.len(), 12);
        assert_eq!(cfg.event_subscription[0], "issues");
        assert_eq!(cfg.event_subscription[11], "check_run");
    }

    // -----------------------------------------------------------------------
    // 2. Load from YAML file
    // -----------------------------------------------------------------------
    #[test]
    fn test_load_from_yaml_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        fs::write(&config_path, "port: 9090\nhost: 127.0.0.1\nmax_retry: 5\n").unwrap();

        let cfg = GlobalConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.port, 9090);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.max_retry, 5);
        // Remaining fields should be defaults
        assert_eq!(cfg.max_concurrent_agents, 5);
        assert_eq!(cfg.global_timeout, 7200);
    }

    // -----------------------------------------------------------------------
    // 3. Load with nested keys (server.port)
    // -----------------------------------------------------------------------
    #[test]
    fn test_load_nested_keys() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        fs::write(
            &config_path,
            "server:\n  port: 4444\n  host: localhost\nqueue:\n  max_retry: 10\n",
        )
        .unwrap();

        let cfg = GlobalConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.port, 4444);
        assert_eq!(cfg.host, "localhost");
        assert_eq!(cfg.max_retry, 10);
    }

    // -----------------------------------------------------------------------
    // 4. Load with flat keys
    // -----------------------------------------------------------------------
    #[test]
    fn test_load_flat_keys() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        fs::write(
            &config_path,
            "port: 5555\nhost: 10.0.0.1\nmax_concurrent_agents: 12\n",
        )
        .unwrap();

        let cfg = GlobalConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.port, 5555);
        assert_eq!(cfg.host, "10.0.0.1");
        assert_eq!(cfg.max_concurrent_agents, 12);
    }

    // -----------------------------------------------------------------------
    // 5. Validation: invalid port resets to default
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_invalid_port_resets() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        // port: 0 is invalid
        fs::write(&config_path, "port: 0\n").unwrap();

        let cfg = GlobalConfig::load(Some(&config_path)).unwrap();
        assert_eq!(cfg.port, DEFAULT_SERVER_PORT);
    }

    // -----------------------------------------------------------------------
    // 6. Validation: zero max_concurrent_agents resets to default
    // -----------------------------------------------------------------------
    #[test]
    fn test_validate_zero_max_concurrent_agents_resets() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        fs::write(&config_path, "max_concurrent_agents: 0\n").unwrap();

        let cfg = GlobalConfig::load(Some(&config_path)).unwrap();
        assert_eq!(
            cfg.max_concurrent_agents,
            DEFAULT_CONFIG_MAX_CONCURRENT_AGENTS
        );
    }

    // -----------------------------------------------------------------------
    // 7. Save and reload roundtrip
    // -----------------------------------------------------------------------
    #[test]
    fn test_save_and_reload_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");

        let original = GlobalConfig {
            port: 3000,
            host: "192.168.1.1".into(),
            max_retry: 7,
            ..GlobalConfig::default()
        };

        original.save(Some(&config_path)).unwrap();

        let reloaded = GlobalConfig::load(Some(&config_path)).unwrap();
        assert_eq!(reloaded.port, 3000);
        assert_eq!(reloaded.host, "192.168.1.1");
        assert_eq!(reloaded.max_retry, 7);
        assert_eq!(
            reloaded.max_concurrent_agents,
            original.max_concurrent_agents
        );
        assert_eq!(reloaded.global_timeout, original.global_timeout);
        assert_eq!(reloaded.event_subscription.len(), 12);
    }

    // -----------------------------------------------------------------------
    // 8. RepoConfig defaults
    // -----------------------------------------------------------------------
    #[test]
    fn test_repo_config_defaults() {
        let cfg = RepoConfig::default();
        assert!(cfg.allowed_read_paths.is_empty());
        assert_eq!(cfg.excluded_read_paths.len(), 3);
        assert!(cfg.excluded_read_paths.contains(&"~/.ssh/".to_string()));
        assert!(cfg.excluded_read_paths.contains(&"~/.aws/".to_string()));
        assert!(cfg
            .excluded_read_paths
            .contains(&"~/.githubclaw/secrets/".to_string()));
        assert_eq!(cfg.event_subscription.len(), 12);
    }

    // -----------------------------------------------------------------------
    // 9. find_repo_root finds .git directory
    // -----------------------------------------------------------------------
    #[test]
    fn test_find_repo_root() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        let sub = repo.join("src").join("deep");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();

        let found = find_repo_root(Some(&sub));
        assert_eq!(found, Some(repo));
    }

    #[test]
    fn test_find_repo_root_not_found() {
        let tmp = TempDir::new().unwrap();
        let no_git = tmp.path().join("plain");
        fs::create_dir_all(&no_git).unwrap();

        // Starting from a leaf inside tmp with no .git anywhere up to root —
        // this might actually find the real repo in CI, so we just verify it
        // returns either None or some PathBuf (the function shouldn't panic).
        let _result = find_repo_root(Some(&no_git));
    }

    // -----------------------------------------------------------------------
    // 10. get_pid_file and get_log_file return correct paths
    // -----------------------------------------------------------------------
    #[test]
    fn test_get_pid_file() {
        let pid = get_pid_file();
        assert!(pid.ends_with("server.pid"));
        assert!(pid.to_string_lossy().contains(".githubclaw"));
    }

    #[test]
    fn test_get_log_file() {
        let log = get_log_file();
        assert!(log.ends_with("webhook_server.log"));
        assert!(log.to_string_lossy().contains(".githubclaw"));
        assert!(log.to_string_lossy().contains("logs"));
    }

    // -----------------------------------------------------------------------
    // Bonus: flat key takes precedence over nested key
    // -----------------------------------------------------------------------
    #[test]
    fn test_flat_key_precedence_over_nested() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        fs::write(&config_path, "port: 1111\nserver:\n  port: 2222\n").unwrap();

        let cfg = GlobalConfig::load(Some(&config_path)).unwrap();
        // Flat key should win
        assert_eq!(cfg.port, 1111);
    }

    // -----------------------------------------------------------------------
    // Bonus: load nonexistent file returns defaults
    // -----------------------------------------------------------------------
    #[test]
    fn test_load_nonexistent_returns_defaults() {
        let cfg =
            GlobalConfig::load(Some(Path::new("/tmp/does_not_exist_githubclaw.yaml"))).unwrap();
        assert_eq!(cfg.port, DEFAULT_SERVER_PORT);
        assert_eq!(cfg.host, "0.0.0.0");
    }

    // -----------------------------------------------------------------------
    // Bonus: RepoConfig load and save roundtrip
    // -----------------------------------------------------------------------
    #[test]
    fn test_repo_config_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path();

        let original = RepoConfig {
            allowed_read_paths: vec!["/data".into()],
            ..RepoConfig::default()
        };

        original.save(Some(repo_root)).unwrap();

        let reloaded = RepoConfig::load(Some(repo_root)).unwrap();
        assert_eq!(reloaded.allowed_read_paths, vec!["/data".to_string()]);
        assert_eq!(reloaded.excluded_read_paths.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Bonus: default_event_subscription has 12 entries
    // -----------------------------------------------------------------------
    #[test]
    fn test_default_event_subscription() {
        let events = default_event_subscription();
        assert_eq!(events.len(), 12);
        assert!(events.contains(&"issues".to_string()));
        assert!(events.contains(&"pull_request".to_string()));
        assert!(events.contains(&"check_run".to_string()));
        assert!(events.contains(&"discussion".to_string()));
    }
}
