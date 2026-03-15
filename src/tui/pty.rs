//! PTY embedding for TUI interactive sessions.
//!
//! Spawns Claude Code / Codex in a pseudo-terminal and captures output
//! for rendering inside a ratatui panel. Input from the TUI is forwarded
//! to the PTY.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Interactive backend for the embedded PTY session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractiveBackend {
    ClaudeCode,
    Codex,
}

impl InteractiveBackend {
    /// Resolve the interactive backend from repo-local agent definitions.
    ///
    /// Order of precedence:
    /// 1. `.githubclaw/agents/orchestrator.md`
    /// 2. Majority backend from `.githubclaw/agents/*.md`
    /// 3. `defaults/agents/orchestrator.md`
    /// 4. `codex`
    pub fn for_repo(repo_root: &Path) -> Self {
        backend_from_agent_file(&repo_root.join(".githubclaw/agents/orchestrator.md"))
            .or_else(|| backend_from_agent_dir(&repo_root.join(".githubclaw/agents")))
            .or_else(|| backend_from_agent_file(&repo_root.join("defaults/agents/orchestrator.md")))
            .unwrap_or(Self::Codex)
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    fn program_and_args(&self, session_name: &str) -> (&'static str, Vec<String>) {
        match self {
            Self::ClaudeCode => (
                "claude",
                vec!["--resume".to_string(), session_name.to_string()],
            ),
            Self::Codex => (
                "codex",
                vec!["resume".to_string(), session_name.to_string()],
            ),
        }
    }
}

/// An embedded PTY session running Claude Code or Codex.
pub struct PtySession {
    /// Buffered output from the PTY (rendered in TUI panel).
    output_buf: Arc<Mutex<Vec<u8>>>,
    /// Writer to send input to the PTY.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Whether the child process has exited.
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl PtySession {
    /// Spawn an interactive session in a PTY.
    ///
    /// `session_name` is used for the backend-specific resume command.
    pub fn spawn(
        backend: InteractiveBackend,
        session_name: &str,
        working_dir: &str,
        cols: u16,
        rows: u16,
    ) -> Result<Self, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("Failed to open PTY: {}", e))?;

        let (program, args) = backend.program_and_args(session_name);
        let mut cmd = CommandBuilder::new(program);
        for arg in &args {
            cmd.arg(arg);
        }
        cmd.cwd(working_dir);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to spawn {} in PTY: {}", backend.display_name(), e))?;

        // Drop slave — we only need the master side
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("Failed to clone PTY reader: {}", e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("Failed to take PTY writer: {}", e))?;

        let output_buf = Arc::new(Mutex::new(Vec::with_capacity(64 * 1024)));
        let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Background thread to read PTY output
        let buf_clone = Arc::clone(&output_buf);
        let exited_clone = Arc::clone(&exited);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut tmp = [0u8; 4096];
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) => {
                        exited_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    Ok(n) => {
                        let mut buf = buf_clone.lock().unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                        // Cap buffer at 256KB — drop old content
                        if buf.len() > 256 * 1024 {
                            let drain_to = buf.len() - 128 * 1024;
                            buf.drain(..drain_to);
                        }
                    }
                    Err(_) => {
                        exited_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
            // Wait for child to exit
            drop(child);
        });

        Ok(Self {
            output_buf,
            writer: Arc::new(Mutex::new(writer)),
            exited,
        })
    }

    /// Send a key/byte sequence to the PTY (user input).
    pub fn send_input(&self, data: &[u8]) -> Result<(), String> {
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(data)
            .map_err(|e| format!("PTY write error: {}", e))?;
        writer
            .flush()
            .map_err(|e| format!("PTY flush error: {}", e))?;
        Ok(())
    }

    /// Read all buffered output since last call (drains the buffer).
    pub fn read_output(&self) -> Vec<u8> {
        let mut buf = self.output_buf.lock().unwrap();
        let output = buf.clone();
        buf.clear();
        output
    }

    /// Peek at current buffered output without draining.
    pub fn peek_output(&self) -> Vec<u8> {
        let buf = self.output_buf.lock().unwrap();
        buf.clone()
    }

    /// Get the last N lines of output for display.
    pub fn last_lines(&self, max_lines: usize) -> String {
        let buf = self.output_buf.lock().unwrap();
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        lines[start..].join("\n")
    }

    /// Check if the PTY process has exited.
    pub fn has_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Resize the PTY.
    pub fn resize(&self, _cols: u16, _rows: u16) {
        // portable-pty resize requires the master handle which we can't
        // easily access after construction. For now, this is a no-op.
        // A future improvement could store the master for resizing.
    }

    #[cfg(test)]
    pub(crate) fn test_stub() -> Self {
        Self {
            output_buf: Arc::new(Mutex::new(Vec::new())),
            writer: Arc::new(Mutex::new(Box::new(std::io::sink()))),
            exited: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

fn backend_from_agent_file(path: &Path) -> Option<InteractiveBackend> {
    let agent = crate::agents::parser::parse_agent_file(path).ok()?;
    Some(match agent.backend.as_str() {
        "claude-code" => InteractiveBackend::ClaudeCode,
        _ => InteractiveBackend::Codex,
    })
}

fn backend_from_agent_dir(path: &Path) -> Option<InteractiveBackend> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut codex = 0usize;
    let mut claude = 0usize;

    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }

        if let Some(backend) = backend_from_agent_file(&path) {
            match backend {
                InteractiveBackend::ClaudeCode => claude += 1,
                InteractiveBackend::Codex => codex += 1,
            }
        }
    }

    if claude == 0 && codex == 0 {
        None
    } else if claude > codex {
        Some(InteractiveBackend::ClaudeCode)
    } else {
        Some(InteractiveBackend::Codex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn claude_resume_command_uses_resume_flag() {
        let (program, args) = InteractiveBackend::ClaudeCode.program_and_args("session-name");
        assert_eq!(program, "claude");
        assert_eq!(
            args,
            vec!["--resume".to_string(), "session-name".to_string()]
        );
    }

    #[test]
    fn codex_resume_command_uses_resume_subcommand() {
        let (program, args) = InteractiveBackend::Codex.program_and_args("session-name");
        assert_eq!(program, "codex");
        assert_eq!(args, vec!["resume".to_string(), "session-name".to_string()]);
    }

    #[test]
    fn repo_local_orchestrator_backend_takes_precedence() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        let agent_dir = repo_root.join(".githubclaw/agents");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join("orchestrator.md"),
            "---\nbackend: codex\n---\n\n# Orchestrator\n",
        )
        .unwrap();

        assert_eq!(
            InteractiveBackend::for_repo(repo_root),
            InteractiveBackend::Codex
        );
    }

    #[test]
    fn repo_agent_backend_majority_beats_default_orchestrator() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        let agent_dir = repo_root.join(".githubclaw/agents");
        let default_dir = repo_root.join("defaults/agents");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&default_dir).unwrap();
        fs::write(
            agent_dir.join("coder.md"),
            "---\nbackend: codex\n---\n\n# Coder\n",
        )
        .unwrap();
        fs::write(
            default_dir.join("orchestrator.md"),
            "---\nbackend: claude-code\n---\n\n# Orchestrator\n",
        )
        .unwrap();

        assert_eq!(
            InteractiveBackend::for_repo(repo_root),
            InteractiveBackend::Codex
        );
    }

    #[test]
    fn default_orchestrator_backend_is_used_when_repo_has_no_agent_overrides() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        let default_dir = repo_root.join("defaults/agents");
        fs::create_dir_all(&default_dir).unwrap();
        fs::write(
            default_dir.join("orchestrator.md"),
            "---\nbackend: claude-code\n---\n\n# Orchestrator\n",
        )
        .unwrap();

        assert_eq!(
            InteractiveBackend::for_repo(repo_root),
            InteractiveBackend::ClaudeCode
        );
    }
}
