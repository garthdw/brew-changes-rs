pub mod changelog;
pub mod config;
pub mod github;
pub mod homebrew;
pub mod template;
pub mod ui;
pub mod version;

// GoReleaser sets this from the release tag; local builds fall back to Cargo.toml.
pub const VERSION: &str = match option_env!("BREW_CHANGES_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
