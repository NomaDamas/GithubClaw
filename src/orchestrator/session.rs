//! Orchestrator session management.
//!
//! Each repo gets a per-repo [`OrchestratorSession`] that listens on a Unix
//! domain socket and forwards incoming GitHub events to the Anthropic API for
//! triage. The model responds with an [`ActionList`](super::schema::ActionList)
//! which the session validates and returns to the caller.

use std::path::{Path, PathBuf};

use serde_json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tracing::{error, info, warn};

use crate::constants::*;
use crate::orchestrator::schema::ActionList;
// Action is used in tests
#[cfg(test)]
use crate::orchestrator::schema::Action;

// ---------------------------------------------------------------------------
// Session struct
// ---------------------------------------------------------------------------

pub struct OrchestratorSession {
    pub repo: String,
    pub repo_name: String,
    pub repo_dir: String,
    pub socket_path: String,
    model: String,
    idle_timeout: u64,
    system_prompt_path: String,
    global_prompt_path: String,
    persistence_dir: PathBuf,
    conversation_history: Vec<serde_json::Value>,
}

impl OrchestratorSession {
    /// Create a new orchestrator session for the given repo.
    ///
    /// `repo` should be in `owner/name` format. `repo_dir` is the local
    /// checkout path. `model` and `idle_timeout` fall back to compile-time
    /// defaults when `None`.
    pub fn new(
        repo: &str,
        repo_dir: &str,
        model: Option<&str>,
        idle_timeout: Option<u64>,
    ) -> Self {
        let repo_name = repo.replace('/', "-");
        let socket_path = format!("/tmp/githubclaw-{}.sock", repo_name);
        let persistence_dir = home_dir().join(".githubclaw/sessions").join(&repo_name);

        // Attempt to load persisted conversation history
        let conversation_history =
            Self::load_persisted_state(&persistence_dir).unwrap_or_default();

        Self {
            repo: repo.to_string(),
            repo_name: repo_name.clone(),
            repo_dir: repo_dir.to_string(),
            socket_path,
            model: model.unwrap_or("claude-sonnet-4-20250514").to_string(),
            idle_timeout: idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT_SECONDS),
            system_prompt_path: format!("{}/.githubclaw/orchestrator.md", repo_dir),
            global_prompt_path: format!("{}/.githubclaw/global-prompt.md", repo_dir),
            persistence_dir,
            conversation_history,
        }
    }

    /// Load the repo-specific system prompt from disk.
    ///
    /// Returns a default stub when the file does not exist.
    pub fn load_system_prompt(&self) -> String {
        match std::fs::read_to_string(&self.system_prompt_path) {
            Ok(content) => content,
            Err(_) => format!(
                "You are the orchestrator for the {} repository. \
                 Triage incoming events and decide which agent to dispatch.",
                self.repo
            ),
        }
    }

    /// Load the global prompt from disk (empty string if missing).
    pub fn load_global_prompt(&self) -> String {
        std::fs::read_to_string(&self.global_prompt_path).unwrap_or_default()
    }

    /// Extract and validate an [`ActionList`] from raw model output text.
    ///
    /// Tries, in order:
    /// 1. Direct JSON parse of the full text.
    /// 2. Extracting the first `` ```json ... ``` `` fenced block and parsing that.
    /// 3. Fallback: wraps the raw text as a `no_action` with the text as reasoning.
    pub fn extract_action_list(text: &str) -> ActionList {
        // Attempt 1: direct parse
        if let Ok(list) = serde_json::from_str::<ActionList>(text) {
            if list.validate().is_ok() {
                return list;
            }
        }

        // Attempt 2: extract from ```json ... ``` fences
        if let Some(json_block) = extract_json_fence(text) {
            if let Ok(list) = serde_json::from_str::<ActionList>(&json_block) {
                if list.validate().is_ok() {
                    return list;
                }
            }
        }

        // Attempt 3: fallback
        ActionList::no_action(text.trim())
    }

    // -----------------------------------------------------------------------
    // Unix socket server
    // -----------------------------------------------------------------------

    /// Run the session's Unix socket server, accepting connections and
    /// processing events until idle timeout or shutdown signal.
    pub async fn serve(&mut self) -> Result<(), String> {
        // Remove stale socket file
        let _ = std::fs::remove_file(&self.socket_path);

        let listener = UnixListener::bind(&self.socket_path)
            .map_err(|e| format!("Failed to bind Unix socket {}: {}", self.socket_path, e))?;

        info!(
            "Orchestrator session for {} listening on {}",
            self.repo, self.socket_path
        );

        let idle_timeout = tokio::time::Duration::from_secs(self.idle_timeout);
        let mut last_activity = tokio::time::Instant::now();

        loop {
            // Accept with idle timeout watchdog
            let accept_result = tokio::select! {
                result = listener.accept() => Some(result),
                _ = tokio::time::sleep_until(last_activity + idle_timeout) => None,
            };

            let (stream, _addr) = match accept_result {
                Some(Ok((stream, addr))) => {
                    last_activity = tokio::time::Instant::now();
                    (stream, addr)
                }
                Some(Err(e)) => {
                    error!("Accept error: {}", e);
                    continue;
                }
                None => {
                    info!(
                        "Orchestrator session for {} idle timeout ({} secs), shutting down",
                        self.repo, self.idle_timeout
                    );
                    break;
                }
            };

            // Handle connection
            if let Err(e) = self.handle_connection(stream).await {
                error!("Connection handling error for {}: {}", self.repo, e);
            }

            // Persist state after each event
            if let Err(e) = self.persist().await {
                warn!("Failed to persist session state: {}", e);
            }
        }

        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Handle a single connection: read length-prefixed message, process
    /// the event, and write the length-prefixed response.
    async fn handle_connection(
        &mut self,
        mut stream: tokio::net::UnixStream,
    ) -> Result<(), String> {
        // Read length-prefixed message
        let mut len_buf = [0u8; SOCKET_LENGTH_PREFIX_BYTES];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("Failed to read message length: {}", e))?;

        let msg_len = u64::from_be_bytes(len_buf) as usize;
        if msg_len > SOCKET_MESSAGE_MAX_BYTES {
            return Err(format!(
                "Message too large: {} > {} bytes",
                msg_len, SOCKET_MESSAGE_MAX_BYTES
            ));
        }

        let mut msg_buf = vec![0u8; msg_len];
        stream
            .read_exact(&mut msg_buf)
            .await
            .map_err(|e| format!("Failed to read message: {}", e))?;

        let event_json =
            String::from_utf8(msg_buf).map_err(|e| format!("Message was not UTF-8: {}", e))?;

        // Process the event
        let response = self.process_event(&event_json).await.unwrap_or_else(|e| {
            serde_json::to_string(&ActionList::no_action(&format!("Error: {}", e)))
                .unwrap_or_else(|_| "{}".to_string())
        });

        // Write length-prefixed response
        let resp_bytes = response.as_bytes();
        let resp_len = resp_bytes.len() as u64;
        stream
            .write_all(&resp_len.to_be_bytes())
            .await
            .map_err(|e| format!("Failed to write response length: {}", e))?;
        stream
            .write_all(resp_bytes)
            .await
            .map_err(|e| format!("Failed to write response: {}", e))?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Event processing — core method
    // -----------------------------------------------------------------------

    /// Process a single event: build messages, call the Anthropic API (with
    /// agentic tool-use loop), extract an ActionList, and return serialized JSON.
    pub async fn process_event(&mut self, event_json: &str) -> Result<String, String> {
        // 1. Re-read global-prompt.md (fresh every time)
        let global_prompt = self.load_global_prompt();
        let system_prompt = self.load_system_prompt();

        let full_system = if global_prompt.is_empty() {
            system_prompt
        } else {
            format!("{}\n\n---\n\n{}", global_prompt, system_prompt)
        };

        // 2. Build messages: conversation history + new event
        let event_message = serde_json::json!({
            "role": "user",
            "content": format!(
                "Process this GitHub webhook event and respond with an ActionList JSON:\n\n{}",
                event_json
            )
        });
        self.conversation_history.push(event_message.clone());

        // Trim history if it gets too long
        if self.conversation_history.len() > MESSAGE_HISTORY_WARNING_THRESHOLD {
            warn!(
                "Conversation history for {} has {} messages, trimming oldest",
                self.repo,
                self.conversation_history.len()
            );
            let keep = MESSAGE_HISTORY_WARNING_THRESHOLD / 2;
            self.conversation_history =
                self.conversation_history[self.conversation_history.len() - keep..].to_vec();
        }

        // 3. Define tools (the orchestrator output schema)
        let tools = orchestrator_tools();

        // 4. Call Anthropic API (agentic loop)
        let mut messages = self.conversation_history.clone();
        let mut max_iterations = 10;

        loop {
            let api_response = self
                .call_anthropic_api(&messages, &tools, &full_system)
                .await?;

            // Check stop reason
            let _stop_reason = api_response
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("end_turn");

            // Extract content blocks
            let content = api_response
                .get("content")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            // Collect text from content blocks
            let mut text_parts = Vec::new();
            let mut has_tool_use = false;

            for block in &content {
                match block.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        has_tool_use = true;
                    }
                    _ => {}
                }
            }

            // Add assistant response to messages
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": content
            }));

            if has_tool_use && max_iterations > 0 {
                // Handle tool_use: send back tool results
                let mut tool_results = Vec::new();
                for block in &content {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        let tool_id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        // For now, acknowledge the tool use -- real tool execution
                        // would dispatch to gh CLI, file reads, etc.
                        tool_results.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": "Tool execution acknowledged."
                        }));
                    }
                }

                messages.push(serde_json::json!({
                    "role": "user",
                    "content": tool_results
                }));

                max_iterations -= 1;
                continue;
            }

            // Extract ActionList from final text
            let full_text = text_parts.join("\n");
            let action_list = Self::extract_action_list(&full_text);

            // Update conversation history with the final assistant message
            self.conversation_history.push(serde_json::json!({
                "role": "assistant",
                "content": full_text
            }));

            let result = serde_json::to_string(&action_list)
                .map_err(|e| format!("Failed to serialize ActionList: {}", e))?;

            return Ok(result);
        }
    }

    /// Call the Anthropic Messages API.
    async fn call_anthropic_api(
        &self,
        messages: &[serde_json::Value],
        tools: &[serde_json::Value],
        system_prompt: &str,
    ) -> Result<serde_json::Value, String> {
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;

        let client = reqwest::Client::new();

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": system_prompt,
            "messages": messages,
        });

        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
        }

        let response = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Anthropic API request failed: {}", e))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Anthropic API error ({}): {}",
                status, error_body
            ));
        }

        response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Anthropic API response: {}", e))
    }

    // -----------------------------------------------------------------------
    // Session persistence
    // -----------------------------------------------------------------------

    /// Persist conversation history to disk.
    pub async fn persist(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.persistence_dir)
            .map_err(|e| format!("Failed to create persistence dir: {}", e))?;

        let state_path = self.persistence_dir.join("session_state.json");
        let state = serde_json::json!({
            "repo": self.repo,
            "model": self.model,
            "conversation_history": self.conversation_history,
        });

        let data = serde_json::to_string_pretty(&state)
            .map_err(|e| format!("Failed to serialize session state: {}", e))?;

        // Atomic write
        let tmp_path = state_path.with_extension("tmp");
        std::fs::write(&tmp_path, &data)
            .map_err(|e| format!("Failed to write session state: {}", e))?;
        std::fs::rename(&tmp_path, &state_path)
            .map_err(|e| format!("Failed to rename session state: {}", e))?;

        Ok(())
    }

    /// Load prior conversation history from persisted state.
    pub fn load_persisted_state(persistence_dir: &Path) -> Option<Vec<serde_json::Value>> {
        let state_path = persistence_dir.join("session_state.json");
        let data = std::fs::read_to_string(&state_path).ok()?;
        let state: serde_json::Value = serde_json::from_str(&data).ok()?;
        state
            .get("conversation_history")
            .and_then(|v| v.as_array())
            .cloned()
    }
}

/// Define the orchestrator's available tools for the Anthropic API.
fn orchestrator_tools() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "name": "emit_action_list",
        "description": "Emit a list of actions for the orchestrator to execute. Use this to dispatch agents, schedule events, or decide to take no action.",
        "input_schema": {
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "type": {
                                "type": "string",
                                "enum": ["no_action", "dispatch", "schedule_event", "cancel_event"]
                            },
                            "reasoning": { "type": "string" },
                            "agent_type": { "type": "string" },
                            "issue_ref": { "type": "string" },
                            "task_context": { "type": "string" },
                            "event_id": { "type": "string" },
                            "trigger_at": { "type": "string" },
                            "repo": { "type": "string" },
                            "payload": { "type": "object" },
                            "context": { "type": "string" }
                        },
                        "required": ["type"]
                    }
                },
                "reasoning": { "type": "string" }
            },
            "required": ["actions", "reasoning"]
        }
    })]
}

/// Extract the first `` ```json ... ``` `` fenced code block from text.
fn extract_json_fence(text: &str) -> Option<String> {
    let start_marker = "```json";
    let end_marker = "```";

    let start = text.find(start_marker)?;
    let after_marker = start + start_marker.len();
    let rest = &text[after_marker..];
    let end = rest.find(end_marker)?;
    Some(rest[..end].trim().to_string())
}

/// Get the user's home directory from the `HOME` environment variable.
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Client-side: send event to running orchestrator
// ---------------------------------------------------------------------------

/// Send an event to a running orchestrator via its Unix domain socket.
///
/// The wire protocol is length-prefixed: an 8-byte big-endian length followed
/// by the UTF-8 message bytes. The response follows the same framing.
pub async fn send_event_to_orchestrator(
    repo_name: &str,
    event_json: &str,
    timeout_secs: f64,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    let socket_path = format!("/tmp/githubclaw-{}.sock", repo_name);
    let duration = Duration::from_secs_f64(timeout_secs);

    let mut stream = timeout(duration, UnixStream::connect(&socket_path))
        .await
        .map_err(|_| "Connection timed out".to_string())?
        .map_err(|e| format!("Failed to connect to {}: {}", socket_path, e))?;

    // Send length-prefixed message
    let msg_bytes = event_json.as_bytes();
    let len = msg_bytes.len() as u64;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .map_err(|e| format!("Failed to write length: {}", e))?;
    stream
        .write_all(msg_bytes)
        .await
        .map_err(|e| format!("Failed to write message: {}", e))?;

    // Read length-prefixed response
    let mut len_buf = [0u8; SOCKET_LENGTH_PREFIX_BYTES];
    timeout(duration, stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| "Read timed out".to_string())?
        .map_err(|e| format!("Failed to read response length: {}", e))?;

    let resp_len = u64::from_be_bytes(len_buf) as usize;
    if resp_len > SOCKET_MESSAGE_MAX_BYTES {
        return Err(format!(
            "Response too large: {} > {} bytes",
            resp_len, SOCKET_MESSAGE_MAX_BYTES
        ));
    }

    let mut resp_buf = vec![0u8; resp_len];
    timeout(duration, stream.read_exact(&mut resp_buf))
        .await
        .map_err(|_| "Read timed out".to_string())?
        .map_err(|e| format!("Failed to read response: {}", e))?;

    String::from_utf8(resp_buf).map_err(|e| format!("Response was not UTF-8: {}", e))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // 1. OrchestratorSession::new sets correct paths
    #[test]
    fn new_sets_correct_paths() {
        let session = OrchestratorSession::new(
            "acme/widgets",
            "/home/user/repos/widgets",
            None,
            None,
        );
        assert_eq!(session.repo, "acme/widgets");
        assert_eq!(session.repo_name, "acme-widgets");
        assert_eq!(session.repo_dir, "/home/user/repos/widgets");
        assert_eq!(
            session.system_prompt_path,
            "/home/user/repos/widgets/.githubclaw/orchestrator.md"
        );
        assert_eq!(
            session.global_prompt_path,
            "/home/user/repos/widgets/.githubclaw/global-prompt.md"
        );
    }

    // 2. extract_action_list from valid JSON
    #[test]
    fn extract_action_list_valid_json() {
        let json = r#"{
            "actions": [
                {"type": "dispatch", "agent_type": "coder", "issue_ref": "acme/widgets#42", "task_context": "Fix the bug"}
            ],
            "reasoning": "Dispatching coder to fix the bug."
        }"#;
        let list = OrchestratorSession::extract_action_list(json);
        assert_eq!(list.actions.len(), 1);
        match &list.actions[0] {
            Action::Dispatch {
                agent_type,
                issue_ref,
                ..
            } => {
                assert_eq!(agent_type, "coder");
                assert_eq!(issue_ref, "acme/widgets#42");
            }
            other => panic!("Expected Dispatch, got {:?}", other),
        }
    }

    // 3. extract_action_list from JSON in code fences
    #[test]
    fn extract_action_list_from_code_fences() {
        let text = r#"Here is my analysis:

```json
{
    "actions": [
        {"type": "no_action", "reasoning": "Not relevant to this project."}
    ],
    "reasoning": "The event is not relevant."
}
```

That's my decision."#;
        let list = OrchestratorSession::extract_action_list(text);
        assert_eq!(list.actions.len(), 1);
        match &list.actions[0] {
            Action::NoAction { reasoning } => {
                assert_eq!(reasoning, "Not relevant to this project.");
            }
            other => panic!("Expected NoAction, got {:?}", other),
        }
    }

    // 4. extract_action_list fallback for unparseable text
    #[test]
    fn extract_action_list_fallback() {
        let text = "I'm not sure what to do, this is just free-form text.";
        let list = OrchestratorSession::extract_action_list(text);
        assert_eq!(list.actions.len(), 1);
        match &list.actions[0] {
            Action::NoAction { reasoning } => {
                assert!(reasoning.contains("free-form text"));
            }
            other => panic!("Expected NoAction fallback, got {:?}", other),
        }
    }

    // 5. socket_path derived correctly from repo name
    #[test]
    fn socket_path_derived_from_repo() {
        let session = OrchestratorSession::new(
            "owner/repo-name",
            "/tmp/repo",
            None,
            None,
        );
        assert_eq!(session.socket_path, "/tmp/githubclaw-owner-repo-name.sock");
    }

    // 6. load_system_prompt from file
    #[test]
    fn load_system_prompt_from_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().to_str().unwrap();
        let gc_dir = tmp.path().join(".githubclaw");
        fs::create_dir_all(&gc_dir).unwrap();
        fs::write(gc_dir.join("orchestrator.md"), "Custom system prompt.").unwrap();

        let session = OrchestratorSession::new("owner/repo", repo_dir, None, None);
        let prompt = session.load_system_prompt();
        assert_eq!(prompt, "Custom system prompt.");
    }

    // 7. load_system_prompt with default when file missing
    #[test]
    fn load_system_prompt_default_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().to_str().unwrap();

        let session = OrchestratorSession::new("owner/repo", repo_dir, None, None);
        let prompt = session.load_system_prompt();
        assert!(prompt.contains("owner/repo"));
        assert!(prompt.contains("orchestrator"));
    }

    // 8. persistence_dir under ~/.githubclaw/sessions/
    #[test]
    fn persistence_dir_under_githubclaw_sessions() {
        let session = OrchestratorSession::new(
            "acme/widgets",
            "/tmp/repo",
            None,
            None,
        );
        let home = std::env::var("HOME").unwrap();
        let expected = PathBuf::from(format!(
            "{}/.githubclaw/sessions/acme-widgets",
            home
        ));
        assert_eq!(session.persistence_dir, expected);
    }

    // 9. serve() creates socket file (uses temp path)
    #[tokio::test]
    async fn serve_creates_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().to_str().unwrap();
        let socket_path = tmp.path().join("test-serve.sock");
        let socket_path_str = socket_path.to_str().unwrap().to_string();

        let mut session = OrchestratorSession::new("test/serve-repo", repo_dir, None, Some(2));
        session.socket_path = socket_path_str.clone();

        // Spawn serve in background -- it will time out after 2 seconds
        let handle = tokio::spawn(async move {
            let _ = session.serve().await;
        });

        // Give it a moment to bind
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Check that socket file was created
        assert!(
            socket_path.exists(),
            "Socket file should exist at {}",
            socket_path.display()
        );

        // Wait for the serve to finish (idle timeout)
        let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), handle).await;

        // Socket should be cleaned up after serve exits
        // (may or may not exist depending on timing, so don't assert absence)
    }

    // 10. process_event extracts ActionList correctly (mock test -- no real API call)
    #[test]
    fn process_event_extract_action_list_from_api_response() {
        // Simulate what process_event does with the API response text
        let api_text = r#"{
            "actions": [
                {
                    "type": "dispatch",
                    "agent_type": "triage",
                    "issue_ref": "owner/repo#99",
                    "task_context": "Label and prioritize the new issue"
                }
            ],
            "reasoning": "New issue opened, dispatching triage agent."
        }"#;

        let action_list = OrchestratorSession::extract_action_list(api_text);
        assert_eq!(action_list.actions.len(), 1);
        match &action_list.actions[0] {
            Action::Dispatch {
                agent_type,
                issue_ref,
                task_context,
            } => {
                assert_eq!(agent_type, "triage");
                assert_eq!(issue_ref, "owner/repo#99");
                assert!(task_context.contains("Label"));
            }
            other => panic!("Expected Dispatch, got {:?}", other),
        }
        assert!(action_list.reasoning.contains("triage"));
    }

    // 11. persist and load_persisted_state roundtrip
    #[tokio::test]
    async fn persist_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().to_str().unwrap();

        let mut session = OrchestratorSession::new("test/persist", repo_dir, None, None);
        // Override persistence_dir to use temp
        session.persistence_dir = tmp.path().join("sessions").join("test-persist");

        session.conversation_history.push(serde_json::json!({
            "role": "user",
            "content": "test event"
        }));
        session.conversation_history.push(serde_json::json!({
            "role": "assistant",
            "content": "test response"
        }));

        session.persist().await.unwrap();

        // Load in a fresh context
        let loaded =
            OrchestratorSession::load_persisted_state(&session.persistence_dir).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0]["role"], "user");
        assert_eq!(loaded[1]["role"], "assistant");
    }

    // 12. load_persisted_state returns None for missing file
    #[test]
    fn load_persisted_state_returns_none_for_missing() {
        let result =
            OrchestratorSession::load_persisted_state(Path::new("/nonexistent/path"));
        assert!(result.is_none());
    }

    // 13. orchestrator_tools returns valid tool definitions
    #[test]
    fn orchestrator_tools_valid() {
        let tools = orchestrator_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "emit_action_list");
        assert!(tools[0]["input_schema"]["properties"]["actions"].is_object());
    }
}
