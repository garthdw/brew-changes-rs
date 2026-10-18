//! Drives the real binary in a detached tmux session, with a `brew` shim on
//! `PATH` and a mock GitHub, so the terminal loop and process wiring are
//! exercised end to end.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_brew-changes");
const TIMEOUT: Duration = Duration::from_secs(10);

const CHANGELOG: &str = "# Changelog

## 2.0.0
- Added the e2e feature

## 1.5.0
- Fixed the middle bug

## 1.0.0
- Initial release
";

/// Writes a `brew` shim that answers from fixture files and logs every call
/// to `calls.log`.
fn brew_shim(test: &str, outdated: Value) -> Result<PathBuf> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{test}"));
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(dir.join("bin"))?;
    fs::create_dir_all(dir.join("config"))?;
    fs::write(dir.join("outdated.json"), outdated.to_string())?;
    fs::write(
        dir.join("info-formula.json"),
        json!({"formulae": [{"full_name": "e2e-tool", "homepage": "https://github.com/o/r"}]})
            .to_string(),
    )?;
    fs::write(
        dir.join("info-cask.json"),
        json!({"casks": [{"full_token": "e2e-app", "homepage": "https://example.com"}]})
            .to_string(),
    )?;
    let shim = dir.join("bin/brew");
    fs::write(
        &shim,
        format!(
            r#"#!/bin/sh
dir='{}'
echo "$*" >> "$dir/calls.log"
case "$1 $3" in
  outdated*) cat "$dir/outdated.json" ;;
  "info --formula") cat "$dir/info-formula.json" ;;
  "info --cask") cat "$dir/info-cask.json" ;;
  upgrade*)
    if [ -f "$dir/upgrade-stderr" ]; then
      cat "$dir/upgrade-stderr" >&2
      exit 1
    fi ;;
  *) exit 1 ;;
esac
"#,
            dir.display()
        ),
    )?;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn two_outdated() -> Value {
    json!({
        "formulae": [{"name": "e2e-tool", "installed_versions": ["1.0.0"], "current_version": "2.0.0"}],
        "casks": [{"name": "e2e-app", "installed_versions": ["2.0,1"], "current_version": "3.0,2"}]
    })
}

async fn mock_github() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/repos/o/r/contents/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"name": "CHANGELOG.md", "type": "file"}])),
        )
        .mount(&server)
        .await;
    Mock::given(path("/repos/o/r/contents/CHANGELOG.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"content": base64::engine::general_purpose::STANDARD.encode(CHANGELOG)}),
        ))
        .mount(&server)
        .await;
    server
}

/// Forwarded through the cleared environment so `cargo llvm-cov` sees the
/// binary's coverage.
fn profile_file() -> Option<String> {
    std::env::var("LLVM_PROFILE_FILE").ok()
}

fn calls(dir: &Path) -> Result<Vec<String>> {
    Ok(fs::read_to_string(dir.join("calls.log"))?
        .lines()
        .map(String::from)
        .collect())
}

struct Session {
    socket: String,
}

impl Session {
    fn start(test: &str, dir: &Path, github: &MockServer) -> Result<Self> {
        let session = Self {
            socket: format!("brew-changes-e2e-{test}-{}", std::process::id()),
        };
        let mut environment = vec![
            format!("HOME={}", dir.display()),
            "TERM=xterm-256color".into(),
            format!("PATH={}:/usr/bin:/bin", dir.join("bin").display()),
            format!("XDG_CONFIG_HOME={}", dir.join("config").display()),
            // Skips the `gh auth token` lookup.
            "GITHUB_TOKEN=test".into(),
            format!("BREW_CHANGES_GITHUB_API={}", github.uri()),
        ];
        environment.extend(profile_file().map(|file| format!("LLVM_PROFILE_FILE={file}")));
        let mut arguments = vec![
            "start-server",
            ";",
            // Keeps the pane after exit so output printed after the TUI can
            // be captured.
            "set-option",
            "-g",
            "remain-on-exit",
            "on",
            ";",
            "new-session",
            "-d",
            "-s",
            "e2e",
            "-x",
            "100",
            "-y",
            "30",
            "env",
            "-i",
        ];
        arguments.extend(environment.iter().map(String::as_str));
        arguments.push(BIN);
        session.tmux(&arguments)?;
        Ok(session)
    }

    fn tmux(&self, arguments: &[&str]) -> Result<String> {
        let output = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(arguments)
            .output()
            .context("tmux is required for e2e tests")?;
        if !output.status.success() {
            bail!(
                "tmux {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn send(&self, key: &str) -> Result<()> {
        self.tmux(&["send-keys", "-t", "e2e", key])?;
        Ok(())
    }

    fn screen(&self) -> Result<String> {
        self.tmux(&["capture-pane", "-p", "-t", "e2e"])
    }

    async fn wait_until(
        &self,
        description: &str,
        mut done: impl FnMut(&Self) -> Result<bool>,
    ) -> Result<String> {
        let start = Instant::now();
        while !done(self)? {
            if start.elapsed() > TIMEOUT {
                bail!("timed out waiting for {description}:\n{}", self.screen()?);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.screen()
    }

    async fn wait_for(&self, text: &str) -> Result<String> {
        self.wait_until(&format!("{text:?}"), |session| {
            Ok(session.screen()?.contains(text))
        })
        .await
    }

    async fn wait_exit(&self) -> Result<String> {
        self.wait_until("exit", |session| {
            Ok(session
                .tmux(&["display-message", "-p", "-t", "e2e", "#{pane_dead}"])?
                .trim()
                == "1")
        })
        .await
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.tmux(&["kill-server"]) {
            eprintln!("{error:#}");
        }
    }
}

#[test]
fn nothing_outdated() -> Result<()> {
    let dir = brew_shim("nothing_outdated", json!({"formulae": [], "casks": []}))?;
    let output = Command::new(BIN)
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", dir.join("bin").display()),
        )
        .envs(profile_file().map(|file| ("LLVM_PROFILE_FILE", file)))
        .output()?;
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("Everything is up to date."));
    Ok(())
}

#[tokio::test]
async fn lists_and_expands_changelog() -> Result<()> {
    let dir = brew_shim("lists_and_expands_changelog", two_outdated())?;
    let github = mock_github().await;
    let session = Session::start("lists_and_expands_changelog", &dir, &github)?;

    let screen = session.wait_for("e2e-app").await?;
    assert!(screen.contains("e2e-tool"), "{screen}");

    session.send("Enter")?;
    let screen = session.wait_for("Added the e2e feature").await?;
    assert!(screen.contains("Fixed the middle bug"), "{screen}");
    assert!(!screen.contains("Initial release"), "{screen}");

    session.send("q")?;
    let screen = session.wait_exit().await?;
    assert!(screen.contains("No packages upgraded."), "{screen}");
    assert!(
        !calls(&dir)?.iter().any(|call| call.starts_with("upgrade")),
        "{:?}",
        calls(&dir)?
    );
    Ok(())
}

#[tokio::test]
async fn upgrade_current() -> Result<()> {
    let dir = brew_shim("upgrade_current", two_outdated())?;
    let github = mock_github().await;
    let session = Session::start("upgrade_current", &dir, &github)?;

    session.wait_for("e2e-app").await?;
    session.send("u")?;
    session.wait_for("[upgraded]").await?;
    let upgrades: Vec<_> = calls(&dir)?
        .into_iter()
        .filter(|call| call.starts_with("upgrade"))
        .collect();
    assert_eq!(upgrades, ["upgrade e2e-tool"]);
    Ok(())
}

#[tokio::test]
async fn upgrade_all() -> Result<()> {
    let dir = brew_shim("upgrade_all", two_outdated())?;
    let github = mock_github().await;
    let session = Session::start("upgrade_all", &dir, &github)?;

    session.wait_for("e2e-app").await?;
    session.send("a")?;
    let screen = session.wait_exit().await?;
    assert!(screen.contains("Upgrading 2 package(s)..."), "{screen}");
    let upgrades: Vec<_> = calls(&dir)?
        .into_iter()
        .filter(|call| call.starts_with("upgrade"))
        .collect();
    assert_eq!(upgrades, ["upgrade e2e-tool", "upgrade --cask e2e-app"]);
    Ok(())
}

#[tokio::test]
async fn upgrade_current_needing_password_shows_hint() -> Result<()> {
    let dir = brew_shim(
        "upgrade_current_needing_password_shows_hint",
        two_outdated(),
    )?;
    fs::write(
        dir.join("upgrade-stderr"),
        "sudo: a terminal is required to read the password\n",
    )?;
    let github = mock_github().await;
    let session = Session::start("upgrade_current_needing_password_shows_hint", &dir, &github)?;

    session.wait_for("e2e-app").await?;
    session.send("u")?;
    session.wait_for("[upgrade failed]").await?;
    session.send("Enter")?;
    let screen = session.wait_for("This upgrade needs a password").await?;
    assert!(screen.contains("a terminal is required"), "{screen}");
    Ok(())
}
