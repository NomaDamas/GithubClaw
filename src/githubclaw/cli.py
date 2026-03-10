"""GithubClaw CLI - init, start, stop, status, logs commands."""

from __future__ import annotations

import contextlib
import importlib.resources
import json
import os
import platform
import re
import secrets
import shutil
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import typer

from githubclaw import __version__
from githubclaw.config import (
    GLOBAL_CONFIG_DIR,
    GlobalConfig,
    find_repo_root,
    get_log_file,
    get_pid_file,
)


def _load_default_template(filename: str, fallback: str = "") -> str:
    """Load a template from the packaged defaults/ directory."""
    try:
        ref = importlib.resources.files("githubclaw.agents.defaults") / filename
        return ref.read_text(encoding="utf-8")
    except (FileNotFoundError, TypeError):
        return fallback


def _load_ai_instruction(filename: str) -> str:
    """Load an AI instruction file from the packaged defaults/ai_instructions/ directory."""
    try:
        ref = importlib.resources.files("githubclaw.agents.defaults") / "ai_instructions" / filename
        return ref.read_text(encoding="utf-8")
    except (FileNotFoundError, TypeError):
        return ""


def _list_default_agent_files() -> list[str]:
    """List agent .md files from defaults/, excluding known non-agent files."""
    non_agent_files = {
        "global_prompt",
        "orchestrator_template",
        "value_template",
        "memory_template",
    }
    try:
        defaults = importlib.resources.files("githubclaw.agents.defaults")
        return sorted(
            r.name
            for r in defaults.iterdir()
            if r.name.endswith(".md") and r.name.removesuffix(".md") not in non_agent_files
        )
    except (TypeError, FileNotFoundError):
        return []


def _list_ai_instruction_files() -> list[str]:
    """List AI instruction .md files from defaults/ai_instructions/."""
    try:
        ai_dir = importlib.resources.files("githubclaw.agents.defaults") / "ai_instructions"
        return sorted(r.name for r in ai_dir.iterdir() if r.name.endswith(".md"))
    except (TypeError, FileNotFoundError):
        return []


def _parse_github_remote(url: str) -> str | None:
    """Parse owner/repo from a GitHub remote URL.

    Supports:
      - git@github.com:owner/repo.git
      - https://github.com/owner/repo.git
      - https://github.com/owner/repo
    """
    # SSH format
    m = re.match(r"git@github\.com:([^/]+)/([^/]+?)(?:\.git)?$", url)
    if m:
        return f"{m.group(1)}/{m.group(2)}"
    # HTTPS format
    m = re.match(r"https://github\.com/([^/]+)/([^/]+?)(?:\.git)?$", url)
    if m:
        return f"{m.group(1)}/{m.group(2)}"
    return None


def _register_repo(repo_root: Path) -> None:
    """Auto-register the repo in ~/.githubclaw/registry.json."""
    result = subprocess.run(
        ["git", "remote", "get-url", "origin"],
        capture_output=True,
        text=True,
        cwd=repo_root,
    )
    if result.returncode != 0:
        typer.echo("  Warning: no 'origin' remote found; skipping registry.")
        return

    remote_url = result.stdout.strip()
    owner_repo = _parse_github_remote(remote_url)
    if owner_repo is None:
        typer.echo(f"  Warning: could not parse GitHub owner/repo from: {remote_url}")
        return

    repo_name = owner_repo.split("/")[-1]
    registry_path = GLOBAL_CONFIG_DIR / "registry.json"
    GLOBAL_CONFIG_DIR.mkdir(parents=True, exist_ok=True)

    registry: dict[str, dict[str, dict[str, str]]] = {"repos": {}}
    if registry_path.exists():
        with open(registry_path) as f, contextlib.suppress(json.JSONDecodeError):
            loaded = json.load(f)
            if isinstance(loaded, dict):
                registry = loaded
        if "repos" not in registry:
            registry["repos"] = {}

    registry["repos"][owner_repo] = {
        "local_path": str(repo_root.resolve()),
        "socket_path": f"/tmp/githubclaw-{repo_name}.sock",
    }

    with open(registry_path, "w") as f:
        json.dump(registry, f, indent=2)
        f.write("\n")

    typer.echo(f"  Registered {owner_repo} in ~/.githubclaw/registry.json")


def _setup_webhook_secret() -> None:
    """One-time webhook secret setup."""
    secrets_dir = GLOBAL_CONFIG_DIR / "secrets"
    secrets_dir.mkdir(parents=True, exist_ok=True)
    secret_path = secrets_dir / "webhook_secret"

    if secret_path.exists():
        typer.echo("  Webhook secret already configured.")
        return

    secret = secrets.token_hex(32)
    secret_path.write_text(secret)
    secret_path.chmod(0o600)
    typer.echo("  Generated webhook secret at ~/.githubclaw/secrets/webhook_secret")
    typer.echo("  Use this secret when creating your GitHub App webhook.")


def _detect_backends() -> None:
    """Check which agent backends are available."""
    claude_found = shutil.which("claude") is not None
    codex_found = shutil.which("codex") is not None

    available = []
    if claude_found:
        available.append("claude")
    if codex_found:
        available.append("codex")

    if available:
        typer.echo(f"  Available backends: {', '.join(available)}")
    else:
        typer.echo(
            "  Warning: Neither claude nor codex CLI found. "
            "Install one before running agents."
        )


def _preflight_gh() -> None:
    """Pre-flight check for gh CLI."""
    if shutil.which("gh") is None:
        typer.echo("  Warning: gh CLI not found. Install it: https://cli.github.com")
        return

    result = subprocess.run(["gh", "auth", "status"], capture_output=True)
    if result.returncode != 0:
        typer.echo("  Warning: gh CLI not authenticated. Run: gh auth login")
    else:
        typer.echo("  gh CLI authenticated.")


app = typer.Typer(
    name="githubclaw",
    help="Near-autonomous AI agents for open-source project management.",
    no_args_is_help=True,
)


def _version_callback(value: bool) -> None:
    if value:
        typer.echo(f"githubclaw {__version__}")
        raise typer.Exit()


@app.callback()
def main(
    version: bool = typer.Option(
        False,
        "--version",
        "-v",
        help="Show version and exit.",
        callback=_version_callback,
        is_eager=True,
    ),
) -> None:
    """GithubClaw CLI."""


@app.command()
def init() -> None:
    """Scaffold the .githubclaw/ directory in the current repository."""
    repo_root = find_repo_root()
    if repo_root is None:
        typer.echo("Error: not inside a git repository.", err=True)
        raise typer.Exit(code=1)

    claw_dir = repo_root / ".githubclaw"

    if claw_dir.exists():
        typer.echo(f"Directory {claw_dir} already exists. Skipping existing files.")

    # Create directory structure
    agents_dir = claw_dir / "agents"
    ai_dir = claw_dir / "ai_instructions"
    logs_dir = claw_dir / "logs"
    queue_dir = claw_dir / "queue" / "dead"

    for d in [agents_dir, ai_dir, logs_dir, queue_dir]:
        d.mkdir(parents=True, exist_ok=True)

    # Write files, never overwriting existing user customizations
    files: dict[Path, str] = {
        claw_dir / "orchestrator.md": _load_default_template(
            "orchestrator_template.md", "# Orchestrator System Prompt\n"
        ),
        claw_dir / "global-prompt.md": _load_default_template(
            "global_prompt.md", "# GithubClaw Agent Roster & Common Rules\n"
        ),
        claw_dir / "VALUE.md": _load_default_template(
            "value_template.md", "# Project Value Statement\n"
        ),
        claw_dir / "memory.md": _load_default_template(
            "memory_template.md", "# Orchestrator Memory\n"
        ),
        claw_dir / "spawn_claude.sh": _load_default_template(
            "spawn_claude.sh",
            "#!/usr/bin/env bash\n# GithubClaw spawn template for Claude Code.\n",
        ),
        claw_dir / "spawn_codex.sh": _load_default_template(
            "spawn_codex.sh",
            "#!/usr/bin/env bash\n# GithubClaw spawn template for Codex CLI.\n",
        ),
        claw_dir / ".gitignore": _load_default_template(
            "gitignore_template", "secrets/\nqueue/\nlogs/\nmemory.md\n"
        ),
        claw_dir / "config.yaml": _load_default_template(
            "repo_config.yaml", "# GithubClaw per-repo configuration.\n"
        ),
    }

    # Agent definition files — scan defaults/ for all agent .md files
    for agent_filename in _list_default_agent_files():
        agent_name = agent_filename.removesuffix(".md")
        agent_path = agents_dir / agent_filename
        files[agent_path] = _load_default_template(
            agent_filename,
            f"# {agent_name.replace('_', ' ').title()} Agent\n",
        )

    # AI instruction files — scan defaults/ai_instructions/
    for instr_filename in _list_ai_instruction_files():
        instr_path = ai_dir / instr_filename
        files[instr_path] = _load_ai_instruction(instr_filename)

    created = 0
    skipped = 0
    for filepath, content in files.items():
        if filepath.exists():
            skipped += 1
            continue
        filepath.parent.mkdir(parents=True, exist_ok=True)
        filepath.write_text(content)
        created += 1

    # Make spawn scripts executable
    for script in [claw_dir / "spawn_claude.sh", claw_dir / "spawn_codex.sh"]:
        if script.exists():
            script.chmod(0o755)

    typer.echo(f"Initialized .githubclaw/ in {repo_root}")
    typer.echo(f"  Created {created} files, skipped {skipped} existing files.")
    typer.echo("")

    # (a) Auto-register the repo
    _register_repo(repo_root)

    # (b) One-time webhook secret setup
    _setup_webhook_secret()

    # (c) Backend detection
    _detect_backends()

    # (d) Pre-flight check for gh CLI
    _preflight_gh()

    # (e) Guidance on what to git add
    typer.echo("")
    typer.echo("To track agent configs in git:")
    typer.echo(
        "  git add .githubclaw/agents/ .githubclaw/VALUE.md "
        ".githubclaw/global-prompt.md .githubclaw/orchestrator.md"
    )

    # (f) Updated next steps
    typer.echo("")
    typer.echo("Next steps:")
    typer.echo("  1. Edit .githubclaw/VALUE.md with your project mission")
    typer.echo("  2. Create a GitHub App and set webhook URL + secret")
    typer.echo("  3. Set up a tunnel (cloudflare tunnel, ngrok, etc.)")
    typer.echo("  4. githubclaw start")


# ---------------------------------------------------------------------------
# Daemon management helpers
# ---------------------------------------------------------------------------


def _write_launchd_plist(label: str, port: int, log_path: Path) -> Path:
    """Write a macOS launchd plist and return its path."""
    plist_dir = Path.home() / "Library" / "LaunchAgents"
    plist_dir.mkdir(parents=True, exist_ok=True)
    plist_path = plist_dir / f"{label}.plist"

    python_path = sys.executable
    plist_content = textwrap.dedent(f"""\
        <?xml version="1.0" encoding="UTF-8"?>
        <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
          "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
        <plist version="1.0">
        <dict>
            <key>Label</key>
            <string>{label}</string>
            <key>ProgramArguments</key>
            <array>
                <string>{python_path}</string>
                <string>-m</string>
                <string>uvicorn</string>
                <string>githubclaw.server:app</string>
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
            <string>{log_path}</string>
            <key>StandardErrorPath</key>
            <string>{log_path}</string>
            <key>EnvironmentVariables</key>
            <dict>
                <key>PATH</key>
                <string>{os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin")}</string>
            </dict>
        </dict>
        </plist>
    """)
    plist_path.write_text(plist_content)
    return plist_path


def _write_systemd_unit(unit_name: str, port: int, log_path: Path) -> Path:
    """Write a Linux systemd user unit file and return its path."""
    unit_dir = Path.home() / ".config" / "systemd" / "user"
    unit_dir.mkdir(parents=True, exist_ok=True)
    unit_path = unit_dir / f"{unit_name}.service"

    python_path = sys.executable
    unit_content = textwrap.dedent(f"""\
        [Unit]
        Description=GithubClaw Webhook Server
        After=network.target

        [Service]
        Type=simple
        ExecStart={python_path} -m uvicorn githubclaw.server:app --host 0.0.0.0 --port {port}
        Restart=on-failure
        RestartSec=5
        StandardOutput=append:{log_path}
        StandardError=append:{log_path}
        Environment=PATH={os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin")}

        [Install]
        WantedBy=default.target
    """)
    unit_path.write_text(unit_content)
    return unit_path


def _read_pid() -> int | None:
    """Read the PID from the PID file, returning None if not present or stale."""
    pid_file = get_pid_file()
    if not pid_file.exists():
        return None
    try:
        pid = int(pid_file.read_text().strip())
        # Check if process is alive
        os.kill(pid, 0)
        return pid
    except (ValueError, ProcessLookupError, PermissionError):
        pid_file.unlink(missing_ok=True)
        return None


def _health_check(port: int, log_path: Path) -> None:
    """Check server health after startup, retrying up to 3 times."""
    import httpx

    time.sleep(2)
    for attempt in range(3):
        try:
            resp = httpx.get(f"http://127.0.0.1:{port}/health", timeout=5)
            if resp.status_code == 200:
                typer.echo("  Server is healthy.")
                return
        except (httpx.ConnectError, httpx.TimeoutException, httpx.HTTPError):
            pass
        if attempt < 2:
            time.sleep(1)

    typer.echo(
        f"  Warning: Server may not have started correctly. Check logs: {log_path}"
    )


LAUNCHD_LABEL = "com.githubclaw.webhook-server"
SYSTEMD_UNIT = "githubclaw-webhook-server"


@app.command()
def start() -> None:
    """Start the webhook server as a background daemon."""
    existing_pid = _read_pid()
    if existing_pid is not None:
        typer.echo(f"Webhook server already running (PID {existing_pid}).")
        raise typer.Exit(code=1)

    # Ensure global directories exist
    GLOBAL_CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    (GLOBAL_CONFIG_DIR / "logs").mkdir(parents=True, exist_ok=True)
    (GLOBAL_CONFIG_DIR / "secrets").mkdir(parents=True, exist_ok=True)

    config = GlobalConfig.load()
    if not (GLOBAL_CONFIG_DIR / "config.yaml").exists():
        config.save()

    log_path = get_log_file()
    log_path.parent.mkdir(parents=True, exist_ok=True)

    system = platform.system()

    if system == "Darwin":
        plist_path = _write_launchd_plist(LAUNCHD_LABEL, config.port, log_path)
        # Unload first in case a stale definition exists
        subprocess.run(
            ["launchctl", "bootout", f"gui/{os.getuid()}", str(plist_path)],
            capture_output=True,
        )
        result = subprocess.run(
            ["launchctl", "bootstrap", f"gui/{os.getuid()}", str(plist_path)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            typer.echo(f"Failed to start via launchd: {result.stderr.strip()}", err=True)
            raise typer.Exit(code=1)

        # launchd manages the process; find PID from launchctl
        info = subprocess.run(
            ["launchctl", "print", f"gui/{os.getuid()}/{LAUNCHD_LABEL}"],
            capture_output=True,
            text=True,
        )
        pid = None
        for line in info.stdout.splitlines():
            line = line.strip()
            if line.startswith("pid ="):
                with contextlib.suppress(ValueError):
                    pid = int(line.split("=")[1].strip())
        if pid:
            get_pid_file().write_text(str(pid))

        typer.echo(f"Webhook server started via launchd on port {config.port}.")
        typer.echo(f"  Logs: {log_path}")
        typer.echo(f"  Plist: {plist_path}")

    elif system == "Linux":
        unit_path = _write_systemd_unit(SYSTEMD_UNIT, config.port, log_path)
        subprocess.run(["systemctl", "--user", "daemon-reload"], capture_output=True)
        result = subprocess.run(
            ["systemctl", "--user", "start", SYSTEMD_UNIT],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            typer.echo(f"Failed to start via systemd: {result.stderr.strip()}", err=True)
            raise typer.Exit(code=1)

        # Get PID from systemd
        pid_result = subprocess.run(
            [
                "systemctl",
                "--user",
                "show",
                SYSTEMD_UNIT,
                "--property=MainPID",
                "--value",
            ],
            capture_output=True,
            text=True,
        )
        try:
            pid = int(pid_result.stdout.strip())
            if pid > 0:
                get_pid_file().write_text(str(pid))
        except ValueError:
            pass

        typer.echo(f"Webhook server started via systemd on port {config.port}.")
        typer.echo(f"  Logs: {log_path}")
        typer.echo(f"  Unit: {unit_path}")

    else:
        typer.echo(
            f"Unsupported platform: {system}. Only macOS and Linux are supported.",
            err=True,
        )
        raise typer.Exit(code=1)

    # Health check after launch
    _health_check(config.port, log_path)


@app.command()
def stop(
    force: bool = typer.Option(
        False, "--force", "-f", help="Immediate kill instead of graceful drain."
    ),
) -> None:
    """Stop the webhook server.

    Without --force: graceful drain (stop accepting webhooks, wait for running
    agents to finish up to the configured drain_timeout, then shut down).

    With --force: immediately kill all processes.
    """
    system = platform.system()

    if force:
        _stop_force(system)
    else:
        _stop_graceful(system)


@app.command()
def status() -> None:
    """Show the status of the webhook server and registered repos."""
    # Check if server is running
    pid = _read_pid()
    if pid is not None:
        typer.echo(f"Webhook server is running (PID {pid}).")
        # Try to show port from config
        try:
            config = GlobalConfig.load()
            typer.echo(f"  Port: {config.port}")
        except Exception:
            pass
    else:
        typer.echo("Webhook server is not running.")

    # Show registered repos
    registry_path = GLOBAL_CONFIG_DIR / "registry.json"
    if registry_path.exists():
        try:
            with open(registry_path) as f:
                registry = json.load(f)
            repos = registry.get("repos", {})
            if repos:
                typer.echo("")
                typer.echo(f"Registered repos ({len(repos)}):")
                for repo_name, info in repos.items():
                    local_path = info.get("local_path", "unknown")
                    typer.echo(f"  {repo_name}")
                    typer.echo(f"    Path: {local_path}")
                    # Show queue size if queue dir exists
                    queue_dir = Path(local_path) / ".githubclaw" / "queue"
                    if queue_dir.exists():
                        queue_files = [
                            f
                            for f in queue_dir.iterdir()
                            if f.is_file() and f.suffix == ".json"
                        ]
                        typer.echo(f"    Queue: {len(queue_files)} item(s)")
            else:
                typer.echo("")
                typer.echo("No repos registered.")
        except (json.JSONDecodeError, KeyError):
            typer.echo("")
            typer.echo("Registry file is corrupted.")
    else:
        typer.echo("")
        typer.echo("No repos registered (registry.json not found).")


@app.command()
def logs(
    follow: bool = typer.Option(
        False, "--follow", "-f", help="Follow log output (like tail -f)."
    ),
) -> None:
    """Show webhook server logs."""
    log_path = get_log_file()
    if not log_path.exists():
        typer.echo("No logs found.")
        raise typer.Exit()

    if follow:
        # Replace process with tail -f
        os.execvp("tail", ["tail", "-f", str(log_path)])
    else:
        # Print last 50 lines
        try:
            lines = log_path.read_text().splitlines()
            for line in lines[-50:]:
                typer.echo(line)
        except OSError as e:
            typer.echo(f"Error reading log file: {e}", err=True)
            raise typer.Exit(code=1) from e


def _stop_graceful(system: str) -> None:
    """Graceful drain shutdown: SIGTERM lets the server finish in-flight work."""
    pid = _read_pid()

    if system == "Darwin":
        plist_path = Path.home() / "Library" / "LaunchAgents" / f"{LAUNCHD_LABEL}.plist"
        if plist_path.exists():
            # Send SIGTERM to the process for graceful drain, then bootout
            if pid:
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGTERM)
            result = subprocess.run(
                ["launchctl", "bootout", f"gui/{os.getuid()}", str(plist_path)],
                capture_output=True,
                text=True,
            )
            get_pid_file().unlink(missing_ok=True)
            if result.returncode == 0 or "No such process" in result.stderr:
                typer.echo("Webhook server stopped (graceful drain).")
            else:
                typer.echo(f"launchctl bootout warning: {result.stderr.strip()}")
            return

    elif system == "Linux":
        result = subprocess.run(
            ["systemctl", "--user", "stop", SYSTEMD_UNIT],
            capture_output=True,
            text=True,
        )
        get_pid_file().unlink(missing_ok=True)
        if result.returncode == 0:
            typer.echo("Webhook server stopped (graceful drain).")
        else:
            typer.echo(f"systemctl stop warning: {result.stderr.strip()}")
        return

    # Fallback: direct PID-based stop
    if pid is None:
        typer.echo("Webhook server is not running.")
        raise typer.Exit(code=1)

    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGTERM)
    get_pid_file().unlink(missing_ok=True)
    typer.echo("Webhook server stopped (graceful drain).")


def _stop_force(system: str) -> None:
    """Immediate kill: SIGKILL all processes without waiting."""
    pid = _read_pid()

    if system == "Darwin":
        plist_path = Path.home() / "Library" / "LaunchAgents" / f"{LAUNCHD_LABEL}.plist"
        if pid:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
        if plist_path.exists():
            subprocess.run(
                ["launchctl", "bootout", f"gui/{os.getuid()}", str(plist_path)],
                capture_output=True,
            )
        get_pid_file().unlink(missing_ok=True)
        typer.echo("Webhook server killed (force).")
        return

    elif system == "Linux":
        result = subprocess.run(
            ["systemctl", "--user", "kill", "--signal=KILL", SYSTEMD_UNIT],
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["systemctl", "--user", "stop", SYSTEMD_UNIT],
            capture_output=True,
        )
        get_pid_file().unlink(missing_ok=True)
        if result.returncode == 0:
            typer.echo("Webhook server killed (force).")
        else:
            typer.echo(f"systemctl kill warning: {result.stderr.strip()}")
        return

    # Fallback: direct PID-based kill
    if pid is None:
        typer.echo("Webhook server is not running.")
        raise typer.Exit(code=1)

    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGKILL)
    get_pid_file().unlink(missing_ok=True)
    typer.echo("Webhook server killed (force).")


if __name__ == "__main__":
    app()
