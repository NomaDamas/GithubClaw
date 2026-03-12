//! 4-layer prompt assembly for GithubClaw agents.
//!
//! Layers:
//!   1. `.githubclaw/global-prompt.md`   -- agent roster, common rules
//!   2. `.githubclaw/VALUE.md`           -- project north star (read fresh every time)
//!   3. Agent instruction body            -- from parsed definition (minus frontmatter)
//!   4. `task_context`                    -- from orchestrator dispatch output
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
    repo_root: PathBuf,
    temp_files: Vec<PathBuf>,
}

impl PromptAssembler {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
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

    /// Layer 1: Read `.githubclaw/global-prompt.md` with fallback to defaults.
    fn read_global_prompt(&self) -> Result<String, std::io::Error> {
        let repo_path = self.repo_root.join(".githubclaw").join("global-prompt.md");
        if repo_path.exists() {
            let content = Self::read_layer_file(&repo_path);
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
                repo_path.display(),
                default_path.display(),
            ),
        ))
    }

    /// Layer 2: Read `.githubclaw/VALUE.md` fresh every time.
    fn read_value(&self) -> String {
        Self::read_layer_file(&self.repo_root.join(".githubclaw").join("VALUE.md"))
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

    fn setup_repo(tmp: &TempDir) -> PathBuf {
        let root = tmp.path().to_path_buf();
        let gc_dir = root.join(".githubclaw");
        fs::create_dir_all(&gc_dir).unwrap();
        root
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
        let root = setup_repo(&tmp);

        fs::write(
            root.join(".githubclaw").join("global-prompt.md"),
            "Global rules here.",
        )
        .unwrap();
        fs::write(
            root.join(".githubclaw").join("VALUE.md"),
            "Ship fast, ship safe.",
        )
        .unwrap();

        let agent_def = make_agent_def("You are a coder agent.");
        let mut assembler = PromptAssembler::new(&root);

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
        let root = setup_repo(&tmp);

        fs::write(
            root.join(".githubclaw").join("global-prompt.md"),
            "Global rules.",
        )
        .unwrap();
        // No VALUE.md created.

        let agent_def = make_agent_def("Instructions.");
        let mut assembler = PromptAssembler::new(&root);

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
        let root = setup_repo(&tmp);

        fs::write(root.join(".githubclaw").join("global-prompt.md"), "Global.").unwrap();

        let agent_def = make_agent_def("Agent body.");
        let mut assembler = PromptAssembler::new(&root);

        let path = assembler.assemble(&agent_def, "").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert!(!content.contains("Current Task"));
    }

    #[test]
    fn global_prompt_missing_is_non_fatal() {
        let tmp = TempDir::new().unwrap();
        let root = setup_repo(&tmp);
        // No global-prompt.md, no defaults dir.

        let agent_def = make_agent_def("Just instructions.");
        let mut assembler = PromptAssembler::new(&root);

        // Should still succeed -- global prompt missing is a warning, not fatal.
        let path = assembler.assemble(&agent_def, "Do it").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("Just instructions."));
        assert!(content.contains("Do it"));
    }

    #[test]
    fn temp_file_created_and_cleaned_up() {
        let tmp = TempDir::new().unwrap();
        let root = setup_repo(&tmp);

        let agent_def = make_agent_def("Body.");
        let mut assembler = PromptAssembler::new(&root);

        let path = assembler.assemble(&agent_def, "").unwrap();
        assert!(path.exists());

        assembler.cleanup(Some(&path));
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_all_removes_all_tracked_temp_files() {
        let tmp = TempDir::new().unwrap();
        let root = setup_repo(&tmp);

        let agent_def = make_agent_def("Body.");
        let mut assembler = PromptAssembler::new(&root);

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
