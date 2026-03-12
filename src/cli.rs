//! GithubClaw CLI — init, start, stop, status, logs commands.
//!
//! Rust translation of the Python `cli.py`.

use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::config::{find_repo_root, get_log_file, get_pid_file, global_config_dir, GlobalConfig};

// ---------------------------------------------------------------------------
// Embedded default templates (compile-time via include_str!)
// ---------------------------------------------------------------------------

const DEFAULT_ORCHESTRATOR_MD: &str = include_str!("../defaults/orchestrator.md");
const DEFAULT_GLOBAL_PROMPT_MD: &str = include_str!("../defaults/global_prompt.md");
const DEFAULT_VALUE_MD: &str = include_str!("../defaults/value.md");
const DEFAULT_MEMORY_MD: &str = include_str!("../defaults/memory.md");
const DEFAULT_SPAWN_CLAUDE_SH: &str = r#"#!/usr/bin/env bash
# GithubClaw spawn template for Claude Code.
set -euo pipefail
exec claude -p \
  --dangerously-skip-permissions \
  --allowedTools "${ALLOWED_TOOLS}" \
  --disallowedTools "${DISALLOWED_TOOLS}" \
  --max-turns "${MAX_TURNS:-200}" \
  --append-system-prompt-file "${PROMPT_FILE}" \
  "$TASK_PROMPT"
"#;
const DEFAULT_SPAWN_CODEX_SH: &str = r#"#!/usr/bin/env bash
# GithubClaw spawn template for Codex CLI.
set -euo pipefail
cat "${PROMPT_FILE}" | codex exec - \
  --dangerously-bypass-approvals-and-sandbox
"#;
const DEFAULT_GITIGNORE: &str = "secrets/\nqueue/\nlogs/\nmemory.md\n";
const DEFAULT_REPO_CONFIG_YAML: &str = "# GithubClaw per-repo configuration.\n# See https://github.com/GithubClaw/githubclaw for options.\n";

// Agent definitions — embedded from defaults/agents/*.md at compile time.
const DEFAULT_AGENT_CS: &str = include_str!("../defaults/agents/cs.md");
const DEFAULT_AGENT_BUG_TRACKER: &str = include_str!("../defaults/agents/bug_tracker.md");
const DEFAULT_AGENT_LIBRARIAN: &str = include_str!("../defaults/agents/librarian.md");
const DEFAULT_AGENT_PROJECT_MANAGER: &str = include_str!("../defaults/agents/project_manager.md");
const DEFAULT_AGENT_CODER: &str = include_str!("../defaults/agents/coder.md");
const DEFAULT_AGENT_QA: &str = include_str!("../defaults/agents/qa.md");
const DEFAULT_AGENT_REVIEWER: &str = include_str!("../defaults/agents/reviewer.md");
const DEFAULT_AGENT_CONTENTS_MARKETER: &str =
    include_str!("../defaults/agents/contents_marketer.md");
const DEFAULT_AGENT_VISIONARY: &str = include_str!("../defaults/agents/visionary.md");
const DEFAULT_AGENT_SECURITY_REVIEWER: &str =
    include_str!("../defaults/agents/security_reviewer.md");

// ---------------------------------------------------------------------------
// Launchd / systemd constants
// ---------------------------------------------------------------------------

const LAUNCHD_LABEL: &str = "com.githubclaw.webhook-server";
const SYSTEMD_UNIT: &str = "githubclaw-webhook-server";

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "githubclaw",
    version = env!("CARGO_PKG_VERSION"),
    about = "Near-autonomous AI agents for open-source project management."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold the .githubclaw/ directory in the current repository
    Init,
    /// Re-scan the current repository's open issues and PRs into the bootstrap queue
    Bootstrap,
    /// Start the webhook server as a background daemon
    Start,
    /// Stop the webhook server
    Stop {
        /// Immediate kill instead of graceful drain
        #[arg(long, short)]
        force: bool,
    },
    /// Show the status of the webhook server and registered repos
    Status,
    /// Show webhook server logs
    Logs {
        /// Follow log output (like tail -f)
        #[arg(long, short)]
        follow: bool,
    },
    /// Run the webhook server inline (used by launchd/systemd)
    Serve {
        /// Host to bind to
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// Port to bind to
        #[arg(long, default_value_t = 8000)]
        port: u16,
    },
}

pub fn run() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Init => cmd_init(),
        Commands::Bootstrap => cmd_bootstrap(),
        Commands::Start => cmd_start(),
        Commands::Stop { force } => cmd_stop(force),
        Commands::Status => cmd_status(),
        Commands::Logs { follow } => cmd_logs(follow),
        Commands::Serve { host, port } => cmd_serve(&host, port),
    }
}

// ===========================================================================
// cmd_init
// ===========================================================================

fn cmd_init() {
    let repo_root = match find_repo_root(None) {
        Some(r) => r,
        None => {
            eprintln!("Error: not inside a git repository.");
            std::process::exit(1);
        }
    };

    let claw_dir = repo_root.join(".githubclaw");

    if claw_dir.exists() {
        println!(
            "Directory {} already exists. Skipping existing files.",
            claw_dir.display()
        );
    }

    // Create directory structure
    let agents_dir = claw_dir.join("agents");
    let ai_dir = claw_dir.join("ai_instructions");
    let logs_dir = claw_dir.join("logs");
    let queue_dir = claw_dir.join("queue").join("dead");

    for d in [&agents_dir, &ai_dir, &logs_dir, &queue_dir] {
        fs::create_dir_all(d).unwrap_or_else(|e| {
            eprintln!("Error creating directory {}: {e}", d.display());
            std::process::exit(1);
        });
    }

    // Files to write (path -> content). Prompt/config files are user-owned and
    // are never overwritten. Runtime spawn scripts are refreshed so existing
    // repos pick up compatible launcher behavior after upgrades.
    let files: Vec<(PathBuf, &str)> = vec![
        (claw_dir.join("orchestrator.md"), DEFAULT_ORCHESTRATOR_MD),
        (claw_dir.join("global-prompt.md"), DEFAULT_GLOBAL_PROMPT_MD),
        (claw_dir.join("VALUE.md"), DEFAULT_VALUE_MD),
        (claw_dir.join("memory.md"), DEFAULT_MEMORY_MD),
        (claw_dir.join("spawn_claude.sh"), DEFAULT_SPAWN_CLAUDE_SH),
        (claw_dir.join("spawn_codex.sh"), DEFAULT_SPAWN_CODEX_SH),
        (claw_dir.join(".gitignore"), DEFAULT_GITIGNORE),
        (claw_dir.join("config.yaml"), DEFAULT_REPO_CONFIG_YAML),
        // Agent definition files (all 10 agents)
        (agents_dir.join("cs.md"), DEFAULT_AGENT_CS),
        (agents_dir.join("bug_tracker.md"), DEFAULT_AGENT_BUG_TRACKER),
        (agents_dir.join("librarian.md"), DEFAULT_AGENT_LIBRARIAN),
        (
            agents_dir.join("project_manager.md"),
            DEFAULT_AGENT_PROJECT_MANAGER,
        ),
        (agents_dir.join("coder.md"), DEFAULT_AGENT_CODER),
        (agents_dir.join("qa.md"), DEFAULT_AGENT_QA),
        (agents_dir.join("reviewer.md"), DEFAULT_AGENT_REVIEWER),
        (
            agents_dir.join("contents_marketer.md"),
            DEFAULT_AGENT_CONTENTS_MARKETER,
        ),
        (agents_dir.join("visionary.md"), DEFAULT_AGENT_VISIONARY),
        (
            agents_dir.join("security_reviewer.md"),
            DEFAULT_AGENT_SECURITY_REVIEWER,
        ),
    ];

    let mut created: usize = 0;
    let mut skipped: usize = 0;
    let mut refreshed: usize = 0;

    for (filepath, content) in &files {
        let existed = filepath.exists();
        let is_spawn_script = filepath
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == "spawn_claude.sh" || n == "spawn_codex.sh")
            .unwrap_or(false);

        if existed && !is_spawn_script {
            skipped += 1;
            continue;
        }
        if let Some(parent) = filepath.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(filepath, content) {
            eprintln!("Error writing {}: {e}", filepath.display());
        } else {
            if existed && is_spawn_script {
                refreshed += 1;
            } else {
                created += 1;
            }
        }
    }

    // Make spawn scripts executable (chmod 755)
    for script_name in &["spawn_claude.sh", "spawn_codex.sh"] {
        let script = claw_dir.join(script_name);
        if script.exists() {
            let _ = fs::set_permissions(&script, fs::Permissions::from_mode(0o755));
        }
    }

    println!("Initialized .githubclaw/ in {}", repo_root.display());
    println!(
        "  Created {created} files, refreshed {refreshed} runtime scripts, skipped {skipped} existing files."
    );
    println!();

    // (a) Auto-register the repo
    register_repo(&repo_root);

    // (b) One-time webhook secret setup
    setup_webhook_secret();
    setup_registration_secret();

    // (c) Backend detection
    detect_backends();

    // (d) Pre-flight check for gh CLI
    preflight_gh();

    // (e) Guidance on what to git add
    println!();
    println!("To track agent configs in git:");
    println!(
        "  git add .githubclaw/agents/ .githubclaw/VALUE.md \
         .githubclaw/global-prompt.md .githubclaw/orchestrator.md"
    );

    // (f) Next steps
    println!();
    println!("Next steps:");
    println!("  1. Edit .githubclaw/VALUE.md with your project mission");
    println!("  2. Create a GitHub App and set webhook URL + secret");
    println!("  3. Set up a tunnel (cloudflare tunnel, ngrok, etc.)");
    println!("  4. githubclaw start");
}

fn cmd_bootstrap() {
    use crate::process_manager::ProcessManager;
    use crate::scheduler::ScheduledEventManager;
    use crate::server::{bootstrap_repo, load_registry, ServerState};
    use std::collections::HashSet;
    use tokio::sync::{Mutex, RwLock};

    let repo_root = match find_repo_root(None) {
        Some(r) => r,
        None => {
            eprintln!("Error: not inside a git repository.");
            std::process::exit(1);
        }
    };

    let output = match Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(&repo_root)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Error running git remote get-url origin: {e}");
            std::process::exit(1);
        }
    };
    if !output.status.success() {
        eprintln!("Error: no 'origin' remote found.");
        std::process::exit(1);
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let owner_repo = match parse_github_remote(&remote_url) {
        Some(or) => or,
        None => {
            eprintln!("Error: could not parse GitHub owner/repo from: {remote_url}");
            std::process::exit(1);
        }
    };

    let global_dir = global_config_dir();
    let registry = load_registry(&global_dir.join("registry.json"));
    let entry = match registry.get(&owner_repo).cloned() {
        Some(entry) => entry,
        None => {
            eprintln!("Error: repo {owner_repo} is not registered. Run `githubclaw init` first.");
            std::process::exit(1);
        }
    };

    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| {
        eprintln!("Failed to create tokio runtime: {e}");
        std::process::exit(1);
    });

    rt.block_on(async move {
        let scheduler_path = global_dir.join("scheduled_events.json");
        let state = Arc::new(ServerState {
            webhook_secret: String::new(),
            registry: RwLock::new(registry),
            started_repos: RwLock::new(HashSet::new()),
            queues: Mutex::new(HashMap::new()),
            githubclaw_home: global_dir.clone(),
            registration: crate::registration::RegistrationState::new_for_tests(
                global_dir.join("hosted_proxy").join("installations.json"),
                "bootstrap-registration-secret",
            )
            .unwrap(),
            process_manager: Arc::new(ProcessManager::new(1)),
            scheduler: Mutex::new(ScheduledEventManager::new(&scheduler_path)),
            rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::default()),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            orchestrator_backend: crate::orchestrator::session::OrchestratorBackend::Codex,
            orchestrators: Mutex::new(HashMap::new()),
        });

        match bootstrap_repo(&state, &owner_repo, &entry, true).await {
            Ok(()) => println!("Bootstrapped open issues/PRs for {owner_repo}."),
            Err(e) => {
                eprintln!("Bootstrap failed for {owner_repo}: {e}");
                std::process::exit(1);
            }
        }
    });
}

// ===========================================================================
// cmd_start
// ===========================================================================

fn cmd_start() {
    if let Some(pid) = read_pid() {
        eprintln!("Webhook server already running (PID {pid}).");
        std::process::exit(1);
    }

    // Ensure global directories exist
    let global_dir = global_config_dir();
    let _ = fs::create_dir_all(global_dir.join("logs"));
    let _ = fs::create_dir_all(global_dir.join("secrets"));

    let config = GlobalConfig::load(None).unwrap_or_else(|e| {
        eprintln!("Error loading config: {e}");
        std::process::exit(1);
    });

    // Save default global config if it doesn't exist
    if !global_dir.join("config.yaml").exists() {
        let _ = config.save(None);
    }

    let log_path = get_log_file();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let system = std::env::consts::OS;

    match system {
        "macos" => {
            let plist_path = write_launchd_plist(LAUNCHD_LABEL, config.port, &log_path);
            let uid = unsafe { libc::getuid() };

            // Unload stale definition first
            let _ = Command::new("launchctl")
                .args([
                    "bootout",
                    &format!("gui/{uid}"),
                    &plist_path.to_string_lossy(),
                ])
                .output();

            let result = Command::new("launchctl")
                .args([
                    "bootstrap",
                    &format!("gui/{uid}"),
                    &plist_path.to_string_lossy(),
                ])
                .output();

            match result {
                Ok(output) if !output.status.success() => {
                    eprintln!(
                        "Failed to start via launchd: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Failed to run launchctl: {e}");
                    std::process::exit(1);
                }
                _ => {}
            }

            // Find PID from launchctl print
            if let Ok(info) = Command::new("launchctl")
                .args(["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")])
                .output()
            {
                let stdout = String::from_utf8_lossy(&info.stdout);
                for line in stdout.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("pid =") {
                        if let Some(val) = trimmed.split('=').nth(1) {
                            if let Ok(pid) = val.trim().parse::<u32>() {
                                let _ = fs::write(get_pid_file(), pid.to_string());
                            }
                        }
                    }
                }
            }

            println!(
                "Webhook server started via launchd on port {}.",
                config.port
            );
            println!("  Logs: {}", log_path.display());
            println!("  Plist: {}", plist_path.display());
        }
        "linux" => {
            let unit_path = write_systemd_unit(SYSTEMD_UNIT, config.port, &log_path);

            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .output();

            let result = Command::new("systemctl")
                .args(["--user", "start", SYSTEMD_UNIT])
                .output();

            match result {
                Ok(output) if !output.status.success() => {
                    eprintln!(
                        "Failed to start via systemd: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Failed to run systemctl: {e}");
                    std::process::exit(1);
                }
                _ => {}
            }

            // Get PID from systemd
            if let Ok(pid_output) = Command::new("systemctl")
                .args([
                    "--user",
                    "show",
                    SYSTEMD_UNIT,
                    "--property=MainPID",
                    "--value",
                ])
                .output()
            {
                let s = String::from_utf8_lossy(&pid_output.stdout);
                if let Ok(pid) = s.trim().parse::<u32>() {
                    if pid > 0 {
                        let _ = fs::write(get_pid_file(), pid.to_string());
                    }
                }
            }

            println!(
                "Webhook server started via systemd on port {}.",
                config.port
            );
            println!("  Logs: {}", log_path.display());
            println!("  Unit: {}", unit_path.display());
        }
        _ => {
            eprintln!("Unsupported platform: {system}. Only macOS and Linux are supported.");
            std::process::exit(1);
        }
    }

    // Health check
    health_check(config.port, &log_path);
}

// ===========================================================================
// cmd_stop
// ===========================================================================

fn cmd_stop(force: bool) {
    let system = std::env::consts::OS;

    if force {
        stop_force(system);
    } else {
        stop_graceful(system);
    }
}

fn stop_graceful(system: &str) {
    let pid = read_pid();

    match system {
        "macos" => {
            let plist_path = home_dir()
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist"));
            if plist_path.exists() {
                // Send SIGTERM for graceful drain
                if let Some(pid) = pid {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGTERM);
                    }
                }
                let uid = unsafe { libc::getuid() };
                let result = Command::new("launchctl")
                    .args([
                        "bootout",
                        &format!("gui/{uid}"),
                        &plist_path.to_string_lossy(),
                    ])
                    .output();

                let _ = fs::remove_file(get_pid_file());

                match result {
                    Ok(output)
                        if output.status.success()
                            || String::from_utf8_lossy(&output.stderr)
                                .contains("No such process") =>
                    {
                        println!("Webhook server stopped (graceful drain).");
                    }
                    Ok(output) => {
                        println!(
                            "launchctl bootout warning: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        );
                    }
                    Err(e) => {
                        eprintln!("Failed to run launchctl: {e}");
                    }
                }
                return;
            }
        }
        "linux" => {
            let result = Command::new("systemctl")
                .args(["--user", "stop", SYSTEMD_UNIT])
                .output();

            let _ = fs::remove_file(get_pid_file());

            match result {
                Ok(output) if output.status.success() => {
                    println!("Webhook server stopped (graceful drain).");
                }
                Ok(output) => {
                    println!(
                        "systemctl stop warning: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Err(e) => {
                    eprintln!("Failed to run systemctl: {e}");
                }
            }
            return;
        }
        _ => {}
    }

    // Fallback: direct PID-based stop
    match pid {
        Some(pid) => {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let _ = fs::remove_file(get_pid_file());
            println!("Webhook server stopped (graceful drain).");
        }
        None => {
            eprintln!("Webhook server is not running.");
            std::process::exit(1);
        }
    }
}

fn stop_force(system: &str) {
    let pid = read_pid();

    match system {
        "macos" => {
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
            let plist_path = home_dir()
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist"));
            if plist_path.exists() {
                let uid = unsafe { libc::getuid() };
                let _ = Command::new("launchctl")
                    .args([
                        "bootout",
                        &format!("gui/{uid}"),
                        &plist_path.to_string_lossy(),
                    ])
                    .output();
            }
            let _ = fs::remove_file(get_pid_file());
            println!("Webhook server killed (force).");
        }
        "linux" => {
            let result = Command::new("systemctl")
                .args(["--user", "kill", "--signal=KILL", SYSTEMD_UNIT])
                .output();

            let _ = Command::new("systemctl")
                .args(["--user", "stop", SYSTEMD_UNIT])
                .output();

            let _ = fs::remove_file(get_pid_file());

            match result {
                Ok(output) if output.status.success() => {
                    println!("Webhook server killed (force).");
                }
                Ok(output) => {
                    println!(
                        "systemctl kill warning: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    );
                }
                Err(e) => {
                    eprintln!("Failed to run systemctl: {e}");
                }
            }
        }
        _ => {
            // Fallback: direct PID-based kill
            match pid {
                Some(pid) => {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                    let _ = fs::remove_file(get_pid_file());
                    println!("Webhook server killed (force).");
                }
                None => {
                    eprintln!("Webhook server is not running.");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ===========================================================================
// cmd_status
// ===========================================================================

fn cmd_status() {
    // Check if server is running
    match read_pid() {
        Some(pid) => {
            println!("Webhook server is running (PID {pid}).");
            if let Ok(config) = GlobalConfig::load(None) {
                println!("  Port: {}", config.port);
            }
        }
        None => {
            println!("Webhook server is not running.");
        }
    }

    // Show registered repos
    let registry_path = global_config_dir().join("registry.json");
    if !registry_path.exists() {
        println!();
        println!("No repos registered (registry.json not found).");
        return;
    }

    let registry_contents = match fs::read_to_string(&registry_path) {
        Ok(c) => c,
        Err(_) => {
            println!();
            println!("Registry file is corrupted.");
            return;
        }
    };

    let registry: serde_json::Value = match serde_json::from_str(&registry_contents) {
        Ok(v) => v,
        Err(_) => {
            println!();
            println!("Registry file is corrupted.");
            return;
        }
    };

    let repos = match registry.get("repos").and_then(|r| r.as_object()) {
        Some(r) if !r.is_empty() => r,
        _ => {
            println!();
            println!("No repos registered.");
            return;
        }
    };

    println!();
    println!("Registered repos ({}):", repos.len());
    for (repo_name, info) in repos {
        let local_path = info
            .get("local_path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!("  {repo_name}");
        println!("    Path: {local_path}");

        // Show queue size if queue dir exists
        let queue_dir = Path::new(local_path).join(".githubclaw").join("queue");
        if queue_dir.exists() {
            let queue_count = fs::read_dir(&queue_dir)
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path().is_file()
                                && e.path().extension().is_some_and(|ext| ext == "json")
                        })
                        .count()
                })
                .unwrap_or(0);
            println!("    Queue: {queue_count} item(s)");
        }
    }
}

// ===========================================================================
// cmd_logs
// ===========================================================================

fn cmd_logs(follow: bool) {
    let log_path = get_log_file();
    if !log_path.exists() {
        println!("No logs found.");
        return;
    }

    if follow {
        // Replace process with tail -f
        use std::os::unix::process::CommandExt;
        let err = Command::new("tail")
            .args(["-f", &log_path.to_string_lossy()])
            .exec();
        eprintln!("Failed to exec tail: {err}");
        std::process::exit(1);
    } else {
        // Print last 50 lines
        match fs::read_to_string(&log_path) {
            Ok(contents) => {
                let lines: Vec<&str> = contents.lines().collect();
                let start = if lines.len() > 50 {
                    lines.len() - 50
                } else {
                    0
                };
                for line in &lines[start..] {
                    println!("{line}");
                }
            }
            Err(e) => {
                eprintln!("Error reading log file: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ===========================================================================
// cmd_serve — runs the axum server inline (called by launchd/systemd)
// ===========================================================================

fn cmd_serve(host: &str, port: u16) {
    use crate::process_manager::ProcessManager;
    use crate::scheduler::ScheduledEventManager;
    use crate::server::{
        bootstrap_repo, create_router, load_registry, load_webhook_secret, ServerState,
    };
    use std::collections::HashSet;
    use tokio::sync::{Mutex, RwLock};

    // Build the tokio runtime for the async server
    let rt = tokio::runtime::Runtime::new().unwrap_or_else(|e| {
        eprintln!("Failed to create tokio runtime: {e}");
        std::process::exit(1);
    });

    rt.block_on(async {
        // Initialize tracing
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();

        let global_dir = global_config_dir();

        // Load config
        let config = GlobalConfig::load(None).unwrap_or_else(|e| {
            eprintln!("Error loading config: {e}");
            std::process::exit(1);
        });

        // Load webhook secret
        let secret_path = global_dir.join("secrets").join("webhook_secret");
        let webhook_secret = load_webhook_secret(&secret_path).unwrap_or_else(|e| {
            eprintln!("Error loading webhook secret: {e}");
            std::process::exit(1);
        });
        let registration_secret_path = global_dir.join("secrets").join("registration_secret");
        ensure_secret_file(&registration_secret_path, "registration", false);
        let registration_secret =
            load_webhook_secret(&registration_secret_path).unwrap_or_else(|e| {
                eprintln!("Error loading registration secret: {e}");
                std::process::exit(1);
            });

        // Load registry
        let registry_path = global_dir.join("registry.json");
        let registry = load_registry(&registry_path);

        if registry.is_empty() {
            tracing::warn!("No repos registered. Run `githubclaw init` in a repo first.");
        }

        // Load scheduler
        let scheduler_path = global_dir.join("scheduled_events.json");
        let mut scheduler = ScheduledEventManager::new(&scheduler_path);
        if let Err(e) = scheduler.load() {
            tracing::warn!("Failed to load scheduled events: {}", e);
        }

        // Create server state
        let state = Arc::new(ServerState {
            webhook_secret,
            registry: RwLock::new(registry.clone()),
            started_repos: RwLock::new(HashSet::new()),
            queues: Mutex::new(HashMap::new()),
            githubclaw_home: global_dir.clone(),
            registration: crate::registration::RegistrationState::load(
                global_dir.join("hosted_proxy").join("installations.json"),
                registration_secret,
            )
            .unwrap_or_else(|e| {
                eprintln!("Error loading installation registrations: {e}");
                std::process::exit(1);
            }),
            process_manager: Arc::new(ProcessManager::new(config.max_concurrent_agents)),
            scheduler: Mutex::new(scheduler),
            rate_limiter: Arc::new(crate::rate_limiter::RateLimiter::default()),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            orchestrator_backend: crate::orchestrator::session::OrchestratorBackend::Codex,
            orchestrators: Mutex::new(HashMap::new()),
        });

        // Bootstrap repos: scan existing open issues/PRs for each repo
        for (repo_name, entry) in &registry {
            if let Err(e) = bootstrap_repo(&state, repo_name, entry, false).await {
                tracing::warn!("Bootstrap failed for {}: {}", repo_name, e);
            }
        }

        // Start the process monitor in background
        let _monitor_handle = state.process_manager.start_monitor();

        // Start the scheduler firing loop in background
        {
            let sched_state = Arc::clone(&state);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                    crate::constants::SCHEDULER_CHECK_INTERVAL_SECONDS,
                ));
                loop {
                    interval.tick().await;
                    if sched_state
                        .shutdown
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        break;
                    }
                    let mut scheduler = sched_state.scheduler.lock().await;
                    let sched_state_inner = Arc::clone(&sched_state);
                    scheduler
                        .fire_due_events_with_callback(|repo, payload| {
                            let st = Arc::clone(&sched_state_inner);
                            async move {
                                let mut queues = st.queues.lock().await;
                                let registry = st.registry.read().await;
                                let queue = crate::server::get_or_create_queue_pub(
                                    &mut queues,
                                    &registry,
                                    &st.githubclaw_home,
                                    &repo,
                                )
                                .map_err(|e| e.to_string())?;
                                queue
                                    .enqueue(
                                        serde_json::json!({
                                            "type": "scheduled_fired",
                                            "scheduled_payload": payload,
                                        }),
                                        "scheduled_fired",
                                    )
                                    .map_err(|e| e.to_string())?;
                                Ok(())
                            }
                        })
                        .await;
                }
            });
        }

        // Set up shutdown flag handler
        {
            let shutdown_flag = state.shutdown.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            });
        }

        // Start per-repo event drain loops + rate limiter recovery probe
        crate::server::start_event_processing(Arc::clone(&state)).await;

        // Build router
        let app = create_router(Arc::clone(&state));

        // Bind and serve
        let bind_addr = format!("{host}:{port}");
        tracing::info!("GithubClaw webhook server listening on {}", bind_addr);

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Failed to bind to {}: {}", bind_addr, e);
                std::process::exit(1);
            });

        // Graceful shutdown on SIGTERM/SIGINT
        let shutdown_signal = async {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            let sigint = tokio::signal::ctrl_c();
            tokio::select! {
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM, shutting down...");
                }
                _ = sigint => {
                    tracing::info!("Received SIGINT, shutting down...");
                }
            }
        };

        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal)
            .await
            .unwrap_or_else(|e| {
                eprintln!("Server error: {e}");
                std::process::exit(1);
            });

        tracing::info!("Server shut down.");
    });
}

// ===========================================================================
// Helper functions
// ===========================================================================

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Parse `owner/repo` from a GitHub remote URL.
///
/// Supports:
///   - `git@github.com:owner/repo.git`
///   - `https://github.com/owner/repo.git`
///   - `https://github.com/owner/repo`
fn parse_github_remote(url: &str) -> Option<String> {
    let re_ssh = regex::Regex::new(r"^git@github\.com:([^/]+)/([^/]+?)(?:\.git)?$").ok()?;
    if let Some(caps) = re_ssh.captures(url) {
        return Some(format!("{}/{}", &caps[1], &caps[2]));
    }

    let re_https = regex::Regex::new(r"^https://github\.com/([^/]+)/([^/]+?)(?:\.git)?$").ok()?;
    if let Some(caps) = re_https.captures(url) {
        return Some(format!("{}/{}", &caps[1], &caps[2]));
    }

    None
}

/// Auto-register the repo in `~/.githubclaw/registry.json`.
fn register_repo(repo_root: &Path) {
    let output = match Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
    {
        Ok(o) => o,
        Err(_) => {
            println!("  Warning: could not run git; skipping registry.");
            return;
        }
    };

    if !output.status.success() {
        println!("  Warning: no 'origin' remote found; skipping registry.");
        return;
    }

    let remote_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let owner_repo = match parse_github_remote(&remote_url) {
        Some(or) => or,
        None => {
            println!("  Warning: could not parse GitHub owner/repo from: {remote_url}");
            return;
        }
    };

    let repo_name = owner_repo.split('/').next_back().unwrap_or(&owner_repo);
    let global_dir = global_config_dir();
    let _ = fs::create_dir_all(&global_dir);
    let registry_path = global_dir.join("registry.json");

    let mut registry: serde_json::Value = if registry_path.exists() {
        fs::read_to_string(&registry_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({"repos": {}}))
    } else {
        serde_json::json!({"repos": {}})
    };

    if registry.get("repos").is_none() {
        registry["repos"] = serde_json::json!({});
    }

    let resolved = fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());

    registry["repos"][&owner_repo] = serde_json::json!({
        "local_path": resolved.to_string_lossy(),
        "socket_path": format!("/tmp/githubclaw-{repo_name}.sock"),
    });

    let json_str = serde_json::to_string_pretty(&registry).unwrap_or_default();
    let _ = fs::write(&registry_path, format!("{json_str}\n"));

    println!("  Registered {owner_repo} in ~/.githubclaw/registry.json");
}

/// One-time webhook secret setup.
fn setup_webhook_secret() {
    let secrets_dir = global_config_dir().join("secrets");
    let _ = fs::create_dir_all(&secrets_dir);
    let secret_path = secrets_dir.join("webhook_secret");
    ensure_secret_file(&secret_path, "webhook", true);
}

fn setup_registration_secret() {
    let secrets_dir = global_config_dir().join("secrets");
    let _ = fs::create_dir_all(&secrets_dir);
    let secret_path = secrets_dir.join("registration_secret");
    ensure_secret_file(&secret_path, "registration", true);
}

fn ensure_secret_file(path: &Path, label: &str, announce_existing: bool) {
    if path.exists() {
        if announce_existing {
            println!("  {} secret already configured.", capitalize(label));
        }
        return;
    }

    let mut buf = [0u8; 32];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            let secret = hex::encode(buf);
            if fs::write(path, &secret).is_ok() {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
                println!("  Generated {} secret at {}", label, path.display());
                if label == "webhook" {
                    println!("  Use this secret when creating your GitHub App webhook.");
                }
                return;
            }
        }
    }
    eprintln!("  Warning: could not generate {} secret.", label);
}

fn capitalize(input: &str) -> String {
    let mut chars = input.chars();
    match chars.next() {
        Some(first) => {
            let mut capitalized = first.to_uppercase().collect::<String>();
            capitalized.push_str(chars.as_str());
            capitalized
        }
        None => String::new(),
    }
}

/// Check which agent backends are available.
fn detect_backends() {
    let claude_found = which("claude");
    let codex_found = which("codex");

    let mut available = Vec::new();
    if claude_found {
        available.push("claude");
    }
    if codex_found {
        available.push("codex");
    }

    if available.is_empty() {
        println!(
            "  Warning: Neither claude nor codex CLI found. \
             Install one before running agents."
        );
    } else {
        println!("  Available backends: {}", available.join(", "));
    }
}

/// Pre-flight check for gh CLI.
fn preflight_gh() {
    if !which("gh") {
        println!("  Warning: gh CLI not found. Install it: https://cli.github.com");
        return;
    }

    let result = Command::new("gh").args(["auth", "status"]).output();
    match result {
        Ok(output) if output.status.success() => {
            println!("  gh CLI authenticated.");
        }
        _ => {
            println!("  Warning: gh CLI not authenticated. Run: gh auth login");
        }
    }
}

/// Check if a binary is on PATH.
fn which(binary: &str) -> bool {
    Command::new("which")
        .arg(binary)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Read PID from the PID file, returning None if not present or if the process
/// is not alive.
fn read_pid() -> Option<u32> {
    let pid_file = get_pid_file();
    if !pid_file.exists() {
        return None;
    }

    let contents = fs::read_to_string(&pid_file).ok()?;
    let pid: u32 = contents.trim().parse().ok()?;

    // Check if process is alive (signal 0)
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    if alive {
        Some(pid)
    } else {
        let _ = fs::remove_file(&pid_file);
        None
    }
}

/// Write a macOS launchd plist and return its path.
fn write_launchd_plist(label: &str, port: u16, log_path: &Path) -> PathBuf {
    let plist_dir = home_dir().join("Library").join("LaunchAgents");
    let _ = fs::create_dir_all(&plist_dir);
    let plist_path = plist_dir.join(format!("{label}.plist"));

    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("githubclaw"));
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());

    let plist_content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
        <string>--host</string>
        <string>0.0.0.0</string>
        <string>--port</string>
        <string>{port}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>
</dict>
</plist>
"#,
        exe = exe_path.display(),
        log = log_path.display(),
        path = path_env,
    );

    let _ = fs::write(&plist_path, plist_content);
    plist_path
}

/// Write a Linux systemd user unit file and return its path.
fn write_systemd_unit(unit_name: &str, port: u16, log_path: &Path) -> PathBuf {
    let unit_dir = home_dir().join(".config").join("systemd").join("user");
    let _ = fs::create_dir_all(&unit_dir);
    let unit_path = unit_dir.join(format!("{unit_name}.service"));

    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("githubclaw"));
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into());

    let unit_content = format!(
        r#"[Unit]
Description=GithubClaw Webhook Server
After=network.target

[Service]
Type=simple
ExecStart={exe} serve --host 0.0.0.0 --port {port}
Restart=on-failure
RestartSec=5
StandardOutput=append:{log}
StandardError=append:{log}
Environment=PATH={path}

[Install]
WantedBy=default.target
"#,
        exe = exe_path.display(),
        log = log_path.display(),
        path = path_env,
    );

    let _ = fs::write(&unit_path, unit_content);
    unit_path
}

/// Simple health check after startup — retry up to 3 times.
fn health_check(port: u16, log_path: &Path) {
    std::thread::sleep(std::time::Duration::from_secs(2));

    for attempt in 0..3 {
        let result = Command::new("curl")
            .args([
                "-sf",
                "--max-time",
                "5",
                &format!("http://127.0.0.1:{port}/health"),
            ])
            .output();

        if let Ok(output) = result {
            if output.status.success() {
                println!("  Server is healthy.");
                return;
            }
        }

        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    println!(
        "  Warning: Server may not have started correctly. Check logs: {}",
        log_path.display()
    );
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_remote_ssh() {
        let result = parse_github_remote("git@github.com:octocat/Hello-World.git");
        assert_eq!(result, Some("octocat/Hello-World".to_string()));
    }

    #[test]
    fn test_parse_github_remote_https() {
        let result = parse_github_remote("https://github.com/octocat/Hello-World.git");
        assert_eq!(result, Some("octocat/Hello-World".to_string()));
    }

    #[test]
    fn test_parse_github_remote_https_no_git_suffix() {
        let result = parse_github_remote("https://github.com/octocat/Hello-World");
        assert_eq!(result, Some("octocat/Hello-World".to_string()));
    }

    #[test]
    fn test_parse_github_remote_invalid() {
        assert_eq!(parse_github_remote("not-a-url"), None);
        assert_eq!(parse_github_remote("https://gitlab.com/owner/repo"), None);
        assert_eq!(parse_github_remote(""), None);
    }
}
