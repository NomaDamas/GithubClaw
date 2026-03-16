//! Disk-backed idempotency receipts for worker dispatches.
//!
//! When an orchestrator event is replayed after a crash, the same `githubclaw
//! dispatch` command can be issued again. These receipts let the CLI detect
//! that an identical dispatch has already succeeded and skip re-running it.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchReceipt {
    pub key: String,
    pub event_id: String,
    pub agent_type: String,
    pub issue: u64,
    pub prompt_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedupe_suffix: Option<String>,
    pub created_at_unix_seconds: u64,
}

pub struct DispatchReceiptStore {
    dir: PathBuf,
}

impl DispatchReceiptStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            dir: repo_root.join(".githubclaw").join("dispatch_receipts"),
        }
    }

    pub fn key_for(
        event_id: &str,
        agent_type: &str,
        issue: u64,
        prompt: &str,
        dedupe_suffix: Option<&str>,
    ) -> String {
        let prompt_sha = sha256_hex(prompt.as_bytes());
        let mut hasher = Sha256::new();
        hasher.update(event_id.as_bytes());
        hasher.update([0]);
        hasher.update(agent_type.as_bytes());
        hasher.update([0]);
        hasher.update(issue.to_string().as_bytes());
        hasher.update([0]);
        hasher.update(prompt_sha.as_bytes());
        hasher.update([0]);
        hasher.update(dedupe_suffix.unwrap_or_default().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    pub fn has_receipt(&self, key: &str) -> bool {
        self.receipt_path(key).exists()
    }

    pub fn record_success(
        &self,
        event_id: &str,
        agent_type: &str,
        issue: u64,
        prompt: &str,
        dedupe_suffix: Option<&str>,
    ) -> std::io::Result<DispatchReceipt> {
        fs::create_dir_all(&self.dir)?;

        let receipt = DispatchReceipt {
            key: Self::key_for(event_id, agent_type, issue, prompt, dedupe_suffix),
            event_id: event_id.to_string(),
            agent_type: agent_type.to_string(),
            issue,
            prompt_sha256: sha256_hex(prompt.as_bytes()),
            dedupe_suffix: dedupe_suffix.map(ToString::to_string),
            created_at_unix_seconds: unix_timestamp_now(),
        };

        let path = self.receipt_path(&receipt.key);
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, serde_json::to_string_pretty(&receipt)?)?;
        fs::rename(&tmp_path, &path)?;
        Ok(receipt)
    }

    fn receipt_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn identical_inputs_produce_same_key() {
        let key1 = DispatchReceiptStore::key_for("evt-1", "implementer", 42, "Fix it", None);
        let key2 = DispatchReceiptStore::key_for("evt-1", "implementer", 42, "Fix it", None);
        assert_eq!(key1, key2);
    }

    #[test]
    fn dedupe_suffix_changes_key() {
        let key1 = DispatchReceiptStore::key_for("evt-1", "implementer", 42, "Fix it", None);
        let key2 =
            DispatchReceiptStore::key_for("evt-1", "implementer", 42, "Fix it", Some("second"));
        assert_ne!(key1, key2);
    }

    #[test]
    fn record_success_writes_receipt() {
        let tmp = TempDir::new().unwrap();
        let store = DispatchReceiptStore::new(tmp.path());
        let receipt = store
            .record_success("evt-1", "implementer", 42, "Fix it", None)
            .unwrap();

        assert!(store.has_receipt(&receipt.key));
    }
}
