//! Normalized resume messages for Orchestrator `--resume` sessions.
//!
//! When a marker webhook arrives and the Orchestrator needs to be resumed,
//! GithubClaw constructs a structured message from the webhook payload
//! and delivers it as the next prompt to the Claude Code `--resume` session.

use serde::{Deserialize, Serialize};

use crate::markers::{self, MarkerType, ParsedMarker};

// ---------------------------------------------------------------------------
// Resume message schema
// ---------------------------------------------------------------------------

/// Normalized message delivered to Orchestrator on `--resume`.
///
/// Contains all context the Orchestrator needs to decide the next step
/// without making additional GitHub API calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResumeMessage {
    /// Type of event that triggered the resume.
    pub event_type: ResumeEventType,

    /// The marker that was detected (if marker-triggered).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<MarkerType>,

    /// Marker attributes (e.g., `reproduced=true`).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub marker_attributes: std::collections::HashMap<String, String>,

    /// Root issue number.
    pub issue: u64,

    /// Agent type that generated the event (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,

    /// Summary extracted from `<!-- githubclaw:summary -->` block.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,

    /// URL of the GitHub comment that triggered this resume.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment_url: Option<String>,

    /// Raw event type from GitHub webhook (e.g., "issue_comment").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_event: Option<String>,
}

/// Types of events that can trigger an Orchestrator resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeEventType {
    /// A known marker was detected in a GitHub comment.
    MarkerDetected,
    /// A human posted a comment (e.g., direction after stuck).
    HumanComment,
    /// An agent subprocess exited with non-zero (crash/rate-limit).
    AgentFailed,
    /// A new issue was created or updated.
    IssueEvent,
    /// External event (e.g., reporter added info after reproduction failure).
    ExternalComment,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

impl ResumeMessage {
    /// Build a resume message from a marker detected in a GitHub comment.
    pub fn from_marker(
        issue: u64,
        marker: &ParsedMarker,
        summary: Option<String>,
        agent: Option<String>,
        comment_url: Option<String>,
    ) -> Self {
        Self {
            event_type: ResumeEventType::MarkerDetected,
            marker: Some(marker.marker_type.clone()),
            marker_attributes: marker.attributes.clone(),
            issue,
            agent,
            summary,
            comment_url,
            github_event: Some("issue_comment".into()),
        }
    }

    /// Build a resume message from a human comment (e.g., stuck recovery).
    pub fn from_human_comment(issue: u64, comment_body: &str, comment_url: Option<String>) -> Self {
        let summary =
            markers::extract_summary(comment_body).unwrap_or_else(|| comment_body.to_string());
        Self {
            event_type: ResumeEventType::HumanComment,
            marker: None,
            marker_attributes: Default::default(),
            issue,
            agent: None,
            summary: Some(summary),
            comment_url,
            github_event: Some("issue_comment".into()),
        }
    }

    /// Build a resume message from an agent failure (non-zero exit).
    pub fn from_agent_failure(issue: u64, agent_type: &str, exit_code: i32) -> Self {
        Self {
            event_type: ResumeEventType::AgentFailed,
            marker: None,
            marker_attributes: Default::default(),
            issue,
            agent: Some(agent_type.to_string()),
            summary: Some(format!(
                "Agent '{}' exited with code {}",
                agent_type, exit_code
            )),
            comment_url: None,
            github_event: None,
        }
    }

    /// Build a resume message for a new or updated issue event.
    pub fn from_issue_event(issue: u64, github_event: &str, summary: Option<String>) -> Self {
        Self {
            event_type: ResumeEventType::IssueEvent,
            marker: None,
            marker_attributes: Default::default(),
            issue,
            agent: None,
            summary,
            comment_url: None,
            github_event: Some(github_event.to_string()),
        }
    }

    /// Serialize to a human-readable prompt string for Claude Code `--resume`.
    pub fn to_prompt(&self) -> String {
        let mut parts = Vec::new();

        parts.push(format!(
            "[GithubClaw Event] type={} issue=#{}",
            serde_json::to_string(&self.event_type)
                .unwrap_or_default()
                .trim_matches('"'),
            self.issue
        ));

        if let Some(ref marker) = self.marker {
            let mut marker_str = format!("Marker: {}", marker.as_str());
            if !self.marker_attributes.is_empty() {
                let attrs: Vec<String> = self
                    .marker_attributes
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect();
                marker_str.push_str(&format!(" ({})", attrs.join(", ")));
            }
            parts.push(marker_str);
        }

        if let Some(ref agent) = self.agent {
            parts.push(format!("Agent: {}", agent));
        }

        if let Some(ref summary) = self.summary {
            parts.push(format!("Summary:\n{}", summary));
        }

        if let Some(ref url) = self.comment_url {
            parts.push(format!("Comment: {}", url));
        }

        parts.join("\n")
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markers::ParsedMarker;
    use std::collections::HashMap;

    // 1. Build from marker
    #[test]
    fn from_marker_basic() {
        let marker = ParsedMarker {
            marker_type: MarkerType::Verified,
            attributes: HashMap::new(),
        };
        let msg = ResumeMessage::from_marker(
            123,
            &marker,
            Some("All tests passed.".into()),
            Some("verifier".into()),
            Some("https://github.com/org/repo/issues/123#comment-1".into()),
        );
        assert_eq!(msg.event_type, ResumeEventType::MarkerDetected);
        assert_eq!(msg.marker, Some(MarkerType::Verified));
        assert_eq!(msg.issue, 123);
        assert_eq!(msg.agent, Some("verifier".into()));
        assert_eq!(msg.summary, Some("All tests passed.".into()));
    }

    // 2. Build from marker with attributes
    #[test]
    fn from_marker_with_attributes() {
        let mut attrs = HashMap::new();
        attrs.insert("reproduced".into(), "true".into());
        let marker = ParsedMarker {
            marker_type: MarkerType::Reproduced,
            attributes: attrs,
        };
        let msg =
            ResumeMessage::from_marker(42, &marker, None, Some("bug-reproducer".into()), None);
        assert_eq!(msg.marker_attributes["reproduced"], "true");
    }

    // 3. Build from human comment
    #[test]
    fn from_human_comment_basic() {
        let msg = ResumeMessage::from_human_comment(
            123,
            "Try a different approach: use async instead of threads.",
            Some("https://github.com/org/repo/pull/50#comment-2".into()),
        );
        assert_eq!(msg.event_type, ResumeEventType::HumanComment);
        assert!(msg.summary.unwrap().contains("different approach"));
        assert!(msg.marker.is_none());
    }

    // 4. Build from human comment with summary block
    #[test]
    fn from_human_comment_with_summary_block() {
        let body = "Some preamble\n<!-- githubclaw:summary -->\nUse connection pooling.\n<!-- /githubclaw:summary -->\nMore text";
        let msg = ResumeMessage::from_human_comment(123, body, None);
        assert_eq!(msg.summary, Some("Use connection pooling.".into()));
    }

    // 5. Build from agent failure
    #[test]
    fn from_agent_failure_basic() {
        let msg = ResumeMessage::from_agent_failure(42, "implementer", 1);
        assert_eq!(msg.event_type, ResumeEventType::AgentFailed);
        assert_eq!(msg.agent, Some("implementer".into()));
        assert!(msg.summary.unwrap().contains("exited with code 1"));
    }

    // 6. Build from issue event
    #[test]
    fn from_issue_event_basic() {
        let msg = ResumeMessage::from_issue_event(
            99,
            "issues",
            Some("New bug report: crash on startup".into()),
        );
        assert_eq!(msg.event_type, ResumeEventType::IssueEvent);
        assert_eq!(msg.issue, 99);
        assert_eq!(msg.github_event, Some("issues".into()));
    }

    // 7. Serialize to JSON
    #[test]
    fn serialize_to_json() {
        let msg = ResumeMessage::from_issue_event(42, "issues", None);
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"issue\":42"));
        assert!(json.contains("\"event_type\":\"issue_event\""));
    }

    // 8. Deserialize from JSON roundtrip
    #[test]
    fn json_roundtrip() {
        let marker = ParsedMarker {
            marker_type: MarkerType::Approved,
            attributes: HashMap::new(),
        };
        let original = ResumeMessage::from_marker(
            100,
            &marker,
            Some("JTBD analysis complete.".into()),
            None,
            None,
        );
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ResumeMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, original);
    }

    // 9. to_prompt produces readable output
    #[test]
    fn to_prompt_readable() {
        let mut attrs = HashMap::new();
        attrs.insert("reproduced".into(), "true".into());
        let marker = ParsedMarker {
            marker_type: MarkerType::Reproduced,
            attributes: attrs,
        };
        let msg = ResumeMessage::from_marker(
            42,
            &marker,
            Some("Bug confirmed on Ubuntu.".into()),
            Some("bug-reproducer".into()),
            None,
        );
        let prompt = msg.to_prompt();
        assert!(prompt.contains("[GithubClaw Event]"));
        assert!(prompt.contains("issue=#42"));
        assert!(prompt.contains("Marker: reproduced"));
        assert!(prompt.contains("reproduced=true"));
        assert!(prompt.contains("Agent: bug-reproducer"));
        assert!(prompt.contains("Bug confirmed on Ubuntu."));
    }

    // 10. to_prompt minimal (no optional fields)
    #[test]
    fn to_prompt_minimal() {
        let msg = ResumeMessage::from_issue_event(1, "issues", None);
        let prompt = msg.to_prompt();
        assert!(prompt.contains("[GithubClaw Event]"));
        assert!(prompt.contains("issue=#1"));
        assert!(!prompt.contains("Marker:"));
        assert!(!prompt.contains("Agent:"));
        assert!(!prompt.contains("Summary:"));
    }
}
