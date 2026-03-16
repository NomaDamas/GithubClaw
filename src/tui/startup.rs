use std::io::{self, IsTerminal, Write};
use std::process::Command;

pub const GITHUBCLAW_REPO: &str = "NomaDamas/GithubClaw";

trait StartupBackend {
    fn current_version(&self) -> &str;
    fn fetch_latest_version(&mut self) -> Result<Option<String>, String>;
    fn prompt_yes_no(&mut self, message: &str) -> io::Result<bool>;
    fn install_update(&mut self) -> Result<(), String>;
    fn has_starred_repo(&mut self, repo: &str) -> Result<bool, String>;
    fn star_repo(&mut self, repo: &str) -> Result<(), String>;
    fn print_line(&mut self, message: &str);
}

pub fn run_tui_startup_checks() {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return;
    }

    let mut backend = SystemStartupBackend;
    if let Err(err) = run_tui_startup_checks_with(&mut backend) {
        eprintln!("Warning: failed to run TUI startup checks: {err}");
    }
}

fn run_tui_startup_checks_with<B: StartupBackend>(backend: &mut B) -> io::Result<()> {
    maybe_prompt_for_update(backend)?;
    maybe_prompt_for_star(backend)?;
    Ok(())
}

struct SystemStartupBackend;

impl StartupBackend for SystemStartupBackend {
    fn current_version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn fetch_latest_version(&mut self) -> Result<Option<String>, String> {
        let output = Command::new("cargo")
            .args(["search", "githubclaw", "--limit", "10"])
            .output()
            .map_err(|err| format!("failed to run cargo search: {err}"))?;

        if !output.status.success() {
            return Err(command_failure("cargo search", &output));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_crate_version(&stdout, "githubclaw"))
    }

    fn prompt_yes_no(&mut self, message: &str) -> io::Result<bool> {
        let mut stdout = io::stdout();
        loop {
            write!(stdout, "{message} ")?;
            stdout.flush()?;

            let mut input = String::new();
            let bytes = io::stdin().read_line(&mut input)?;
            if bytes == 0 {
                writeln!(stdout)?;
                return Ok(false);
            }

            match input.trim().to_ascii_lowercase().as_str() {
                "" | "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                _ => {
                    writeln!(stdout, "Please answer y or n.")?;
                }
            }
        }
    }

    fn install_update(&mut self) -> Result<(), String> {
        let output = Command::new("cargo")
            .args(["install", "githubclaw", "--locked", "--force"])
            .output()
            .map_err(|err| format!("failed to run cargo install: {err}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure("cargo install", &output))
        }
    }

    fn has_starred_repo(&mut self, repo: &str) -> Result<bool, String> {
        let output = Command::new("gh")
            .args([
                "repo",
                "view",
                repo,
                "--json",
                "viewerHasStarred",
                "-q",
                ".viewerHasStarred",
            ])
            .output()
            .map_err(|err| format!("failed to run gh repo view: {err}"))?;

        if !output.status.success() {
            return Err(command_failure("gh repo view", &output));
        }

        match String::from_utf8_lossy(&output.stdout).trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(format!("unexpected gh repo view output: {other}")),
        }
    }

    fn star_repo(&mut self, repo: &str) -> Result<(), String> {
        let output = Command::new("gh")
            .args(["repo", "star", repo, "--yes"])
            .output()
            .map_err(|err| format!("failed to run gh repo star: {err}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(command_failure("gh repo star", &output))
        }
    }

    fn print_line(&mut self, message: &str) {
        println!("{message}");
    }
}

fn maybe_prompt_for_update<B: StartupBackend>(backend: &mut B) -> io::Result<()> {
    let latest_version = match backend.fetch_latest_version() {
        Ok(version) => version,
        Err(err) => {
            backend.print_line(&format!("Skipping update check: {err}"));
            return Ok(());
        }
    };

    let Some(latest_version) = latest_version else {
        return Ok(());
    };

    if !is_newer_version(backend.current_version(), &latest_version) {
        return Ok(());
    }

    let prompt = format!(
        "An update is available for githubclaw ({} -> {}). Install now? [Y/n]",
        backend.current_version(),
        latest_version
    );
    if backend.prompt_yes_no(&prompt)? {
        match backend.install_update() {
            Ok(()) => backend.print_line("githubclaw update installed successfully."),
            Err(err) => backend.print_line(&format!("githubclaw update failed: {err}")),
        }
    }

    Ok(())
}

fn maybe_prompt_for_star<B: StartupBackend>(backend: &mut B) -> io::Result<()> {
    let has_starred = match backend.has_starred_repo(GITHUBCLAW_REPO) {
        Ok(value) => value,
        Err(err) => {
            backend.print_line(&format!("Skipping GitHub star check: {err}"));
            return Ok(());
        }
    };

    if has_starred {
        return Ok(());
    }

    let prompt = format!("Would you like to star {GITHUBCLAW_REPO}? [Y/n]");
    if backend.prompt_yes_no(&prompt)? {
        match backend.star_repo(GITHUBCLAW_REPO) {
            Ok(()) => backend.print_line("Thanks. GithubClaw has been starred."),
            Err(err) => backend.print_line(&format!("Failed to star GithubClaw: {err}")),
        }
    }

    Ok(())
}

fn is_newer_version(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

fn parse_version(input: &str) -> Option<(u64, u64, u64)> {
    let core = input.trim().trim_start_matches('v').split('-').next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

fn parse_crate_version(search_output: &str, crate_name: &str) -> Option<String> {
    let prefix = format!("{crate_name} = \"");
    search_output.lines().find_map(|line| {
        let trimmed = line.trim();
        let version = trimmed.strip_prefix(&prefix)?;
        let end = version.find('"')?;
        Some(version[..end].to_string())
    })
}

fn command_failure(command_name: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return format!("{command_name} failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return format!("{command_name} failed: {stdout}");
    }

    format!(
        "{command_name} failed with exit code {}",
        output.status.code().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBackend {
        current_version: String,
        latest_version: Option<String>,
        update_answer: bool,
        star_answer: bool,
        starred: bool,
        update_calls: usize,
        star_calls: usize,
        prompts: Vec<String>,
        lines: Vec<String>,
        latest_version_error: Option<String>,
        star_check_error: Option<String>,
        update_error: Option<String>,
        star_error: Option<String>,
    }

    impl MockBackend {
        fn new(current_version: &str) -> Self {
            Self {
                current_version: current_version.to_string(),
                latest_version: None,
                update_answer: true,
                star_answer: true,
                starred: false,
                update_calls: 0,
                star_calls: 0,
                prompts: Vec::new(),
                lines: Vec::new(),
                latest_version_error: None,
                star_check_error: None,
                update_error: None,
                star_error: None,
            }
        }
    }

    impl StartupBackend for MockBackend {
        fn current_version(&self) -> &str {
            &self.current_version
        }

        fn fetch_latest_version(&mut self) -> Result<Option<String>, String> {
            if let Some(err) = &self.latest_version_error {
                return Err(err.clone());
            }
            Ok(self.latest_version.clone())
        }

        fn prompt_yes_no(&mut self, message: &str) -> io::Result<bool> {
            self.prompts.push(message.to_string());
            if message.contains("update") {
                Ok(self.update_answer)
            } else {
                Ok(self.star_answer)
            }
        }

        fn install_update(&mut self) -> Result<(), String> {
            self.update_calls += 1;
            match &self.update_error {
                Some(err) => Err(err.clone()),
                None => Ok(()),
            }
        }

        fn has_starred_repo(&mut self, _repo: &str) -> Result<bool, String> {
            if let Some(err) = &self.star_check_error {
                return Err(err.clone());
            }
            Ok(self.starred)
        }

        fn star_repo(&mut self, _repo: &str) -> Result<(), String> {
            self.star_calls += 1;
            match &self.star_error {
                Some(err) => Err(err.clone()),
                None => Ok(()),
            }
        }

        fn print_line(&mut self, message: &str) {
            self.lines.push(message.to_string());
        }
    }

    #[test]
    fn prompts_for_update_then_star_when_tui_starts() {
        let mut backend = MockBackend::new("0.1.0");
        backend.latest_version = Some("0.2.0".to_string());
        backend.update_answer = true;
        backend.star_answer = true;

        run_tui_startup_checks_with(&mut backend).unwrap();

        assert_eq!(backend.update_calls, 1);
        assert_eq!(backend.star_calls, 1);
        assert_eq!(backend.prompts.len(), 2);
        assert!(backend.prompts[0].contains("update"));
        assert!(backend.prompts[1].contains("star"));
    }

    #[test]
    fn skips_update_prompt_when_current_version_is_latest() {
        let mut backend = MockBackend::new("0.2.0");
        backend.latest_version = Some("0.2.0".to_string());
        backend.star_answer = false;

        run_tui_startup_checks_with(&mut backend).unwrap();

        assert_eq!(backend.update_calls, 0);
        assert_eq!(backend.star_calls, 0);
        assert_eq!(backend.prompts.len(), 1);
        assert!(backend.prompts[0].contains("star"));
    }

    #[test]
    fn skips_star_prompt_when_repo_is_already_starred() {
        let mut backend = MockBackend::new("0.1.0");
        backend.latest_version = Some("0.1.1".to_string());
        backend.update_answer = false;
        backend.starred = true;

        run_tui_startup_checks_with(&mut backend).unwrap();

        assert_eq!(backend.update_calls, 0);
        assert_eq!(backend.star_calls, 0);
        assert_eq!(backend.prompts.len(), 1);
        assert!(backend.prompts[0].contains("update"));
    }

    #[test]
    fn parses_crate_version_from_cargo_search_output() {
        let output = "githubclaw = \"0.2.0\"    # Near-autonomous AI agents\nother = \"1.0.0\"";
        assert_eq!(
            parse_crate_version(output, "githubclaw"),
            Some("0.2.0".to_string())
        );
    }

    #[test]
    fn compares_versions_using_semver_core() {
        assert!(is_newer_version("0.1.0", "0.1.1"));
        assert!(is_newer_version("0.1.9", "0.2.0"));
        assert!(!is_newer_version("0.2.0", "0.2.0"));
        assert!(!is_newer_version("0.2.1", "0.2.0"));
    }
}
