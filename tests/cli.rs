use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[test]
fn version_flag_prints_version_without_touching_brew() -> std::io::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_brew-changes"))
        .arg("--version")
        // An empty PATH makes any attempt to run `brew` fail the test.
        .env("PATH", "")
        .output()?;

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("brew-changes {}\n", env!("CARGO_PKG_VERSION"))
    );
    Ok(())
}

/// A `brew` that runs `script` for every call.
fn brew_shim(test: &str, script: &str) -> std::io::Result<PathBuf> {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{test}"));
    fs::create_dir_all(&dir)?;
    let shim = dir.join("brew");
    fs::write(&shim, format!("#!/bin/sh\n{script}\n"))?;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

/// Runs without a controlling terminal, as from cron or a pipeline.
fn run_detached(path: &Path) -> std::io::Result<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brew-changes"));
    command
        .env("PATH", format!("{}:/usr/bin:/bin", path.display()))
        .env("HOME", path);
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.output()
}

#[test]
fn brew_outdated_failure_shows_brews_reason() -> std::io::Result<()> {
    let dir = brew_shim("outdated-failure", "echo 'Error: broken tap' >&2; exit 1")?;
    let output = run_detached(&dir)?;
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error: broken tap"), "{stderr}");
    Ok(())
}

#[test]
fn no_terminal_is_an_error_not_a_panic() -> std::io::Result<()> {
    let dir = brew_shim(
        "no-terminal",
        r#"case "$1" in
  outdated) echo '{"formulae": [{"name": "x", "installed_versions": ["1"], "current_version": "2"}]}' ;;
  *) echo '{}' ;;
esac"#,
    )?;
    let output = run_detached(&dir)?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("error:") && !stderr.contains("panicked"),
        "{stderr}"
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    Ok(())
}
