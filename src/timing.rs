use std::fmt::Display;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};

/// A file rather than stderr, which belongs to the TUI.
static LOG: OnceLock<Mutex<File>> = OnceLock::new();

/// Opens the file named by `BREW_CHANGES_TIMING`, if set, to append to.
pub fn init() -> Result<()> {
    let Some(path) = std::env::var_os("BREW_CHANGES_TIMING") else {
        return Ok(());
    };
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening BREW_CHANGES_TIMING file {}", path.display()))?;
    LOG.get_or_init(|| Mutex::new(file));
    Ok(())
}

pub fn record(label: impl Display, start: Instant) {
    let Some(log) = LOG.get() else { return };
    let Ok(mut file) = log.lock() else { return };
    // The TUI owns the terminal, so a failed write has nowhere to be reported.
    writeln!(
        file,
        "{:>8.1}ms {label}",
        start.elapsed().as_secs_f64() * 1000.0
    )
    .ok();
}

pub async fn timed<T>(label: impl Display, future: impl Future<Output = T>) -> T {
    let start = Instant::now();
    let output = future.await;
    record(label, start);
    output
}
