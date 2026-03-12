//! Async scheduled event manager.
//!
//! Persists events to a JSON file and checks periodically for due events.
//! Supports both recurring and one-shot events.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::constants::*;

fn default_true() -> bool {
    true
}

/// A single scheduled event definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledEvent {
    pub event_id: String,
    pub repo: String,
    /// ISO-8601 UTC timestamp for next fire.
    pub trigger_at: String,
    pub payload: serde_json::Value,
    #[serde(default = "default_true")]
    pub one_shot: bool,
    pub interval_seconds: Option<u64>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub retry_count: u32,
}

impl ScheduledEvent {
    /// Parse `trigger_at` into a `DateTime<Utc>`.
    pub fn trigger_datetime(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.trigger_at)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    /// Returns `true` if the event's trigger time is at or before `now`.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.trigger_datetime().is_some_and(|dt| dt <= now)
    }
}

/// Manages scheduled events persisted to disk.
///
/// Call [`ScheduledEventManager::load`] to read events from disk.  Events that
/// become due can be discovered via [`ScheduledEventManager::due_events`].
pub struct ScheduledEventManager {
    json_path: PathBuf,
    events: Vec<ScheduledEvent>,
}

impl ScheduledEventManager {
    /// Create a new manager that persists to `json_path`.
    pub fn new(json_path: impl AsRef<Path>) -> Self {
        Self {
            json_path: json_path.as_ref().to_path_buf(),
            events: Vec::new(),
        }
    }

    /// Load scheduled events from disk. If the file does not exist, starts
    /// with an empty list.
    pub fn load(&mut self) -> std::io::Result<()> {
        if let Some(parent) = self.json_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        if !self.json_path.exists() {
            self.events = Vec::new();
            return Ok(());
        }

        let data = std::fs::read_to_string(&self.json_path)?;
        match serde_json::from_str::<Vec<ScheduledEvent>>(&data) {
            Ok(events) => {
                self.events = events;
                Ok(())
            }
            Err(e) => {
                tracing::error!("Failed to parse scheduled events: {}", e);
                self.events = Vec::new();
                Ok(())
            }
        }
    }

    /// Persist current events list to disk using atomic write.
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.json_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let data = serde_json::to_string_pretty(&self.events).map_err(std::io::Error::other)?;

        let tmp_path = self.json_path.with_extension("tmp");
        std::fs::write(&tmp_path, &data)?;
        std::fs::rename(&tmp_path, &self.json_path)?;
        Ok(())
    }

    /// Create and persist a new scheduled event. Returns the generated event ID.
    ///
    /// # Panics
    ///
    /// Panics if `one_shot` is `false` and `interval_seconds` is `None`.
    pub fn create_event(
        &mut self,
        repo: &str,
        trigger_at: DateTime<Utc>,
        payload: serde_json::Value,
        one_shot: bool,
        interval_seconds: Option<u64>,
        description: &str,
    ) -> String {
        if !one_shot && interval_seconds.is_none() {
            panic!("Recurring events (one_shot=false) require interval_seconds to be set");
        }
        if let Some(secs) = interval_seconds {
            assert!(secs > 0, "interval_seconds must be positive, got {secs}");
        }

        let event_id = uuid::Uuid::new_v4().to_string()[..12].to_string();
        let event = ScheduledEvent {
            event_id: event_id.clone(),
            repo: repo.to_string(),
            trigger_at: trigger_at.to_rfc3339(),
            payload,
            one_shot,
            interval_seconds,
            description: description.to_string(),
            retry_count: 0,
        };
        self.events.push(event);
        // Save is best-effort here; callers can check via save() separately
        let _ = self.save();
        event_id
    }

    /// Cancel a scheduled event by its ID. Returns `true` if found and removed.
    pub fn cancel_event(&mut self, event_id: &str) -> bool {
        let before = self.events.len();
        self.events.retain(|ev| ev.event_id != event_id);
        let removed = self.events.len() < before;
        if removed {
            let _ = self.save();
        }
        removed
    }

    /// Return a reference to all current events.
    pub fn list_events(&self) -> &[ScheduledEvent] {
        &self.events
    }

    /// Return references to all events that are due as of `now`.
    pub fn due_events(&self, now: DateTime<Utc>) -> Vec<&ScheduledEvent> {
        self.events.iter().filter(|ev| ev.is_due(now)).collect()
    }

    /// Fire all due events through the provided async callback.
    ///
    /// Returns the number of events successfully fired.  One-shot events are
    /// removed after firing.  Recurring events have their `trigger_at` advanced.
    /// Failed events are retried up to `MAX_SCHEDULED_EVENT_RETRY_COUNT` times
    /// before being dropped (dead-lettered).
    pub async fn fire_due_events_with_callback<F, Fut>(&mut self, callback: F) -> usize
    where
        F: Fn(String, serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        let now = chrono::Utc::now();
        let mut fired = 0;
        let mut remaining = Vec::new();

        for event in self.events.drain(..) {
            if event.is_due(now) {
                match callback(event.repo.clone(), event.payload.clone()).await {
                    Ok(()) => {
                        fired += 1;
                        if !event.one_shot {
                            // Advance recurring event
                            if let Some(interval) = event.interval_seconds {
                                let mut e = event;
                                if let Some(mut next) = e.trigger_datetime() {
                                    while next <= now {
                                        next += chrono::Duration::seconds(interval as i64);
                                    }
                                    e.trigger_at = next.to_rfc3339();
                                    e.retry_count = 0;
                                    remaining.push(e);
                                }
                            }
                        }
                        // one_shot: don't re-add
                    }
                    Err(_) => {
                        let mut e = event;
                        e.retry_count += 1;
                        if e.retry_count < MAX_SCHEDULED_EVENT_RETRY_COUNT {
                            remaining.push(e);
                        }
                        // else dead-letter (drop)
                    }
                }
            } else {
                remaining.push(event);
            }
        }

        self.events = remaining;
        if fired > 0 {
            let _ = self.save();
        }
        fired
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;
    use tempfile::TempDir;

    /// Helper to create a manager backed by a temp directory.
    fn temp_manager() -> (ScheduledEventManager, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("scheduled.json");
        let mgr = ScheduledEventManager::new(&path);
        (mgr, dir)
    }

    // -- ScheduledEvent unit tests --

    #[test]
    fn is_due_with_past_timestamp() {
        let past = Utc::now() - Duration::hours(1);
        let event = ScheduledEvent {
            event_id: "test1".into(),
            repo: "owner/repo".into(),
            trigger_at: past.to_rfc3339(),
            payload: json!({}),
            one_shot: true,
            interval_seconds: None,
            description: String::new(),
            retry_count: 0,
        };
        assert!(event.is_due(Utc::now()));
    }

    #[test]
    fn is_due_with_future_timestamp() {
        let future = Utc::now() + Duration::hours(1);
        let event = ScheduledEvent {
            event_id: "test2".into(),
            repo: "owner/repo".into(),
            trigger_at: future.to_rfc3339(),
            payload: json!({}),
            one_shot: true,
            interval_seconds: None,
            description: String::new(),
            retry_count: 0,
        };
        assert!(!event.is_due(Utc::now()));
    }

    // -- ScheduledEventManager tests --

    #[test]
    fn create_event_adds_to_list_and_saves() {
        let (mut mgr, _dir) = temp_manager();
        let trigger = Utc::now() + Duration::hours(1);
        let id = mgr.create_event(
            "owner/repo",
            trigger,
            json!({"key": "val"}),
            true,
            None,
            "test event",
        );

        assert_eq!(mgr.list_events().len(), 1);
        assert_eq!(mgr.list_events()[0].event_id, id);
        assert_eq!(mgr.list_events()[0].repo, "owner/repo");
        assert_eq!(mgr.list_events()[0].description, "test event");

        // File should exist on disk
        assert!(mgr.json_path.exists());
    }

    #[test]
    fn cancel_event_removes_from_list() {
        let (mut mgr, _dir) = temp_manager();
        let trigger = Utc::now() + Duration::hours(1);
        let id = mgr.create_event("owner/repo", trigger, json!({}), true, None, "");

        assert_eq!(mgr.list_events().len(), 1);
        assert!(mgr.cancel_event(&id));
        assert_eq!(mgr.list_events().len(), 0);
    }

    #[test]
    fn cancel_event_returns_false_for_nonexistent_id() {
        let (mut mgr, _dir) = temp_manager();
        assert!(!mgr.cancel_event("nonexistent_id"));
    }

    #[test]
    fn list_events_returns_all_events() {
        let (mut mgr, _dir) = temp_manager();
        let trigger = Utc::now() + Duration::hours(1);
        mgr.create_event("repo1", trigger, json!({}), true, None, "first");
        mgr.create_event("repo2", trigger, json!({}), true, None, "second");
        mgr.create_event("repo3", trigger, json!({}), true, None, "third");

        assert_eq!(mgr.list_events().len(), 3);
    }

    #[test]
    fn due_events_returns_only_due_events() {
        let (mut mgr, _dir) = temp_manager();
        let now = Utc::now();
        let past = now - Duration::hours(1);
        let future = now + Duration::hours(1);

        mgr.create_event("past-repo", past, json!({}), true, None, "past");
        mgr.create_event("future-repo", future, json!({}), true, None, "future");

        let due = mgr.due_events(now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].repo, "past-repo");
    }

    #[test]
    fn load_and_save_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("scheduled.json");

        // Create and save
        {
            let mut mgr = ScheduledEventManager::new(&path);
            let trigger = Utc::now() + Duration::hours(1);
            mgr.create_event(
                "owner/repo",
                trigger,
                json!({"action": "test"}),
                true,
                None,
                "roundtrip",
            );
        }

        // Load in a new manager
        {
            let mut mgr = ScheduledEventManager::new(&path);
            mgr.load().unwrap();
            assert_eq!(mgr.list_events().len(), 1);
            assert_eq!(mgr.list_events()[0].repo, "owner/repo");
            assert_eq!(mgr.list_events()[0].description, "roundtrip");
        }
    }

    #[test]
    fn load_from_nonexistent_file_starts_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does_not_exist.json");
        let mut mgr = ScheduledEventManager::new(&path);
        mgr.load().unwrap();
        assert!(mgr.list_events().is_empty());
    }

    #[test]
    #[should_panic(expected = "Recurring events (one_shot=false) require interval_seconds")]
    fn recurring_event_requires_interval_seconds() {
        let (mut mgr, _dir) = temp_manager();
        let trigger = Utc::now() + Duration::hours(1);
        mgr.create_event(
            "owner/repo",
            trigger,
            json!({}),
            false,
            None,
            "bad recurring",
        );
    }
}
