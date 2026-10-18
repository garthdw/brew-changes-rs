use std::process::Command;

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
