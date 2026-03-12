//! Agent definition file parser.
//!
//! Parses YAML frontmatter from `.githubclaw/agents/*.md` files and extracts
//! configuration (backend, git author, tool permissions, timeout) plus the
//! instruction body (markdown minus frontmatter).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

const VALID_BACKENDS: &[&str] = &["codex", "claude-code"];

/// Per-backend tool allow/disallow lists.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolPermissions {
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub disallowed: Vec<String>,
}

/// Parsed agent definition -- frontmatter fields plus instruction body.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// Derived from filename (e.g. "coder" from coder.md).
    pub name: String,
    /// "codex" or "claude-code".
    pub backend: String,
    pub git_author_name: String,
    pub git_author_email: String,
    /// `None` means use global default.
    pub timeout: Option<u64>,
    /// Per-backend tool permissions.
    pub tools: HashMap<String, ToolPermissions>,
    /// Instruction body (markdown minus frontmatter).
    pub instruction_body: String,
}

impl AgentDefinition {
    /// Return the tool permissions for the configured backend.
    pub fn active_tools(&self) -> ToolPermissions {
        self.tools.get(&self.backend).cloned().unwrap_or_default()
    }
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            backend: "codex".to_string(),
            git_author_name: "GithubClaw Agent".to_string(),
            git_author_email: "agent@githubclaw.local".to_string(),
            timeout: None,
            tools: HashMap::new(),
            instruction_body: String::new(),
        }
    }
}

/// Raw YAML frontmatter structure for deserialization.
#[derive(Debug, Deserialize, Default)]
struct RawFrontmatter {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    git_author_name: Option<String>,
    #[serde(default)]
    git_author_email: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    tools: Option<HashMap<String, RawToolPermissions>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawToolPermissions {
    #[serde(default)]
    allowed: Option<Vec<String>>,
    #[serde(default)]
    disallowed: Option<Vec<String>>,
}

/// Parse a single agent definition `.md` file.
///
/// # Arguments
///
/// * `path` - Path to the agent definition file (e.g. `.githubclaw/agents/coder.md`).
///
/// # Errors
///
/// Returns an error if the file cannot be read or has malformed frontmatter.
pub fn parse_agent_file(path: &Path) -> Result<AgentDefinition, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    // Regex to match YAML frontmatter delimited by --- on its own line.
    let re = Regex::new(r"(?s)\A---\s*\n(.*?\n)---\s*\n").unwrap();

    let (fm, instruction_body) = match re.captures(&text) {
        Some(caps) => {
            let frontmatter_raw = caps.get(1).unwrap().as_str();
            let body_start = caps.get(0).unwrap().end();
            let body = text[body_start..].trim().to_string();

            let fm: RawFrontmatter = serde_yaml::from_str(frontmatter_raw)
                .map_err(|e| format!("Malformed YAML frontmatter in {}: {}", path.display(), e))?;
            (fm, body)
        }
        None => {
            // No frontmatter -- treat entire file as instruction body with defaults.
            (RawFrontmatter::default(), text.trim().to_string())
        }
    };

    let mut backend = fm.backend.unwrap_or_else(|| "codex".to_string());
    if !VALID_BACKENDS.contains(&backend.as_str()) {
        warn!(
            "Invalid backend {:?} in {}; expected one of {:?}. Defaulting to 'codex'.",
            backend,
            path.display(),
            VALID_BACKENDS,
        );
        backend = "codex".to_string();
    }

    let git_author_email = fm
        .git_author_email
        .unwrap_or_else(|| "agent@githubclaw.local".to_string());
    if !git_author_email.contains('@') {
        warn!(
            "git_author_email {:?} in {} does not contain '@'; this may cause issues.",
            git_author_email,
            path.display(),
        );
    }

    let tools = parse_tools(fm.tools);

    Ok(AgentDefinition {
        name,
        backend,
        git_author_name: fm
            .git_author_name
            .unwrap_or_else(|| "GithubClaw Agent".to_string()),
        git_author_email,
        timeout: fm.timeout,
        tools,
        instruction_body,
    })
}

/// Parse the tools section from frontmatter YAML.
fn parse_tools(
    raw: Option<HashMap<String, RawToolPermissions>>,
) -> HashMap<String, ToolPermissions> {
    let Some(raw_map) = raw else {
        return HashMap::new();
    };

    raw_map
        .into_iter()
        .map(|(backend_name, perms)| {
            let tp = ToolPermissions {
                allowed: perms.allowed.unwrap_or_default(),
                disallowed: perms.disallowed.unwrap_or_default(),
            };
            (backend_name, tp)
        })
        .collect()
}

/// List all available agent type names by scanning `.githubclaw/agents/` directory.
///
/// Returns stem names of all `.md` files found (e.g. `["coder", "qa", "reviewer"]`).
pub fn list_agent_types(repo_root: &Path) -> Vec<String> {
    let agents_dir = repo_root.join(".githubclaw").join("agents");
    if !agents_dir.is_dir() {
        return Vec::new();
    }

    let mut names: Vec<String> = std::fs::read_dir(&agents_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();

    names.sort();
    names
}

/// Load an agent definition by type name from the repo's `.githubclaw/agents/` directory.
///
/// Falls back to built-in defaults if the repo doesn't have a custom definition.
///
/// # Errors
///
/// Returns an error if neither repo-local nor default definition exists.
pub fn load_agent_definition(
    repo_root: &Path,
    agent_type: &str,
) -> Result<AgentDefinition, String> {
    // Try repo-local first.
    let repo_path = repo_root
        .join(".githubclaw")
        .join("agents")
        .join(format!("{}.md", agent_type));
    if repo_path.exists() {
        return parse_agent_file(&repo_path);
    }

    // Fall back to built-in default.
    let default_path = defaults_dir().join(format!("{}.md", agent_type));
    if default_path.exists() {
        return parse_agent_file(&default_path);
    }

    Err(format!(
        "No agent definition found for '{}' in {} or built-in defaults",
        agent_type,
        repo_path.display()
    ))
}

/// Return the path to the built-in defaults directory.
///
/// At runtime this uses a well-known path. In production you may want to
/// embed resources at compile time instead.
pub fn defaults_dir() -> PathBuf {
    PathBuf::from("/usr/local/share/githubclaw/defaults")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_agent_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let agents_dir = dir.join(".githubclaw").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        let file_path = agents_dir.join(format!("{}.md", name));
        fs::write(&file_path, content).unwrap();
        file_path
    }

    #[test]
    fn parse_file_with_valid_frontmatter_and_body() {
        let tmp = TempDir::new().unwrap();
        let content = "---\nbackend: codex\ngit_author_name: Test Bot\ngit_author_email: test@example.com\ntimeout: 3600\n---\n\n# Instructions\n\nDo the thing.\n";
        let path = write_agent_file(tmp.path(), "coder", content);

        let def = parse_agent_file(&path).unwrap();
        assert_eq!(def.name, "coder");
        assert_eq!(def.backend, "codex");
        assert_eq!(def.git_author_name, "Test Bot");
        assert_eq!(def.git_author_email, "test@example.com");
        assert_eq!(def.timeout, Some(3600));
        assert!(def.instruction_body.contains("Do the thing."));
    }

    #[test]
    fn parse_file_without_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let content = "# Just Instructions\n\nNo frontmatter here.\n";
        let path = write_agent_file(tmp.path(), "simple", content);

        let def = parse_agent_file(&path).unwrap();
        assert_eq!(def.name, "simple");
        assert_eq!(def.backend, "codex");
        assert_eq!(def.git_author_name, "GithubClaw Agent");
        assert!(def.instruction_body.contains("No frontmatter here."));
    }

    #[test]
    fn parse_file_with_empty_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let content = "---\n---\n\nBody text.\n";
        // Empty frontmatter: the regex expects at least a newline between delimiters.
        // "---\n---\n" won't match because (.*?\n) requires at least one char+newline.
        // So this is treated as "no frontmatter" and the whole file is instruction body.
        let path = write_agent_file(tmp.path(), "empty_fm", content);

        let def = parse_agent_file(&path).unwrap();
        assert_eq!(def.backend, "codex");
        assert!(!def.instruction_body.is_empty());
    }

    #[test]
    fn invalid_backend_defaults_to_codex() {
        let tmp = TempDir::new().unwrap();
        let content = "---\nbackend: unsupported-backend\n---\n\nBody.\n";
        let path = write_agent_file(tmp.path(), "bad_backend", content);

        let def = parse_agent_file(&path).unwrap();
        assert_eq!(def.backend, "codex");
    }

    #[test]
    fn tool_permissions_parsed_correctly() {
        let tmp = TempDir::new().unwrap();
        let content = r#"---
backend: codex
tools:
  codex:
    allowed:
      - file_read
      - file_write
    disallowed:
      - shell
  claude-code:
    allowed:
      - Read
    disallowed: []
---

Instructions.
"#;
        let path = write_agent_file(tmp.path(), "tooled", content);

        let def = parse_agent_file(&path).unwrap();
        let codex_tools = def.tools.get("codex").unwrap();
        assert_eq!(codex_tools.allowed, vec!["file_read", "file_write"]);
        assert_eq!(codex_tools.disallowed, vec!["shell"]);

        let claude_tools = def.tools.get("claude-code").unwrap();
        assert_eq!(claude_tools.allowed, vec!["Read"]);
        assert!(claude_tools.disallowed.is_empty());
    }

    #[test]
    fn active_tools_returns_correct_backend_permissions() {
        let mut tools = HashMap::new();
        tools.insert(
            "codex".to_string(),
            ToolPermissions {
                allowed: vec!["file_read".to_string()],
                disallowed: vec!["shell".to_string()],
            },
        );
        tools.insert(
            "claude-code".to_string(),
            ToolPermissions {
                allowed: vec!["Read".to_string()],
                disallowed: vec![],
            },
        );

        let def = AgentDefinition {
            backend: "claude-code".to_string(),
            tools,
            ..Default::default()
        };

        let active = def.active_tools();
        assert_eq!(active.allowed, vec!["Read"]);
        assert!(active.disallowed.is_empty());
    }

    #[test]
    fn list_agent_types_scans_directory() {
        let tmp = TempDir::new().unwrap();
        write_agent_file(tmp.path(), "coder", "---\n---\nBody");
        write_agent_file(tmp.path(), "reviewer", "---\n---\nBody");
        write_agent_file(tmp.path(), "qa", "---\n---\nBody");

        let types = list_agent_types(tmp.path());
        assert_eq!(types, vec!["coder", "qa", "reviewer"]);
    }

    #[test]
    fn load_agent_definition_from_repo_path() {
        let tmp = TempDir::new().unwrap();
        let content = "---\nbackend: claude-code\n---\n\nDo stuff.\n";
        write_agent_file(tmp.path(), "coder", content);

        let def = load_agent_definition(tmp.path(), "coder").unwrap();
        assert_eq!(def.name, "coder");
        assert_eq!(def.backend, "claude-code");
    }

    #[test]
    fn missing_agent_file_returns_error() {
        let tmp = TempDir::new().unwrap();
        let result = load_agent_definition(tmp.path(), "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No agent definition found"));
    }

    #[test]
    fn git_author_email_without_at_still_parses() {
        let tmp = TempDir::new().unwrap();
        let content = "---\ngit_author_email: nope-no-at\n---\n\nBody.\n";
        let path = write_agent_file(tmp.path(), "bad_email", content);

        // Should succeed but log a warning (we can't easily assert on warnings here).
        let def = parse_agent_file(&path).unwrap();
        assert_eq!(def.git_author_email, "nope-no-at");
    }
}
