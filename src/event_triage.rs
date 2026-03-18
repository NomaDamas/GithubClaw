//! Simplified webhook triage for the first safe GithubClaw manager flows.
//!
//! Supported event families:
//! - new issues
//! - non-self issue comments
//! - pull requests targeting `main`

use std::collections::HashSet;

use serde_json::Value;

use crate::markers;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerKind {
    IssueManager,
    MainPrManager,
}

impl ManagerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::IssueManager => "issue_manager",
            Self::MainPrManager => "main_pr_manager",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriagedEvent {
    pub manager: ManagerKind,
    pub session_number: u64,
    pub prompt: String,
}

pub fn triage_event(event: &Value) -> Option<TriagedEvent> {
    if is_new_issue_event(event) {
        let issue_number = event.pointer("/issue/number").and_then(Value::as_u64)?;
        return Some(TriagedEvent {
            manager: ManagerKind::IssueManager,
            session_number: issue_number,
            prompt: build_new_issue_prompt(event, issue_number),
        });
    }

    if is_external_issue_comment_event(event) {
        let issue_number = event.pointer("/issue/number").and_then(Value::as_u64)?;
        return Some(TriagedEvent {
            manager: ManagerKind::IssueManager,
            session_number: issue_number,
            prompt: build_issue_comment_prompt(event, issue_number),
        });
    }

    if is_main_branch_pr_event(event) {
        let pr_number = event
            .pointer("/pull_request/number")
            .and_then(Value::as_u64)?;
        return Some(TriagedEvent {
            manager: ManagerKind::MainPrManager,
            session_number: pr_number,
            prompt: build_main_pr_prompt(event, pr_number),
        });
    }

    None
}

pub fn is_new_issue_event(event: &Value) -> bool {
    event.get("_githubclaw_event_type").and_then(Value::as_str) == Some("issues")
        && event.get("action").and_then(Value::as_str) == Some("opened")
}

pub fn is_external_issue_comment_event(event: &Value) -> bool {
    event.get("_githubclaw_event_type").and_then(Value::as_str) == Some("issue_comment")
        && event.get("action").and_then(Value::as_str) == Some("created")
        && event
            .get("issue")
            .and_then(|issue| issue.get("pull_request"))
            .is_none()
        && !is_self_issue_comment(event)
}

pub fn is_main_branch_pr_event(event: &Value) -> bool {
    if event.get("_githubclaw_event_type").and_then(Value::as_str) != Some("pull_request") {
        return false;
    }

    matches!(
        event.get("action").and_then(Value::as_str),
        Some("opened") | Some("reopened") | Some("ready_for_review")
    ) && event
        .pointer("/pull_request/base/ref")
        .and_then(Value::as_str)
        == Some("main")
}

pub fn is_self_issue_comment(event: &Value) -> bool {
    if event.get("_githubclaw_event_type").and_then(Value::as_str) != Some("issue_comment") {
        return false;
    }

    let actor_is_self = comment_actor_login(event)
        .map(|login| configured_self_actors().contains(&normalize_actor(login)))
        .unwrap_or(false);
    let body_has_signature = event
        .pointer("/comment/body")
        .and_then(Value::as_str)
        .map(markers::has_hidden_signature)
        .unwrap_or(false);

    actor_is_self || body_has_signature
}

fn build_new_issue_prompt(event: &Value, issue_number: u64) -> String {
    let title = event
        .pointer("/issue/title")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "[GithubClaw Triage]\nmanager=issue_manager\ntrigger=new_issue\nissue=#{issue_number}\n\nRequired actions:\n1. Post an immediate short first-contact reply on the issue.\n2. Perform an initial analysis of the request.\n3. Post a concise analysis/update comment.\n4. Prepare the issue for the next human session or approval decision.\n\nIssue title:\n{title}\n\nEvent payload:\n{}",
        serde_json::to_string_pretty(event).unwrap_or_default()
    )
}

fn build_issue_comment_prompt(event: &Value, issue_number: u64) -> String {
    let actor = comment_actor_login(event).unwrap_or("unknown");
    let body = event
        .pointer("/comment/body")
        .and_then(Value::as_str)
        .unwrap_or("");
    let comment_url = event
        .pointer("/comment/html_url")
        .and_then(Value::as_str)
        .unwrap_or("");

    format!(
        "[GithubClaw Triage]\nmanager=issue_manager\ntrigger=external_issue_comment\nissue=#{issue_number}\nactor={actor}\ncomment_url={comment_url}\n\nRequired actions:\n1. Read the comment and the current issue state.\n2. If the human approved work, start implementation.\n3. If the human requested closure or cancellation, handle that explicitly.\n4. Otherwise respond only with the minimum next action.\n\nComment body:\n{body}\n\nEvent payload:\n{}",
        serde_json::to_string_pretty(event).unwrap_or_default()
    )
}

fn build_main_pr_prompt(event: &Value, pr_number: u64) -> String {
    let title = event
        .pointer("/pull_request/title")
        .and_then(Value::as_str)
        .unwrap_or("");
    let action = event
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    format!(
        "[GithubClaw Triage]\nmanager=main_pr_manager\ntrigger=main_branch_pr\npull_request=#{pr_number}\naction={action}\n\nRequired actions:\n1. Treat this as an independent PR session.\n2. Post one concise comment that contains:\n   - a checklist for whether this PR should target main\n   - exact steps/commands the human should run next\n3. Do not start issue implementation work from this PR event alone.\n\nPR title:\n{title}\n\nEvent payload:\n{}",
        serde_json::to_string_pretty(event).unwrap_or_default()
    )
}

fn comment_actor_login(event: &Value) -> Option<&str> {
    event
        .pointer("/comment/user/login")
        .or_else(|| event.pointer("/sender/login"))
        .and_then(Value::as_str)
}

fn configured_self_actors() -> HashSet<String> {
    let mut actors = HashSet::from([
        "githubclaw[bot]".to_string(),
        "githubclaw".to_string(),
        "githubclaw-app[bot]".to_string(),
    ]);

    if let Ok(value) = std::env::var("GITHUBCLAW_SELF_ACTORS") {
        for actor in value.split(',') {
            let normalized = normalize_actor(actor);
            if !normalized.is_empty() {
                actors.insert(normalized);
            }
        }
    }

    actors
}

fn normalize_actor(actor: &str) -> String {
    actor.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triage_new_issue_to_issue_manager() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "issues",
            "action": "opened",
            "issue": { "number": 42, "title": "Bug: webhook confusion" }
        });

        let triage = triage_event(&event).unwrap();
        assert_eq!(triage.manager, ManagerKind::IssueManager);
        assert_eq!(triage.session_number, 42);
        assert!(triage.prompt.contains("trigger=new_issue"));
    }

    #[test]
    fn self_issue_comment_is_filtered_by_actor() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": { "number": 42 },
            "comment": {
                "body": "status update",
                "user": { "login": "githubclaw[bot]" }
            }
        });

        assert!(is_self_issue_comment(&event));
        assert!(triage_event(&event).is_none());
    }

    #[test]
    fn self_issue_comment_is_filtered_by_signature() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": { "number": 42 },
            "comment": {
                "body": format!("done\n\n{}", markers::HIDDEN_SIGNATURE),
                "user": { "login": "someone-else" }
            }
        });

        assert!(is_self_issue_comment(&event));
        assert!(triage_event(&event).is_none());
    }

    #[test]
    fn external_issue_comment_routes_to_issue_manager() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": { "number": 55 },
            "comment": {
                "body": "/approve",
                "html_url": "https://github.com/org/repo/issues/55#issuecomment-1",
                "user": { "login": "maintainer" }
            }
        });

        let triage = triage_event(&event).unwrap();
        assert_eq!(triage.manager, ManagerKind::IssueManager);
        assert_eq!(triage.session_number, 55);
        assert!(triage.prompt.contains("trigger=external_issue_comment"));
        assert!(triage.prompt.contains("/approve"));
    }

    #[test]
    fn pr_issue_comment_is_not_treated_as_issue_manager_input() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "issue_comment",
            "action": "created",
            "issue": {
                "number": 60,
                "pull_request": { "url": "https://api.github.com/repos/org/repo/pulls/60" }
            },
            "comment": {
                "body": "Looks good",
                "user": { "login": "maintainer" }
            }
        });

        assert!(!is_external_issue_comment_event(&event));
        assert!(triage_event(&event).is_none());
    }

    #[test]
    fn main_branch_pr_routes_to_independent_manager() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "pull_request",
            "action": "opened",
            "pull_request": {
                "number": 99,
                "title": "Release candidate",
                "base": { "ref": "main" }
            }
        });

        let triage = triage_event(&event).unwrap();
        assert_eq!(triage.manager, ManagerKind::MainPrManager);
        assert_eq!(triage.session_number, 99);
        assert!(triage.prompt.contains("trigger=main_branch_pr"));
    }

    #[test]
    fn non_main_pr_is_ignored() {
        let event = serde_json::json!({
            "_githubclaw_event_type": "pull_request",
            "action": "opened",
            "pull_request": {
                "number": 100,
                "base": { "ref": "dev" }
            }
        });

        assert!(!is_main_branch_pr_event(&event));
        assert!(triage_event(&event).is_none());
    }
}
