//! Structured output schema for orchestrator responses.
//!
//! Defines the action types the orchestrator can emit: no-op, dispatch an agent,
//! schedule a future event, or cancel a scheduled event. The [`ActionList`] wrapper
//! carries one or more actions plus top-level reasoning, with validation that
//! `NoAction` is mutually exclusive (singleton).

use serde::{Deserialize, Serialize};

/// Tagged union of orchestrator actions.
///
/// Serialized with `#[serde(tag = "type")]` so JSON looks like
/// `{"type": "dispatch", "agent_type": "coder", ...}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Action {
    #[serde(rename = "no_action")]
    NoAction { reasoning: String },

    #[serde(rename = "dispatch")]
    Dispatch {
        agent_type: String,
        issue_ref: String,
        task_context: String,
    },

    #[serde(rename = "schedule_event")]
    ScheduleEvent {
        event_id: String,
        /// ISO-8601 datetime string (e.g. `"2026-03-15T12:00:00Z"`).
        trigger_at: String,
        repo: String,
        #[serde(default)]
        payload: serde_json::Value,
        #[serde(default)]
        context: String,
    },

    #[serde(rename = "cancel_event")]
    CancelEvent { event_id: String },
}

/// A list of orchestrator actions with top-level reasoning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionList {
    pub actions: Vec<Action>,
    #[serde(default)]
    pub reasoning: String,
}

impl ActionList {
    /// Validate the action list.
    ///
    /// Rules:
    /// - `actions` must not be empty.
    /// - If a `NoAction` is present it must be the only action (mutually exclusive).
    pub fn validate(&self) -> Result<(), String> {
        if self.actions.is_empty() {
            return Err("actions list must not be empty".into());
        }
        let has_no_action = self
            .actions
            .iter()
            .any(|a| matches!(a, Action::NoAction { .. }));
        if has_no_action && self.actions.len() > 1 {
            return Err("no_action is mutually exclusive".into());
        }
        Ok(())
    }

    /// Convenience constructor for a singleton no-action response.
    pub fn no_action(reasoning: &str) -> Self {
        Self {
            actions: vec![Action::NoAction {
                reasoning: reasoning.to_string(),
            }],
            reasoning: reasoning.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 1. Serialize/deserialize NoAction
    #[test]
    fn serde_no_action() {
        let action = Action::NoAction {
            reasoning: "nothing to do".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains(r#""type":"no_action"#));
        assert!(json.contains(r#""reasoning":"nothing to do"#));

        let roundtrip: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    // 2. Serialize/deserialize DispatchAction
    #[test]
    fn serde_dispatch() {
        let action = Action::Dispatch {
            agent_type: "coder".into(),
            issue_ref: "owner/repo#42".into(),
            task_context: "Fix the bug described in the issue".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains(r#""type":"dispatch"#));

        let roundtrip: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    // 3. Serialize/deserialize ScheduleEventAction
    #[test]
    fn serde_schedule_event() {
        let action = Action::ScheduleEvent {
            event_id: "evt-001".into(),
            trigger_at: "2026-03-15T12:00:00Z".into(),
            repo: "owner/repo".into(),
            payload: json!({"key": "value"}),
            context: "follow up on stale PR".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains(r#""type":"schedule_event"#));
        assert!(json.contains(r#""trigger_at":"2026-03-15T12:00:00Z"#));

        let roundtrip: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    // 4. Serialize/deserialize CancelEventAction
    #[test]
    fn serde_cancel_event() {
        let action = Action::CancelEvent {
            event_id: "evt-001".into(),
        };
        let json = serde_json::to_string(&action).unwrap();
        assert!(json.contains(r#""type":"cancel_event"#));

        let roundtrip: Action = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, action);
    }

    // 5. ActionList with multiple actions
    #[test]
    fn action_list_multiple_actions() {
        let list = ActionList {
            actions: vec![
                Action::Dispatch {
                    agent_type: "coder".into(),
                    issue_ref: "owner/repo#10".into(),
                    task_context: "implement feature".into(),
                },
                Action::ScheduleEvent {
                    event_id: "evt-002".into(),
                    trigger_at: "2026-04-01T00:00:00Z".into(),
                    repo: "owner/repo".into(),
                    payload: json!(null),
                    context: "".into(),
                },
            ],
            reasoning: "dispatch now and schedule follow-up".into(),
        };

        let json = serde_json::to_string(&list).unwrap();
        let roundtrip: ActionList = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.actions.len(), 2);
        assert_eq!(roundtrip.reasoning, "dispatch now and schedule follow-up");
        assert!(list.validate().is_ok());
    }

    // 6. Validate: empty actions list fails
    #[test]
    fn validate_empty_actions_fails() {
        let list = ActionList {
            actions: vec![],
            reasoning: "oops".into(),
        };
        let err = list.validate().unwrap_err();
        assert_eq!(err, "actions list must not be empty");
    }

    // 7. Validate: no_action with other actions fails
    #[test]
    fn validate_no_action_with_others_fails() {
        let list = ActionList {
            actions: vec![
                Action::NoAction {
                    reasoning: "skip".into(),
                },
                Action::Dispatch {
                    agent_type: "coder".into(),
                    issue_ref: "owner/repo#1".into(),
                    task_context: "do something".into(),
                },
            ],
            reasoning: "mixed".into(),
        };
        let err = list.validate().unwrap_err();
        assert_eq!(err, "no_action is mutually exclusive");
    }

    // 8. Validate: single no_action passes
    #[test]
    fn validate_single_no_action_passes() {
        let list = ActionList::no_action("nothing needed");
        assert!(list.validate().is_ok());
        assert_eq!(list.actions.len(), 1);
        match &list.actions[0] {
            Action::NoAction { reasoning } => assert_eq!(reasoning, "nothing needed"),
            _ => panic!("expected NoAction"),
        }
    }

    // 9. Validate: multiple dispatch actions passes
    #[test]
    fn validate_multiple_dispatches_passes() {
        let list = ActionList {
            actions: vec![
                Action::Dispatch {
                    agent_type: "coder".into(),
                    issue_ref: "owner/repo#1".into(),
                    task_context: "fix bug A".into(),
                },
                Action::Dispatch {
                    agent_type: "reviewer".into(),
                    issue_ref: "owner/repo#2".into(),
                    task_context: "review PR".into(),
                },
            ],
            reasoning: "parallel work".into(),
        };
        assert!(list.validate().is_ok());
    }

    // 10. Parse from JSON string (realistic orchestrator output)
    #[test]
    fn parse_realistic_orchestrator_output() {
        let raw = r#"{
            "actions": [
                {
                    "type": "dispatch",
                    "agent_type": "coder",
                    "issue_ref": "acme/widgets#123",
                    "task_context": "Implement the new caching layer as described in the issue. Use Redis for the backend."
                },
                {
                    "type": "schedule_event",
                    "event_id": "followup-123",
                    "trigger_at": "2026-03-18T09:00:00Z",
                    "repo": "acme/widgets",
                    "payload": {"issue_number": 123, "check": "ci_status"},
                    "context": "Check CI status 7 days after dispatch"
                }
            ],
            "reasoning": "Issue #123 requests a new caching layer. Dispatching coder agent and scheduling a follow-up check in one week."
        }"#;

        let list: ActionList = serde_json::from_str(raw).unwrap();
        assert!(list.validate().is_ok());
        assert_eq!(list.actions.len(), 2);

        match &list.actions[0] {
            Action::Dispatch {
                agent_type,
                issue_ref,
                task_context,
            } => {
                assert_eq!(agent_type, "coder");
                assert_eq!(issue_ref, "acme/widgets#123");
                assert!(task_context.contains("caching layer"));
            }
            other => panic!("expected Dispatch, got {:?}", other),
        }

        match &list.actions[1] {
            Action::ScheduleEvent {
                event_id,
                trigger_at,
                repo,
                payload,
                context,
            } => {
                assert_eq!(event_id, "followup-123");
                assert_eq!(trigger_at, "2026-03-18T09:00:00Z");
                assert_eq!(repo, "acme/widgets");
                assert_eq!(payload["issue_number"], 123);
                assert!(context.contains("CI status"));
            }
            other => panic!("expected ScheduleEvent, got {:?}", other),
        }
    }
}
