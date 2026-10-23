use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use brew_changes::github::{Client, resolve_source};
use brew_changes::homebrew::{self, brew_info_runner};
use brew_changes::ui::app::{Item, Outcome, Upgrade};
use brew_changes::ui::keys::Keymap;
use brew_changes::ui::run::{detect_dark_background, run};
use brew_changes::ui::theme::ThemeConfig;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("brew-changes {}", brew_changes::VERSION);
        return ExitCode::SUCCESS;
    }
    match run_app().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_app() -> Result<()> {
    // Read before the slow brew calls so a typo fails fast.
    let (mut theme_config, theme_warnings) = ThemeConfig::load();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let theme = match argument.strip_prefix("--theme=") {
            Some(theme) => theme.to_string(),
            None if argument == "--theme" => {
                arguments.next().context("--theme needs a theme name")?
            }
            None => bail!("unknown argument {argument:?}, expected --theme <name> or --version"),
        };
        theme_config.select(&theme)?;
    }

    println!("Checking for outdated packages...");

    let packages = homebrew::outdated().await?;
    if packages.is_empty() {
        println!("Everything is up to date.");
        return Ok(());
    }

    let infos = homebrew::resolve_info(brew_info_runner(), &packages).await;
    let items = packages
        .into_iter()
        .zip(infos)
        .map(|(package, info)| Item {
            source: resolve_source(
                &package.name,
                &package.current,
                &[&info.homepage, &info.url, &info.head],
            ),
            name: package.name,
            is_cask: package.is_cask,
            installed: package.installed,
            current: package.current,
            homepage: info.homepage,
            ..Default::default()
        })
        .collect();

    let (keymap, warnings) = Keymap::load();
    for warning in warnings.into_iter().chain(theme_warnings) {
        eprintln!("{warning}");
    }
    let (themes, active) = theme_config.resolve(detect_dark_background);
    let (outcome, items) = run(items, keymap, themes, active, Client::new()?).await?;

    if outcome != Outcome::UpgradeAll {
        println!("No packages upgraded.");
        return Ok(());
    }
    let (casks, formulae): (Vec<Item>, Vec<Item>) = items
        .into_iter()
        .filter(|item| item.upgrade != Upgrade::Done)
        .partition(|item| item.is_cask);
    if formulae.is_empty() && casks.is_empty() {
        println!("Everything is already upgraded.");
        return Ok(());
    }
    println!("Upgrading {} package(s)...", formulae.len() + casks.len());
    let names = |items: Vec<Item>| items.into_iter().map(|item| item.name).collect::<Vec<_>>();
    homebrew::upgrade_all(&names(formulae), &names(casks)).await
}
