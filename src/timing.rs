//! Timings of slow steps, appended to the file named by `BREW_CHANGES_TIMING`.
//! The macros compile to nothing without the `timing` feature, labels included.

#[cfg(feature = "timing")]
use std::sync::{Mutex, OnceLock};

use anyhow::Result;

/// A file rather than stderr, which belongs to the TUI.
#[cfg(feature = "timing")]
static LOG: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

#[cfg(feature = "timing")]
pub fn init() -> Result<()> {
    use anyhow::Context;
    let Some(path) = std::env::var_os("BREW_CHANGES_TIMING") else {
        return Ok(());
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening BREW_CHANGES_TIMING file {}", path.display()))?;
    LOG.get_or_init(|| Mutex::new(file));
    Ok(())
}

/// Fails rather than silently timing nothing.
#[cfg(not(feature = "timing"))]
pub fn init() -> Result<()> {
    if std::env::var_os("BREW_CHANGES_TIMING").is_some() {
        anyhow::bail!("BREW_CHANGES_TIMING needs a build with `--features timing`");
    }
    Ok(())
}

#[cfg(feature = "timing")]
pub fn record(start: std::time::Instant, label: std::fmt::Arguments<'_>) {
    use std::io::Write;
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

/// Starts a timer named `$start` for `record_timing!`.
#[macro_export]
macro_rules! timing_start {
    ($start:ident) => {
        #[cfg(feature = "timing")]
        let $start = std::time::Instant::now();
    };
}

/// Records the time since `timing_start!($start)`, labelled like `format!`.
#[macro_export]
macro_rules! record_timing {
    ($start:ident, $($label:tt)+) => {
        #[cfg(feature = "timing")]
        $crate::timing::record($start, format_args!($($label)+));
    };
}

/// Times an expression, which can end in `.await`.
#[macro_export]
macro_rules! timed {
    ($label:literal, $value:expr) => {{
        $crate::timing_start!(start);
        let value = $value;
        $crate::record_timing!(start, $label);
        value
    }};
}
