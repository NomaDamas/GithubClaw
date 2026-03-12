//! Disk-persisted FIFO event queue with dead-letter support.
//!
//! Queue directory layout per repo:
//!     .githubclaw/queue/
//!         000001_issues_opened.json
//!         000002_issue_comment_created.json
//!         ...
//!     .githubclaw/queue/dead/
//!         000001_issues_opened_dead.json
//!         ...

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::constants::QUEUE_FILENAME_LABEL_MAX_LENGTH;

/// An event stored on disk inside the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedEvent {
    pub sequence: u64,
    #[serde(skip)]
    pub filename: String,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub retry_count: u32,
}

/// FIFO event queue persisted to a per-repo directory on disk.
///
/// Events are written as sequentially-numbered JSON files. Dequeue reads the
/// lowest-numbered file, processes it, and removes it. Events that exceed
/// `max_retry` are moved to a `dead/` subdirectory.
pub struct DiskPersistedQueue {
    queue_dir: PathBuf,
    dead_dir: PathBuf,
    max_retry: u32,
}

impl DiskPersistedQueue {
    /// Create a new queue, ensuring directories exist and cleaning stale `.tmp` files.
    pub fn new(queue_dir: impl AsRef<Path>, max_retry: u32) -> std::io::Result<Self> {
        let queue_dir = queue_dir.as_ref().to_path_buf();
        let dead_dir = queue_dir.join("dead");

        fs::create_dir_all(&queue_dir)?;
        fs::create_dir_all(&dead_dir)?;

        // Clean up stale .tmp files left by crashes between write and rename.
        if let Ok(entries) = fs::read_dir(&queue_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("tmp") && path.is_file() {
                    let _ = fs::remove_file(&path);
                }
            }
        }

        Ok(Self {
            queue_dir,
            dead_dir,
            max_retry,
        })
    }

    /// Persist `payload` as the next event in the queue.
    ///
    /// Returns the path of the written file.
    pub fn enqueue(
        &self,
        payload: serde_json::Value,
        event_type: &str,
    ) -> std::io::Result<PathBuf> {
        let seq = self.next_sequence();
        let label = safe_label(event_type);
        let filename = format!("{:06}_{}.json", seq, label);
        let filepath = self.queue_dir.join(&filename);

        let envelope = serde_json::json!({
            "sequence": seq,
            "payload": payload,
            "retry_count": 0,
        });

        // Atomic write: write to .tmp then rename.
        let tmp_path = filepath.with_extension("tmp");
        fs::write(&tmp_path, serde_json::to_string_pretty(&envelope)?)?;
        fs::rename(&tmp_path, &filepath)?;

        info!(seq, file = %filepath.display(), "Enqueued event");
        Ok(filepath)
    }

    /// Return the next event without removing it, or `None` if empty.
    pub fn peek(&self) -> std::io::Result<Option<QueuedEvent>> {
        let files = self.sorted_event_files();
        match files.first() {
            Some(path) => Ok(Some(Self::load_event(path)?)),
            None => Ok(None),
        }
    }

    /// Remove and return the next event, or `None` if empty.
    pub fn dequeue(&self) -> std::io::Result<Option<QueuedEvent>> {
        let files = self.sorted_event_files();
        match files.first() {
            Some(path) => {
                let event = Self::load_event(path)?;
                fs::remove_file(path)?;
                info!(seq = event.sequence, file = %path.display(), "Dequeued event");
                Ok(Some(event))
            }
            None => Ok(None),
        }
    }

    /// Re-enqueue an event with an incremented retry count.
    ///
    /// If the retry count exceeds `max_retry`, the event is moved to the
    /// dead-letter directory instead.
    pub fn nack(&self, event: &mut QueuedEvent, event_type: &str) -> std::io::Result<()> {
        event.retry_count += 1;

        if event.retry_count > self.max_retry {
            self.move_to_dead_letter(event, event_type)?;
            return Ok(());
        }

        // Re-enqueue at the back of the queue.
        let seq = self.next_sequence();
        let label = safe_label(event_type);
        let filename = format!("{:06}_{}.json", seq, label);
        let filepath = self.queue_dir.join(&filename);

        let envelope = serde_json::json!({
            "sequence": seq,
            "payload": event.payload,
            "retry_count": event.retry_count,
        });

        let tmp_path = filepath.with_extension("tmp");
        fs::write(&tmp_path, serde_json::to_string_pretty(&envelope)?)?;
        fs::rename(&tmp_path, &filepath)?;

        info!(
            retry_count = event.retry_count,
            max_retry = self.max_retry,
            file = %filepath.display(),
            "Nacked event"
        );
        Ok(())
    }

    /// Return the number of events currently in the queue.
    pub fn size(&self) -> usize {
        self.sorted_event_files().len()
    }

    /// Return `true` if the queue has no events.
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Return the number of events in the dead-letter directory.
    pub fn dead_letter_count(&self) -> usize {
        if !self.dead_dir.exists() {
            return 0;
        }
        fs::read_dir(&self.dead_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| {
                        let p = e.path();
                        p.extension().and_then(|ext| ext.to_str()) == Some("json") && p.is_file()
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Return the next sequence number based on existing files.
    fn next_sequence(&self) -> u64 {
        let files = self.sorted_event_files();
        match files.last() {
            Some(path) => {
                let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("0");
                let seq_str = stem.split('_').next().unwrap_or("0");
                seq_str.parse::<u64>().unwrap_or(0) + 1
            }
            None => 1,
        }
    }

    /// Return event JSON files sorted by filename (i.e. sequence order).
    fn sorted_event_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.queue_dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("json") && p.is_file())
            .collect();
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        files
    }

    /// Move a failed event to the dead-letter directory.
    fn move_to_dead_letter(
        &self,
        event: &QueuedEvent,
        event_type: &str,
    ) -> std::io::Result<PathBuf> {
        let label = safe_label(event_type);
        let filename = format!("{:06}_{}_dead.json", event.sequence, label);
        let filepath = self.dead_dir.join(&filename);

        let envelope = serde_json::json!({
            "sequence": event.sequence,
            "payload": event.payload,
            "retry_count": event.retry_count,
        });

        fs::write(&filepath, serde_json::to_string_pretty(&envelope)?)?;
        warn!(
            seq = event.sequence,
            file = %filepath.display(),
            "Event moved to dead-letter queue"
        );
        Ok(filepath)
    }

    /// Load a [`QueuedEvent`] from a JSON file on disk.
    fn load_event(path: &Path) -> std::io::Result<QueuedEvent> {
        let data = fs::read_to_string(path)?;
        let value: serde_json::Value = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        Ok(QueuedEvent {
            sequence: value["sequence"].as_u64().unwrap_or(0),
            filename: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            payload: value["payload"].clone(),
            retry_count: value["retry_count"].as_u64().unwrap_or(0) as u32,
        })
    }
}

/// Sanitise an event type string for use in a filename.
///
/// Replaces `.`, `/`, and ` ` with `_`, then truncates to
/// [`QUEUE_FILENAME_LABEL_MAX_LENGTH`].
fn safe_label(event_type: &str) -> String {
    event_type
        .replace(['.', '/', ' '], "_")
        .chars()
        .take(QUEUE_FILENAME_LABEL_MAX_LENGTH)
        .collect()
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::DEFAULT_QUEUE_MAX_RETRY;
    use serde_json::json;
    use tempfile::TempDir;

    /// Helper: create a queue in a fresh temp directory.
    fn make_queue(max_retry: u32) -> (TempDir, DiskPersistedQueue) {
        let tmp = TempDir::new().expect("failed to create temp dir");
        let queue_dir = tmp.path().join("queue");
        let q = DiskPersistedQueue::new(&queue_dir, max_retry).expect("failed to create queue");
        (tmp, q)
    }

    // 1. new() creates queue and dead directories
    #[test]
    fn test_new_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let queue_dir = tmp.path().join("queue");
        let dead_dir = queue_dir.join("dead");

        assert!(!queue_dir.exists());
        assert!(!dead_dir.exists());

        let _q = DiskPersistedQueue::new(&queue_dir, DEFAULT_QUEUE_MAX_RETRY).unwrap();

        assert!(queue_dir.is_dir());
        assert!(dead_dir.is_dir());
    }

    // 1b. new() cleans up stale .tmp files
    #[test]
    fn test_new_cleans_stale_tmp_files() {
        let tmp = TempDir::new().unwrap();
        let queue_dir = tmp.path().join("queue");
        fs::create_dir_all(&queue_dir).unwrap();

        let stale = queue_dir.join("000001_event.tmp");
        fs::write(&stale, "stale").unwrap();
        assert!(stale.exists());

        let _q = DiskPersistedQueue::new(&queue_dir, DEFAULT_QUEUE_MAX_RETRY).unwrap();

        assert!(!stale.exists());
    }

    // 2. enqueue creates numbered JSON file
    #[test]
    fn test_enqueue_creates_numbered_json_file() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        let path = q
            .enqueue(json!({"action": "opened"}), "issues_opened")
            .unwrap();

        assert!(path.exists());
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "000001_issues_opened.json"
        );

        let content: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(content["sequence"], 1);
        assert_eq!(content["payload"]["action"], "opened");
        assert_eq!(content["retry_count"], 0);
    }

    // 3. enqueue multiple events increments sequence
    #[test]
    fn test_enqueue_multiple_increments_sequence() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        let p1 = q.enqueue(json!({"n": 1}), "event").unwrap();
        let p2 = q.enqueue(json!({"n": 2}), "event").unwrap();
        let p3 = q.enqueue(json!({"n": 3}), "event").unwrap();

        assert_eq!(
            p1.file_name().unwrap().to_str().unwrap(),
            "000001_event.json"
        );
        assert_eq!(
            p2.file_name().unwrap().to_str().unwrap(),
            "000002_event.json"
        );
        assert_eq!(
            p3.file_name().unwrap().to_str().unwrap(),
            "000003_event.json"
        );
    }

    // 4. peek returns first event without removing
    #[test]
    fn test_peek_returns_first_without_removing() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        q.enqueue(json!({"n": 1}), "event").unwrap();
        q.enqueue(json!({"n": 2}), "event").unwrap();

        let event = q.peek().unwrap().expect("peek should return an event");
        assert_eq!(event.sequence, 1);
        assert_eq!(event.payload, json!({"n": 1}));
        assert_eq!(event.filename, "000001_event.json");

        // File should still exist — peek does not remove.
        assert_eq!(q.size(), 2);
    }

    // 5. dequeue returns and removes first event
    #[test]
    fn test_dequeue_returns_and_removes() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        q.enqueue(json!({"n": 1}), "event").unwrap();
        q.enqueue(json!({"n": 2}), "event").unwrap();

        let event = q
            .dequeue()
            .unwrap()
            .expect("dequeue should return an event");
        assert_eq!(event.sequence, 1);
        assert_eq!(event.payload, json!({"n": 1}));

        // Only one event should remain.
        assert_eq!(q.size(), 1);

        let second = q
            .dequeue()
            .unwrap()
            .expect("dequeue should return second event");
        assert_eq!(second.sequence, 2);
        assert_eq!(q.size(), 0);
    }

    // 6. dequeue on empty returns None
    #[test]
    fn test_dequeue_empty_returns_none() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        assert!(q.dequeue().unwrap().is_none());
    }

    // 7. nack re-enqueues with incremented retry count
    #[test]
    fn test_nack_reenqueues_with_incremented_retry() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        q.enqueue(json!({"action": "test"}), "event").unwrap();
        let mut event = q.dequeue().unwrap().unwrap();
        assert_eq!(event.retry_count, 0);

        q.nack(&mut event, "event").unwrap();

        assert_eq!(event.retry_count, 1);
        assert_eq!(q.size(), 1);
        assert_eq!(q.dead_letter_count(), 0);

        let re_event = q.peek().unwrap().unwrap();
        assert_eq!(re_event.retry_count, 1);
        assert_eq!(re_event.payload, json!({"action": "test"}));
    }

    // 8. nack exceeding max_retry moves to dead letter
    #[test]
    fn test_nack_exceeding_max_retry_moves_to_dead_letter() {
        let (_tmp, q) = make_queue(2); // max_retry = 2

        q.enqueue(json!({"action": "fail"}), "event").unwrap();
        let mut event = q.dequeue().unwrap().unwrap();

        // First nack: retry_count -> 1 (re-enqueue)
        q.nack(&mut event, "event").unwrap();
        assert_eq!(q.size(), 1);
        assert_eq!(q.dead_letter_count(), 0);

        // Dequeue and nack again: retry_count -> 2 (re-enqueue, still within limit)
        let mut event = q.dequeue().unwrap().unwrap();
        q.nack(&mut event, "event").unwrap();
        assert_eq!(q.size(), 1);
        assert_eq!(q.dead_letter_count(), 0);

        // Dequeue and nack again: retry_count -> 3 (exceeds max_retry=2, dead letter)
        let mut event = q.dequeue().unwrap().unwrap();
        q.nack(&mut event, "event").unwrap();
        assert_eq!(q.size(), 0);
        assert_eq!(q.dead_letter_count(), 1);
    }

    // 9. dead_letter_count returns correct count
    #[test]
    fn test_dead_letter_count() {
        let (_tmp, q) = make_queue(0); // max_retry = 0, first nack goes to dead letter

        q.enqueue(json!({"a": 1}), "evt").unwrap();
        q.enqueue(json!({"a": 2}), "evt").unwrap();

        let mut e1 = q.dequeue().unwrap().unwrap();
        q.nack(&mut e1, "evt").unwrap();
        assert_eq!(q.dead_letter_count(), 1);

        let mut e2 = q.dequeue().unwrap().unwrap();
        q.nack(&mut e2, "evt").unwrap();
        assert_eq!(q.dead_letter_count(), 2);
    }

    // 10. size and is_empty work correctly
    #[test]
    fn test_size_and_is_empty() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);

        assert_eq!(q.size(), 0);
        assert!(q.is_empty());

        q.enqueue(json!({"n": 1}), "event").unwrap();
        assert_eq!(q.size(), 1);
        assert!(!q.is_empty());

        q.enqueue(json!({"n": 2}), "event").unwrap();
        assert_eq!(q.size(), 2);

        q.dequeue().unwrap();
        assert_eq!(q.size(), 1);

        q.dequeue().unwrap();
        assert_eq!(q.size(), 0);
        assert!(q.is_empty());
    }

    // 11. safe_label sanitizes dots, slashes, spaces and truncates
    #[test]
    fn test_safe_label_sanitizes_and_truncates() {
        assert_eq!(safe_label("issues.opened"), "issues_opened");
        assert_eq!(safe_label("path/to/event"), "path_to_event");
        assert_eq!(safe_label("some event type"), "some_event_type");
        assert_eq!(safe_label("a.b/c d"), "a_b_c_d");

        // Truncation to QUEUE_FILENAME_LABEL_MAX_LENGTH (60)
        let long_label = "a".repeat(100);
        let result = safe_label(&long_label);
        assert_eq!(result.len(), QUEUE_FILENAME_LABEL_MAX_LENGTH);
    }

    // 11b. safe_label with combined special characters
    #[test]
    fn test_safe_label_combined() {
        assert_eq!(
            safe_label("github.event/issue comment"),
            "github_event_issue_comment"
        );
    }

    // Extra: peek on empty returns None
    #[test]
    fn test_peek_empty_returns_none() {
        let (_tmp, q) = make_queue(DEFAULT_QUEUE_MAX_RETRY);
        assert!(q.peek().unwrap().is_none());
    }
}
