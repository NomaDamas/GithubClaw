//! PTY embedding for TUI interactive sessions.
//!
//! Spawns Claude Code / Codex in a pseudo-terminal and captures output
//! for rendering inside a ratatui panel. Input from the TUI is forwarded
//! to the PTY.

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

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
    /// Spawn a Claude Code interactive session in a PTY.
    ///
    /// `session_name` is used for `--resume` if a prior session exists.
    pub fn spawn_claude(
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

        let mut cmd = CommandBuilder::new("claude");
        cmd.arg("--resume");
        cmd.arg(session_name);
        cmd.cwd(working_dir);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("Failed to spawn claude in PTY: {}", e))?;

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
}
