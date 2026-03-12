//! Scoped custom tools for the orchestrator.
//!
//! Defines tool specifications (for sending to the Anthropic API) and handler
//! functions. Each tool either calls the `gh` CLI or performs filesystem
//! operations within a tightly scoped security sandbox.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::constants::*;

// ---------------------------------------------------------------------------
// Denied paths — always blocked even if under an allowed prefix
// ---------------------------------------------------------------------------

const DENIED_PATHS: &[&str] = &[
    ".githubclaw/secrets",
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gh",
];

// ---------------------------------------------------------------------------
// Tool specification (sent to the Anthropic API)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value, // JSON Schema
}

// ---------------------------------------------------------------------------
// JSON Schema helpers (one per tool)
// ---------------------------------------------------------------------------

fn json_schema_issue() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "number": { "type": "integer", "description": "Issue number" }
        },
        "required": ["number"]
    })
}

fn json_schema_list_issues() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "state": { "type": "string", "enum": ["open", "closed", "all"], "default": "open" },
            "labels": { "type": "string", "description": "Comma-separated label names" },
            "limit": { "type": "integer", "default": 30 }
        }
    })
}

fn json_schema_pr() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "number": { "type": "integer", "description": "PR number" }
        },
        "required": ["number"]
    })
}

fn json_schema_list_prs() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "state": { "type": "string", "enum": ["open", "closed", "merged", "all"], "default": "open" },
            "limit": { "type": "integer", "default": 30 }
        }
    })
}

fn json_schema_discussion() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "number": { "type": "integer", "description": "Discussion number" }
        },
        "required": ["number"]
    })
}

fn json_schema_ci() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "ref_name": { "type": "string", "description": "Branch or SHA (default: default branch)" }
        }
    })
}

fn json_schema_search() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "GitHub search query" },
            "limit": { "type": "integer", "default": 30 }
        },
        "required": ["query"]
    })
}

fn json_schema_read_file() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "File path (relative to repo root or absolute)" }
        },
        "required": ["path"]
    })
}

fn json_schema_read_memory() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "section": { "type": "string", "description": "Section heading to read (without ##). Omit to read all." }
        }
    })
}

fn json_schema_write_memory() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "section": { "type": "string", "description": "Section heading (without ##)" },
            "content": { "type": "string", "description": "New content for the section" }
        },
        "required": ["section", "content"]
    })
}

fn json_schema_web_search() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "Search query" }
        },
        "required": ["query"]
    })
}

// ---------------------------------------------------------------------------
// Build tool specifications
// ---------------------------------------------------------------------------

fn tool_spec(name: &str, description: &str, parameters: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

/// Build the list of tool specs for the Anthropic API.
pub fn build_tool_specs() -> Vec<ToolSpec> {
    vec![
        tool_spec(
            "get_issue",
            "Get details of a specific GitHub issue by number.",
            json_schema_issue(),
        ),
        tool_spec(
            "list_issues",
            "List GitHub issues with optional filters.",
            json_schema_list_issues(),
        ),
        tool_spec(
            "get_pr",
            "Get details of a specific PR by number.",
            json_schema_pr(),
        ),
        tool_spec(
            "list_prs",
            "List pull requests with optional state filter.",
            json_schema_list_prs(),
        ),
        tool_spec(
            "get_pr_diff",
            "Get the full diff of a specific PR.",
            json_schema_pr(),
        ),
        tool_spec(
            "get_discussion",
            "Get details of a GitHub Discussion.",
            json_schema_discussion(),
        ),
        tool_spec(
            "get_ci_status",
            "Get GitHub Actions workflow run status.",
            json_schema_ci(),
        ),
        tool_spec(
            "search_issues",
            "Search issues and PRs.",
            json_schema_search(),
        ),
        tool_spec(
            "read_file",
            "Read a file from the repository.",
            json_schema_read_file(),
        ),
        tool_spec(
            "read_memory",
            "Read a section from memory.md.",
            json_schema_read_memory(),
        ),
        tool_spec(
            "write_memory",
            "Write/update a section in memory.md.",
            json_schema_write_memory(),
        ),
        tool_spec(
            "web_search",
            "Search the web (stub).",
            json_schema_web_search(),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Path resolution & security
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

/// Resolve and check a file path against allowed/denied lists.
///
/// Allowed prefixes: `repo_dir`, `~/.githubclaw/`, and any `extra_allowed`.
/// Denied paths (under `$HOME`): `.githubclaw/secrets`, `.ssh`, `.gnupg`,
/// `.aws`, `.config/gh`.
pub fn resolve_and_check_path(
    path: &str,
    repo_dir: &Path,
    extra_allowed: &[String],
) -> Result<PathBuf, String> {
    // 1. Resolve relative paths against repo_dir
    let resolved = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        repo_dir.join(path)
    };

    // Canonicalize-like normalisation: clean up ../ components.
    // We do NOT use fs::canonicalize because the path may not exist yet and we
    // want deterministic behaviour in tests.
    let resolved = normalize_path(&resolved);

    let home = home_dir();

    // 2. Check denied paths (home-relative)
    for denied in DENIED_PATHS {
        let denied_abs = home.join(denied);
        if resolved.starts_with(&denied_abs) {
            return Err(format!("Access denied: path is under {}", denied));
        }
    }

    // 3. Check allowed prefixes
    let githubclaw_dir = home.join(".githubclaw");
    let mut allowed: Vec<PathBuf> = vec![normalize_path(repo_dir), githubclaw_dir];
    for extra in extra_allowed {
        allowed.push(normalize_path(Path::new(extra)));
    }

    for prefix in &allowed {
        if resolved.starts_with(prefix) {
            return Ok(resolved);
        }
    }

    Err(format!(
        "Access denied: path {} is outside allowed directories",
        resolved.display()
    ))
}

/// Simple path normalisation (resolve `.` and `..` without touching the FS).
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                components.pop();
            }
            std::path::Component::CurDir => {}
            c => components.push(c),
        }
    }
    components.iter().collect()
}

// ---------------------------------------------------------------------------
// Memory-file helpers
// ---------------------------------------------------------------------------

/// Parse `memory.md` into sections keyed by `## heading`.
///
/// Content before the first `## ` heading is stored under the key `""`.
pub fn parse_memory_sections(content: &str) -> HashMap<String, String> {
    let mut sections = HashMap::new();
    let mut current_key = String::new();
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // Flush previous section
            let text = current_lines.join("\n").trim().to_string();
            if !text.is_empty() || !current_key.is_empty() {
                sections.insert(current_key.clone(), text);
            }
            current_key = heading.trim().to_string();
            current_lines.clear();
        } else {
            current_lines.push(line);
        }
    }

    // Flush last section
    let text = current_lines.join("\n").trim().to_string();
    if !text.is_empty() || !current_key.is_empty() {
        sections.insert(current_key, text);
    }

    sections
}

/// Rebuild `memory.md` from a sections map.
///
/// The preamble (key `""`) is emitted first, then sections in sorted order.
pub fn rebuild_memory_file(sections: &HashMap<String, String>) -> String {
    let mut out = String::new();

    // Emit preamble if present
    if let Some(preamble) = sections.get("") {
        if !preamble.is_empty() {
            out.push_str(preamble);
            out.push_str("\n\n");
        }
    }

    // Emit named sections in sorted order for determinism
    let mut keys: Vec<&String> = sections.keys().filter(|k| !k.is_empty()).collect();
    keys.sort();

    for key in keys {
        out.push_str(&format!("## {}\n\n", key));
        let body = sections.get(key).map(|s| s.as_str()).unwrap_or("");
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out.push('\n');
    }

    // Trim trailing whitespace but keep a final newline
    let trimmed = out.trim_end().to_string();
    if trimmed.is_empty() {
        trimmed
    } else {
        format!("{}\n", trimmed)
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// Execute a tool call. Returns the tool result as a string.
pub async fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    repo: &str,
    repo_dir: &Path,
    memory_file: &Path,
    extra_allowed: &[String],
) -> Result<String, String> {
    match name {
        "get_issue" => {
            let number = args["number"]
                .as_i64()
                .ok_or("missing or invalid 'number'")?;
            run_gh(
                &[
                    "issue",
                    "view",
                    &number.to_string(),
                    "--json",
                    "number,title,state,body,labels,assignees,comments,author,createdAt,updatedAt",
                ],
                repo,
            )
            .await
        }
        "list_issues" => {
            let state = args["state"].as_str().unwrap_or("open");
            let limit_str = args["limit"]
                .as_u64()
                .unwrap_or(ISSUES_LIST_LIMIT as u64)
                .to_string();
            let labels = args["labels"].as_str().unwrap_or("");
            let since = args["since"].as_str().unwrap_or("");
            // Build the search filter up front so it lives long enough.
            let search_filter = format!("updated:>={}", since);
            let mut gh_args = vec![
                "issue",
                "list",
                "--state",
                state,
                "--limit",
                &limit_str,
                "--json",
                "number,title,state,labels,assignees,createdAt",
            ];
            if !labels.is_empty() {
                gh_args.extend(["--label", labels]);
            }
            if !since.is_empty() {
                // gh issue list doesn't have --since; use --search with date filter
                gh_args.extend(["--search", search_filter.as_str()]);
            }
            run_gh(&gh_args, repo).await
        }
        "get_pr" => {
            let number = args["number"]
                .as_i64()
                .ok_or("missing or invalid 'number'")?;
            run_gh(
                &["pr", "view", &number.to_string(), "--json",
                  "number,title,state,body,labels,assignees,author,reviews,headRefName,baseRefName,mergeable,additions,deletions,createdAt,updatedAt,comments"],
                repo,
            )
            .await
        }
        "list_prs" => {
            let state = args["state"].as_str().unwrap_or("open");
            let limit = args["limit"].as_u64().unwrap_or(PRS_LIST_LIMIT as u64);
            run_gh(
                &[
                    "pr",
                    "list",
                    "--state",
                    state,
                    "--limit",
                    &limit.to_string(),
                    "--json",
                    "number,title,state,labels,assignees,createdAt",
                ],
                repo,
            )
            .await
        }
        "get_pr_diff" => {
            let number = args["number"]
                .as_i64()
                .ok_or("missing or invalid 'number'")?;
            run_gh(&["pr", "diff", &number.to_string()], repo).await
        }
        "get_discussion" => {
            let number = args["number"]
                .as_i64()
                .ok_or("missing or invalid 'number'")?;
            // gh doesn't have a native discussion view; use the API endpoint
            let api_path = format!("repos/{}/discussions/{}", repo, number);
            run_gh(
                &["api", &api_path, "--jq", "."],
                "", // repo already embedded in the API path
            )
            .await
        }
        "get_ci_status" => {
            // Accept either run_id (specific run) or ref_name (branch filter)
            if let Some(run_id) = args.get("run_id").and_then(|v| v.as_i64()) {
                run_gh(
                    &[
                        "run",
                        "view",
                        &run_id.to_string(),
                        "--json",
                        "status,conclusion,name,workflowName,jobs,createdAt,updatedAt",
                    ],
                    repo,
                )
                .await
            } else {
                let ref_name = args["ref_name"].as_str().unwrap_or("");
                let mut gh_args = vec![
                    "run",
                    "list",
                    "--limit",
                    "5",
                    "--json",
                    "databaseId,status,conclusion,name,headBranch,createdAt",
                ];
                if !ref_name.is_empty() {
                    gh_args.extend(["--branch", ref_name]);
                }
                run_gh(&gh_args, repo).await
            }
        }
        "search_issues" => {
            let query = args["query"].as_str().ok_or("missing 'query'")?;
            if query.len() > SEARCH_QUERY_MAX_LENGTH {
                return Err(format!(
                    "Search query too long ({} > {} chars)",
                    query.len(),
                    SEARCH_QUERY_MAX_LENGTH
                ));
            }
            let limit = args["limit"]
                .as_u64()
                .unwrap_or(SEARCH_RESULTS_LIMIT as u64);
            run_gh(
                &[
                    "search",
                    "issues",
                    "--match",
                    "title,body",
                    "--limit",
                    &limit.to_string(),
                    "--json",
                    "number,title,repository,state,type",
                    "--",
                    query,
                ],
                repo,
            )
            .await
        }
        "read_file" => {
            let path = args["path"].as_str().ok_or("missing 'path'")?;
            let resolved = resolve_and_check_path(path, repo_dir, extra_allowed)?;
            tokio::fs::read_to_string(&resolved)
                .await
                .map_err(|e| format!("Failed to read {}: {}", resolved.display(), e))
        }
        "read_memory" => {
            let content = tokio::fs::read_to_string(memory_file)
                .await
                .unwrap_or_default();
            let sections = parse_memory_sections(&content);
            match args.get("section").and_then(|v| v.as_str()) {
                Some(section) => Ok(sections.get(section).cloned().unwrap_or_default()),
                None => Ok(content),
            }
        }
        "write_memory" => {
            let section = args["section"].as_str().ok_or("missing 'section'")?;
            let new_content = args["content"].as_str().ok_or("missing 'content'")?;

            let existing = tokio::fs::read_to_string(memory_file)
                .await
                .unwrap_or_default();
            let mut sections = parse_memory_sections(&existing);
            sections.insert(section.to_string(), new_content.to_string());
            let rebuilt = rebuild_memory_file(&sections);

            // Ensure parent directory exists
            if let Some(parent) = memory_file.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            tokio::fs::write(memory_file, &rebuilt)
                .await
                .map_err(|e| format!("Failed to write memory file: {}", e))?;
            Ok(format!("Updated section '{}'", section))
        }
        "web_search" => Ok("Web search is not configured.".into()),
        _ => Err(format!("Unknown tool: {}", name)),
    }
}

/// Run a `gh` CLI command, optionally scoped to a repo.
///
/// Enforces a timeout of [`GH_CLI_TIMEOUT_SECONDS`] and reports clear errors
/// for missing binary, non-zero exit, and timeout conditions.
async fn run_gh(args: &[&str], repo: &str) -> Result<String, String> {
    use tokio::process::Command;

    let mut cmd = Command::new("gh");
    cmd.args(args);
    if !repo.is_empty() {
        cmd.args(["--repo", repo]);
    }

    // Prevent the child from inheriting stdin (avoids blocking on prompts).
    cmd.stdin(std::process::Stdio::null());

    let timeout_duration = std::time::Duration::from_secs_f64(GH_CLI_TIMEOUT_SECONDS);

    let child = cmd.output();

    let output = match tokio::time::timeout(timeout_duration, child).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            // Distinguish "binary not found" from other I/O errors.
            if e.kind() == std::io::ErrorKind::NotFound {
                return Err(
                    "gh CLI not found. Please install the GitHub CLI (https://cli.github.com/)."
                        .to_string(),
                );
            }
            return Err(format!("Failed to run gh: {}", e));
        }
        Err(_elapsed) => {
            return Err(format!(
                "gh command timed out after {}s",
                GH_CLI_TIMEOUT_SECONDS
            ));
        }
    };

    if output.status.success() {
        String::from_utf8(output.stdout).map_err(|e| format!("gh output was not UTF-8: {}", e))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("gh failed ({}): {}", output.status, stderr.trim()))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // 1. build_tool_specs returns 12 tools
    #[test]
    fn build_tool_specs_returns_12_tools() {
        let specs = build_tool_specs();
        assert_eq!(specs.len(), 12);

        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"get_issue"));
        assert!(names.contains(&"list_issues"));
        assert!(names.contains(&"get_pr"));
        assert!(names.contains(&"list_prs"));
        assert!(names.contains(&"get_pr_diff"));
        assert!(names.contains(&"get_discussion"));
        assert!(names.contains(&"get_ci_status"));
        assert!(names.contains(&"search_issues"));
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"read_memory"));
        assert!(names.contains(&"write_memory"));
        assert!(names.contains(&"web_search"));
    }

    // 2. resolve_and_check_path: valid repo-relative path
    #[test]
    fn resolve_repo_relative_path() {
        let repo = PathBuf::from("/tmp/test-repo");
        let result = resolve_and_check_path("src/main.rs", &repo, &[]);
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/test-repo/src/main.rs"));
    }

    // 3. resolve_and_check_path: absolute path under repo
    #[test]
    fn resolve_absolute_path_under_repo() {
        let repo = PathBuf::from("/tmp/test-repo");
        let result = resolve_and_check_path("/tmp/test-repo/Cargo.toml", &repo, &[]);
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/test-repo/Cargo.toml"));
    }

    // 4. resolve_and_check_path: path under ~/.githubclaw/ allowed
    #[test]
    fn resolve_path_under_githubclaw_dir() {
        let home = std::env::var("HOME").unwrap();
        let repo = PathBuf::from("/tmp/test-repo");
        let gc_path = format!("{}/.githubclaw/memory.md", home);
        let result = resolve_and_check_path(&gc_path, &repo, &[]);
        assert!(result.is_ok(), "Expected Ok, got {:?}", result);
        assert_eq!(
            result.unwrap(),
            PathBuf::from(format!("{}/.githubclaw/memory.md", home))
        );
    }

    // 5. resolve_and_check_path: path under ~/.ssh/ denied
    #[test]
    fn resolve_path_under_ssh_denied() {
        let home = std::env::var("HOME").unwrap();
        let repo = PathBuf::from("/tmp/test-repo");
        let ssh_path = format!("{}/.ssh/id_rsa", home);
        let result = resolve_and_check_path(&ssh_path, &repo, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Access denied"));
    }

    // 6. resolve_and_check_path: path under ~/.githubclaw/secrets/ denied
    #[test]
    fn resolve_path_under_githubclaw_secrets_denied() {
        let home = std::env::var("HOME").unwrap();
        let repo = PathBuf::from("/tmp/test-repo");
        let secret_path = format!("{}/.githubclaw/secrets/api_key.txt", home);
        let result = resolve_and_check_path(&secret_path, &repo, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Access denied"));
    }

    // 7. resolve_and_check_path: path outside allowed dirs denied
    #[test]
    fn resolve_path_outside_allowed_denied() {
        let repo = PathBuf::from("/tmp/test-repo");
        let result = resolve_and_check_path("/etc/passwd", &repo, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Access denied"));
    }

    // 8. parse_memory_sections with multiple sections
    #[test]
    fn parse_memory_multiple_sections() {
        let content = "\
## Overview

This is the overview.

## Architecture

The system has three parts.

## Notes

Some notes here.
";
        let sections = parse_memory_sections(content);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections["Overview"], "This is the overview.");
        assert_eq!(sections["Architecture"], "The system has three parts.");
        assert_eq!(sections["Notes"], "Some notes here.");
    }

    // 9. parse_memory_sections with empty content
    #[test]
    fn parse_memory_empty_content() {
        let sections = parse_memory_sections("");
        assert!(sections.is_empty());
    }

    // 10. rebuild_memory_file roundtrip
    #[test]
    fn rebuild_memory_roundtrip() {
        let content = "\
## Architecture

The system has three parts.

## Overview

This is the overview.
";
        let sections = parse_memory_sections(content);
        let rebuilt = rebuild_memory_file(&sections);

        // Re-parse and verify same sections
        let reparsed = parse_memory_sections(&rebuilt);
        assert_eq!(reparsed.len(), sections.len());
        for (key, value) in &sections {
            assert_eq!(
                reparsed.get(key).map(|s| s.as_str()),
                Some(value.as_str()),
                "Section '{}' mismatch after roundtrip",
                key
            );
        }
    }

    // 11. parse_memory_sections ignores preamble
    #[test]
    fn parse_memory_ignores_preamble() {
        let content = "\
# Memory File

This is preamble text before any section.

## First Section

Content of first section.
";
        let sections = parse_memory_sections(content);
        // Preamble stored under key ""
        assert!(sections.contains_key(""));
        assert!(sections[""].contains("preamble text"));
        assert_eq!(sections["First Section"], "Content of first section.");
    }

    // -----------------------------------------------------------------------
    // execute_tool tests (async — only local/stub tools, no gh CLI)
    // -----------------------------------------------------------------------

    /// Helper: create a temp directory with a file inside it.
    fn temp_file_with_content(name: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join(name);
        std::fs::write(&file_path, content).expect("write temp file");
        (dir, file_path)
    }

    // 12. execute_tool read_file — valid path
    #[tokio::test]
    async fn execute_read_file_valid() {
        let (dir, file_path) = temp_file_with_content("hello.txt", "hello world");
        let args = json!({ "path": file_path.to_str().unwrap() });
        let memory = dir.path().join("memory.md");

        let result = execute_tool("read_file", &args, "owner/repo", dir.path(), &memory, &[]).await;

        assert!(result.is_ok(), "Expected Ok, got {:?}", result);
        assert_eq!(result.unwrap(), "hello world");
    }

    // 13. execute_tool read_file — denied path (outside allowed dirs)
    #[tokio::test]
    async fn execute_read_file_denied() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        let args = json!({ "path": "/etc/passwd" });

        let result = execute_tool("read_file", &args, "owner/repo", dir.path(), &memory, &[]).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Access denied"));
    }

    // 14. execute_tool read_memory — reads correct section
    #[tokio::test]
    async fn execute_read_memory_section() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        std::fs::write(
            &memory,
            "## Overview\n\nProject overview.\n\n## Architecture\n\nThree modules.\n",
        )
        .expect("write memory");

        let args = json!({ "section": "Architecture" });
        let result =
            execute_tool("read_memory", &args, "owner/repo", dir.path(), &memory, &[]).await;

        assert!(result.is_ok(), "Expected Ok, got {:?}", result);
        assert_eq!(result.unwrap(), "Three modules.");
    }

    // 15. execute_tool read_memory — missing section returns empty string
    #[tokio::test]
    async fn execute_read_memory_missing_section() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        std::fs::write(&memory, "## Overview\n\nHello.\n").expect("write memory");

        let args = json!({ "section": "NonExistent" });
        let result =
            execute_tool("read_memory", &args, "owner/repo", dir.path(), &memory, &[]).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "");
    }

    // 16. execute_tool write_memory — updates section and preserves others
    #[tokio::test]
    async fn execute_write_memory_updates_section() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        std::fs::write(
            &memory,
            "## Overview\n\nOld overview.\n\n## Notes\n\nKeep me.\n",
        )
        .expect("write memory");

        let args = json!({ "section": "Overview", "content": "New overview." });
        let result = execute_tool(
            "write_memory",
            &args,
            "owner/repo",
            dir.path(),
            &memory,
            &[],
        )
        .await;

        assert!(result.is_ok(), "Expected Ok, got {:?}", result);
        assert!(result.unwrap().contains("Updated section 'Overview'"));

        // Verify the file was updated correctly
        let updated = std::fs::read_to_string(&memory).expect("read back");
        let sections = parse_memory_sections(&updated);
        assert_eq!(sections["Overview"], "New overview.");
        assert_eq!(sections["Notes"], "Keep me.");
    }

    // 17. execute_tool write_memory — adds new section
    #[tokio::test]
    async fn execute_write_memory_adds_section() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        std::fs::write(&memory, "## Overview\n\nExisting.\n").expect("write memory");

        let args = json!({ "section": "Decisions", "content": "Use Rust." });
        let result = execute_tool(
            "write_memory",
            &args,
            "owner/repo",
            dir.path(),
            &memory,
            &[],
        )
        .await;

        assert!(result.is_ok());

        let updated = std::fs::read_to_string(&memory).expect("read back");
        let sections = parse_memory_sections(&updated);
        assert_eq!(sections["Overview"], "Existing.");
        assert_eq!(sections["Decisions"], "Use Rust.");
    }

    // 18. execute_tool web_search — returns stub message
    #[tokio::test]
    async fn execute_web_search_stub() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        let args = json!({ "query": "rust async" });

        let result =
            execute_tool("web_search", &args, "owner/repo", dir.path(), &memory, &[]).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Web search is not configured.");
    }

    // 19. execute_tool unknown tool — returns error
    #[tokio::test]
    async fn execute_unknown_tool() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        let args = json!({});

        let result = execute_tool(
            "nonexistent_tool",
            &args,
            "owner/repo",
            dir.path(),
            &memory,
            &[],
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown tool"));
    }

    // 20. execute_tool search_issues — rejects oversized query
    #[tokio::test]
    async fn execute_search_issues_query_too_long() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let memory = dir.path().join("memory.md");
        let long_query = "a".repeat(SEARCH_QUERY_MAX_LENGTH + 1);
        let args = json!({ "query": long_query });

        let result = execute_tool(
            "search_issues",
            &args,
            "owner/repo",
            dir.path(),
            &memory,
            &[],
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("too long"), "Expected 'too long' in: {}", err);
    }
}
