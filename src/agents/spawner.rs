//! Agent CLI spawner.
//!
//! Builds the environment variables and command-line arguments needed to launch
//! an agent subprocess (e.g. `claude-code` or `codex`) with the correct
//! prompt, tool permissions, and git identity.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::agents::parser::AgentDefinition;

/// Embedded gh wrapper script (compiled into the binary).
const GH_WRAPPER_SCRIPT: &str = include_str!("../../scripts/gh");

/// Spawns agent subprocesses with the correct environment and CLI flags.
pub struct AgentSpawner {
    repo_root: PathBuf,
    max_turns: u32,
}

impl AgentSpawner {
    /// Create a new spawner rooted at `repo_root` with the given turn limit.
    pub fn new(repo_root: impl AsRef<Path>, max_turns: u32) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
            max_turns,
        }
    }

    /// Build the full set of environment variables for an agent subprocess.
    ///
    /// Sets git identity, tool permissions, prompt/task info, GithubClaw
    /// metadata, root issue tracking, and gh wrapper PATH injection.
    /// Optionally merges caller-supplied `extra_env`.
    pub fn build_env(
        &self,
        agent_def: &AgentDefinition,
        prompt_file: &Path,
        task_prompt: &str,
        extra_env: Option<&HashMap<String, String>>,
    ) -> HashMap<String, String> {
        let mut env = HashMap::new();
        let prompt_text = std::fs::read_to_string(prompt_file).unwrap_or_default();

        // Git identity
        env.insert("GIT_AUTHOR_NAME".into(), agent_def.git_author_name.clone());
        env.insert(
            "GIT_AUTHOR_EMAIL".into(),
            agent_def.git_author_email.clone(),
        );
        env.insert(
            "GIT_COMMITTER_NAME".into(),
            agent_def.git_author_name.clone(),
        );
        env.insert(
            "GIT_COMMITTER_EMAIL".into(),
            agent_def.git_author_email.clone(),
        );

        // Tool permissions
        let tools = agent_def.active_tools();
        env.insert("ALLOWED_TOOLS".into(), tools.allowed.join(","));
        env.insert("DISALLOWED_TOOLS".into(), tools.disallowed.join(","));

        // Prompt and task
        env.insert(
            "PROMPT_FILE".into(),
            prompt_file.to_string_lossy().into_owned(),
        );
        env.insert("SYSTEM_PROMPT".into(), prompt_text.clone());
        env.insert("TASK_CONTEXT".into(), task_prompt.to_string());
        let task_prompt_value = if agent_def.backend == "codex" {
            prompt_text
        } else {
            task_prompt.to_string()
        };
        env.insert("TASK_PROMPT".into(), task_prompt_value);
        env.insert("MAX_TURNS".into(), self.max_turns.to_string());

        // GithubClaw metadata
        env.insert("GITHUBCLAW_AGENT_TYPE".into(), agent_def.name.clone());
        env.insert("GITHUBCLAW_BACKEND".into(), agent_def.backend.clone());
        env.insert(
            "GITHUBCLAW_REPO_ROOT".into(),
            self.repo_root.to_string_lossy().into_owned(),
        );

        // Root issue tracking for ref #N injection
        // GITHUBCLAW_ROOT_ISSUE is injected by the caller via extra_env

        // gh wrapper PATH injection
        // Prepend the scripts/ directory (containing gh renamed to gh)
        // to PATH so all gh CLI calls go through our wrapper.
        if let Some(wrapper_dir) = self.gh_wrapper_dir() {
            let current_path = std::env::var("PATH").unwrap_or_default();
            env.insert(
                "PATH".into(),
                format!("{}:{}", wrapper_dir.display(), current_path),
            );
            // Tell the wrapper where the real gh binary is
            if let Ok(real_gh) = which::which("gh") {
                env.insert(
                    "GITHUBCLAW_REAL_GH".into(),
                    real_gh.to_string_lossy().into_owned(),
                );
            }
        }

        // Merge extra env (caller overrides take precedence)
        if let Some(extra) = extra_env {
            for (k, v) in extra {
                env.insert(k.clone(), v.clone());
            }
        }

        env
    }

    /// Get or create the gh wrapper directory.
    ///
    /// Extracts the embedded gh wrapper script to a stable temp directory
    /// so it's always available regardless of where the binary is installed.
    /// The wrapper is placed at `~/.githubclaw/bin/gh`.
    fn gh_wrapper_dir(&self) -> Option<PathBuf> {
        let wrapper_dir = crate::config::global_config_dir().join("bin");
        let wrapper_path = wrapper_dir.join("gh");

        // Only write if missing or outdated
        if !wrapper_path.exists() {
            if std::fs::create_dir_all(&wrapper_dir).is_err() {
                return None;
            }
            if std::fs::write(&wrapper_path, GH_WRAPPER_SCRIPT).is_err() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755));
            }
        }

        Some(wrapper_dir)
    }

    /// Build the command-line arguments for launching the agent.
    ///
    /// Returns a `Vec<String>` where the first element is the program and the
    /// rest are arguments.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend is not recognized.
    pub fn build_command(
        &self,
        agent_def: &AgentDefinition,
        _prompt_file: &Path,
        _task_prompt: &str,
    ) -> Result<Vec<String>, String> {
        self.build_inline_command(&agent_def.backend, false)
    }

    /// Build the command for a resumed Claude orchestrator session.
    pub fn build_resume_command(&self, agent_def: &AgentDefinition) -> Result<Vec<String>, String> {
        self.build_inline_command(&agent_def.backend, true)
    }

    fn build_inline_command(
        &self,
        backend: &str,
        include_resume: bool,
    ) -> Result<Vec<String>, String> {
        let shell = match backend {
            "claude-code" => {
                let mut command = concat!(
                    "cat \"$PROMPT_FILE\" | ",
                    "claude -p ",
                    "--dangerously-skip-permissions ",
                    "--allowedTools \"$ALLOWED_TOOLS\" ",
                    "--disallowedTools \"$DISALLOWED_TOOLS\" ",
                    "--max-turns \"$MAX_TURNS\""
                )
                .to_string();
                if include_resume {
                    command.push_str(" --resume \"$GITHUBCLAW_SESSION_NAME\"");
                }
                command
            }
            "codex" => concat!(
                "cat \"$PROMPT_FILE\" | ",
                "codex exec - ",
                "--dangerously-bypass-approvals-and-sandbox"
            )
            .to_string(),
            other => return Err(format!("unknown backend: {}", other)),
        };

        Ok(vec!["bash".to_string(), "-lc".to_string(), shell])
    }

    // spawn() will be async in the real implementation.
    // For now we only expose build_env and build_command for testing.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::parser::{AgentDefinition, ToolPermissions};
    use crate::constants::DEFAULT_AGENT_MAX_TURNS;
    use std::collections::HashMap;
    use tempfile::TempDir;

    /// Helper: build a minimal agent definition with sensible defaults.
    fn make_agent_def(backend: &str) -> AgentDefinition {
        let mut tools = HashMap::new();
        tools.insert(
            backend.to_string(),
            ToolPermissions {
                allowed: vec!["Read".into(), "Write".into()],
                disallowed: vec!["Shell".into()],
            },
        );
        AgentDefinition {
            name: "coder".into(),
            backend: backend.into(),
            git_author_name: "Test Bot".into(),
            git_author_email: "bot@test.local".into(),
            timeout: None,
            tools,
            instruction_body: "Do the work.".into(),
        }
    }

    // 1. build_env sets git author fields
    #[test]
    fn build_env_sets_git_author_fields() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), DEFAULT_AGENT_MAX_TURNS);
        let def = make_agent_def("claude-code");
        let prompt = tmp.path().join("prompt.md");

        let env = spawner.build_env(&def, &prompt, "fix bug", None);

        assert_eq!(env["GIT_AUTHOR_NAME"], "Test Bot");
        assert_eq!(env["GIT_AUTHOR_EMAIL"], "bot@test.local");
        assert_eq!(env["GIT_COMMITTER_NAME"], "Test Bot");
        assert_eq!(env["GIT_COMMITTER_EMAIL"], "bot@test.local");
    }

    // 2. build_env sets tool permissions
    #[test]
    fn build_env_sets_tool_permissions() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 100);
        let def = make_agent_def("claude-code");
        let prompt = tmp.path().join("prompt.md");
        std::fs::write(&prompt, "System prompt text").unwrap();

        let env = spawner.build_env(&def, &prompt, "task", None);

        assert_eq!(env["ALLOWED_TOOLS"], "Read,Write");
        assert_eq!(env["DISALLOWED_TOOLS"], "Shell");
        assert_eq!(env["SYSTEM_PROMPT"], "System prompt text");
        assert_eq!(env["TASK_CONTEXT"], "task");
    }

    // 3. build_env sets prompt and task info
    #[test]
    fn build_env_sets_prompt_and_task_info() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 50);
        let def = make_agent_def("codex");
        let prompt = tmp.path().join("prompt.md");
        std::fs::write(&prompt, "full prompt").unwrap();

        let env = spawner.build_env(&def, &prompt, "implement caching", None);

        assert_eq!(env["PROMPT_FILE"], prompt.to_string_lossy().as_ref());
        assert_eq!(env["SYSTEM_PROMPT"], "full prompt");
        assert_eq!(env["TASK_CONTEXT"], "implement caching");
        assert_eq!(env["TASK_PROMPT"], "full prompt");
        assert_eq!(env["MAX_TURNS"], "50");
        assert_eq!(env["GITHUBCLAW_AGENT_TYPE"], "coder");
        assert_eq!(env["GITHUBCLAW_BACKEND"], "codex");
        assert_eq!(
            env["GITHUBCLAW_REPO_ROOT"],
            tmp.path().to_string_lossy().as_ref()
        );
    }

    // 4. build_env merges extra_env
    #[test]
    fn build_env_merges_extra_env() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 100);
        let def = make_agent_def("claude-code");
        let prompt = tmp.path().join("prompt.md");
        std::fs::write(&prompt, "System prompt text").unwrap();

        let mut extra = HashMap::new();
        extra.insert("CUSTOM_VAR".into(), "custom_value".into());
        extra.insert("GIT_AUTHOR_NAME".into(), "Override Bot".into());

        let env = spawner.build_env(&def, &prompt, "task", Some(&extra));

        assert_eq!(env["CUSTOM_VAR"], "custom_value");
        // Extra env overrides built-in values
        assert_eq!(env["GIT_AUTHOR_NAME"], "Override Bot");
    }

    // 5. build_command for claude-code backend
    #[test]
    fn build_command_claude_code() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 200);
        let def = make_agent_def("claude-code");
        let prompt = tmp.path().join("prompt.md");

        let cmd = spawner.build_command(&def, &prompt, "fix the bug").unwrap();

        assert_eq!(cmd[0], "bash");
        assert_eq!(cmd[1], "-lc");
        assert!(cmd[2].contains("cat \"$PROMPT_FILE\" | claude -p"));
        assert!(cmd[2].contains("--max-turns \"$MAX_TURNS\""));
        assert!(!cmd[2].contains("--prompt-file"));
        assert!(!cmd[2].contains("--task"));
    }

    // 6. build_command for codex backend
    #[test]
    fn build_command_codex() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 100);
        let def = make_agent_def("codex");
        let prompt = tmp.path().join("prompt.md");

        let cmd = spawner
            .build_command(&def, &prompt, "implement feature")
            .unwrap();

        assert_eq!(cmd[0], "bash");
        assert_eq!(cmd[1], "-lc");
        assert!(cmd[2].contains("cat \"$PROMPT_FILE\" | codex exec -"));
        assert!(cmd[2].contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(!cmd[2].contains("--prompt-file"));
        assert!(!cmd[2].contains("--task"));
    }

    // 7. build_command for unknown backend returns error
    #[test]
    fn build_command_unknown_backend_errors() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 100);
        let def = make_agent_def("unknown-backend");
        let prompt = tmp.path().join("prompt.md");

        let result = spawner.build_command(&def, &prompt, "task");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown backend"));
    }

    #[test]
    fn build_command_ignores_repo_local_spawn_scripts() {
        let tmp = TempDir::new().unwrap();
        let claw_dir = tmp.path().join(".githubclaw");
        std::fs::create_dir_all(&claw_dir).unwrap();

        let script_path = claw_dir.join("spawn_claude.sh");
        std::fs::write(&script_path, "#!/bin/bash\nexit 99").unwrap();

        let spawner = AgentSpawner::new(tmp.path(), 200);
        let def = make_agent_def("claude-code");
        let prompt = tmp.path().join("prompt.md");

        let cmd = spawner.build_command(&def, &prompt, "fix the bug").unwrap();
        assert_eq!(cmd[0], "bash");
        assert_eq!(cmd[1], "-lc");
        assert!(cmd[2].contains("cat \"$PROMPT_FILE\" | claude -p"));
    }

    #[test]
    fn build_resume_command_claude_adds_resume_flag() {
        let tmp = TempDir::new().unwrap();
        let spawner = AgentSpawner::new(tmp.path(), 42);
        let def = make_agent_def("claude-code");

        let cmd = spawner.build_resume_command(&def).unwrap();

        assert_eq!(cmd[0], "bash");
        assert_eq!(cmd[1], "-lc");
        assert!(cmd[2].contains("--resume \"$GITHUBCLAW_SESSION_NAME\""));
        assert!(!cmd[2].contains("--prompt-file"));
    }
}
