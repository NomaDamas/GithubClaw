//! 4-layer prompt assembly for GithubClaw agents.
//!
//! Layers:
//!   1. `~/.githubclaw/profiles/<profile>/global-prompt.md`
//!   2. `~/.githubclaw/repos/<repo>/VALUE.md`
//!   3. Agent instruction body
//!   4. `task_context`
//!
//! The assembled prompt is written to a temp file and the path is returned.
//! Cleanup removes the temp file after the agent exits.

use crate::agents::parser::AgentDefinition;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const LAYER_SEPARATOR: &str = "\n\n---\n\n";

/// Assembles the 4-layer agent prompt and manages temp file lifecycle.
pub struct PromptAssembler {
    repo_name: String,
    githubclaw_home: PathBuf,
    temp_files: Vec<PathBuf>,
}

impl PromptAssembler {
    pub fn new(repo_name: impl Into<String>) -> Self {
        Self::with_home(repo_name, crate::config::global_config_dir())
    }

    pub fn with_home(repo_name: impl Into<String>, githubclaw_home: impl AsRef<Path>) -> Self {
        Self {
            repo_name: repo_name.into(),
            githubclaw_home: githubclaw_home.as_ref().to_path_buf(),
            temp_files: Vec::new(),
        }
    }

    /// Read a layer file, returning empty string if missing.
    fn read_layer_file(path: &Path) -> String {
        match fs::read_to_string(path) {
            Ok(content) => content.trim().to_string(),
            Err(_) => {
                warn!("Prompt layer file not found: {}", path.display());
                String::new()
            }
        }
    }

    /// Layer 1: Read profile global prompt with fallback to built-in defaults.
    fn read_global_prompt(&self) -> Result<String, std::io::Error> {
        let repo_config =
            crate::config::RepoConfig::load_for_repo(&self.repo_name, Some(&self.githubclaw_home))
                .unwrap_or_default();
        let profile_path =
            crate::config::profile_dir_from_home(&self.githubclaw_home, &repo_config.profile)
                .join("global-prompt.md");
        if profile_path.exists() {
            let content = Self::read_layer_file(&profile_path);
            if !content.is_empty() {
                return Ok(content);
            }
        }

        // Fall back to built-in default.
        let default_path = crate::agents::parser::defaults_dir().join("global_prompt.md");
        if default_path.exists() {
            let content = Self::read_layer_file(&default_path);
            if !content.is_empty() {
                return Ok(content);
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "global-prompt.md not found in {} or built-in defaults ({}). \
                 This file is critical -- it contains the agent roster and common rules. \
                 Run `githubclaw init` to generate it.",
                profile_path.display(),
                default_path.display(),
            ),
        ))
    }

    /// Layer 2: Read repo VALUE.md fresh every time.
    fn read_value(&self) -> String {
        Self::read_layer_file(&crate::config::repo_value_path_from_home(
            &self.githubclaw_home,
            &self.repo_name,
        ))
    }

    /// Assemble the 4-layer prompt and write to a temp file.
    ///
    /// # Arguments
    ///
    /// * `agent_def` - Parsed agent definition (provides Layer 3 instruction body).
    /// * `task_context` - Task context string from orchestrator dispatch output (Layer 4).
    ///
    /// # Returns
    ///
    /// Path to the temp file containing the assembled prompt.
    pub fn assemble(
        &mut self,
        agent_def: &AgentDefinition,
        task_context: &str,
    ) -> std::io::Result<PathBuf> {
        let mut layers: Vec<String> = Vec::new();

        // Layer 1: Global prompt (agent roster, common rules).
        match self.read_global_prompt() {
            Ok(gp) if !gp.is_empty() => layers.push(gp),
            Ok(_) => {}
            Err(e) => {
                warn!("Global prompt unavailable: {}", e);
                // Continue without it -- non-fatal for assembly.
            }
        }

        // Layer 2: VALUE.md (project north star).
        let value = self.read_value();
        if !value.is_empty() {
            layers.push(format!("# Project North Star (VALUE.md)\n\n{}", value));
        }

        // Layer 3: Agent-specific instruction body.
        if !agent_def.instruction_body.is_empty() {
            layers.push(agent_def.instruction_body.clone());
        }

        // Layer 4: Task context from orchestrator.
        if !task_context.is_empty() {
            layers.push(format!("# Current Task\n\n{}", task_context));
        }

        let assembled = layers.join(LAYER_SEPARATOR);

        // Write to temp file.
        let temp_dir = std::env::temp_dir();
        let file_name = format!(
            "githubclaw_prompt_{}.md",
            uuid::Uuid::new_v4().as_hyphenated()
        );
        let temp_path = temp_dir.join(file_name);

        let mut file = fs::File::create(&temp_path)?;
        file.write_all(assembled.as_bytes())?;

        self.temp_files.push(temp_path.clone());
        info!(
            "Assembled prompt ({} chars) written to {}",
            assembled.len(),
            temp_path.display()
        );
        Ok(temp_path)
    }

    /// Remove a specific temp prompt file, or all tracked files if `path` is `None`.
    pub fn cleanup(&mut self, path: Option<&Path>) {
        match path {
            Some(p) => {
                let _ = fs::remove_file(p);
                self.temp_files.retain(|tp| tp != p);
            }
            None => {
                self.cleanup_all();
            }
        }
    }

    /// Remove all tracked temp prompt files.
    pub fn cleanup_all(&mut self) {
        for p in self.temp_files.drain(..) {
            let _ = fs::remove_file(&p);
        }
    }
}

impl Drop for PromptAssembler {
    fn drop(&mut self) {
        self.cleanup_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    const TEST_REPO: &str = "owner/repo";

    fn setup_home(tmp: &TempDir) -> PathBuf {
        let home = tmp.path().to_path_buf();
        fs::create_dir_all(crate::config::profile_dir_from_home(
            &home,
            crate::config::DEFAULT_PROFILE_NAME,
        ))
        .unwrap();
        fs::create_dir_all(crate::config::repo_dir_from_home(&home, TEST_REPO)).unwrap();
        home
    }

    fn make_agent_def(instruction_body: &str) -> AgentDefinition {
        AgentDefinition {
            name: "test-agent".to_string(),
            backend: "codex".to_string(),
            git_author_name: "Test".to_string(),
            git_author_email: "test@example.com".to_string(),
            timeout: None,
            tools: HashMap::new(),
            instruction_body: instruction_body.to_string(),
        }
    }

    #[test]
    fn assemble_with_all_4_layers_present() {
        let tmp = TempDir::new().unwrap();
        let home = setup_home(&tmp);

        fs::write(
            crate::config::profile_dir_from_home(&home, crate::config::DEFAULT_PROFILE_NAME)
                .join("global-prompt.md"),
            "Global rules here.",
        )
        .unwrap();
        fs::write(
            crate::config::repo_value_path_from_home(&home, TEST_REPO),
            "Ship fast, ship safe.",
        )
        .unwrap();

        let agent_def = make_agent_def("You are a coder agent.");
        let mut assembler = PromptAssembler::with_home(TEST_REPO, &home);

        let path = assembler.assemble(&agent_def, "Fix bug #42").unwrap();
        assert!(path.exists());

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Global rules here."));
        assert!(content.contains("Ship fast, ship safe."));
        assert!(content.contains("You are a coder agent."));
        assert!(content.contains("Fix bug #42"));
        // Verify layer separator is present between layers.
        assert!(content.contains(LAYER_SEPARATOR));
    }

    #[test]
    fn assemble_with_missing_value_md() {
        let tmp = TempDir::new().unwrap();
        let home = setup_home(&tmp);

        fs::write(
            crate::config::profile_dir_from_home(&home, crate::config::DEFAULT_PROFILE_NAME)
                .join("global-prompt.md"),
            "Global rules.",
        )
        .unwrap();
        // No VALUE.md created.

        let agent_def = make_agent_def("Instructions.");
        let mut assembler = PromptAssembler::with_home(TEST_REPO, &home);

        let path = assembler.assemble(&agent_def, "Task context").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(content.contains("Global rules."));
        assert!(!content.contains("Project North Star"));
        assert!(content.contains("Instructions."));
        assert!(content.contains("Task context"));
    }

    #[test]
    fn assemble_with_empty_task_context() {
        let tmp = TempDir::new().unwrap();
        let home = setup_home(&tmp);

        fs::write(
            crate::config::profile_dir_from_home(&home, crate::config::DEFAULT_PROFILE_NAME)
                .join("global-prompt.md"),
            "Global.",
        )
        .unwrap();

        let agent_def = make_agent_def("Agent body.");
        let mut assembler = PromptAssembler::with_home(TEST_REPO, &home);

        let path = assembler.assemble(&agent_def, "").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(!content.contains("Current Task"));
    }

    #[test]
    fn global_prompt_missing_is_non_fatal() {
        let tmp = TempDir::new().unwrap();
        let home = setup_home(&tmp);
        // No global-prompt.md, no defaults dir.

        let agent_def = make_agent_def("Just instructions.");
        let mut assembler = PromptAssembler::with_home(TEST_REPO, &home);

        // Should still succeed -- global prompt missing is a warning, not fatal.
        let path = assembler.assemble(&agent_def, "Do it").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Just instructions."));
        assert!(content.contains("Do it"));
    }

    #[test]
    fn temp_file_created_and_cleaned_up() {
        let tmp = TempDir::new().unwrap();
        let home = setup_home(&tmp);

        let agent_def = make_agent_def("Body.");
        let mut assembler = PromptAssembler::with_home(TEST_REPO, &home);

        let path = assembler.assemble(&agent_def, "").unwrap();
        assert!(path.exists());

        assembler.cleanup(Some(&path));
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_all_removes_all_tracked_temp_files() {
        let tmp = TempDir::new().unwrap();
        let home = setup_home(&tmp);

        let agent_def = make_agent_def("Body.");
        let mut assembler = PromptAssembler::with_home(TEST_REPO, &home);

        let path1 = assembler.assemble(&agent_def, "Task 1").unwrap();
        let path2 = assembler.assemble(&agent_def, "Task 2").unwrap();
        assert!(path1.exists());
        assert!(path2.exists());

        assembler.cleanup_all();
        assert!(!path1.exists());
        assert!(!path2.exists());
    }

    #[test]
    fn layer_separator_is_correct() {
        assert_eq!(LAYER_SEPARATOR, "\n\n---\n\n");
    }
}
