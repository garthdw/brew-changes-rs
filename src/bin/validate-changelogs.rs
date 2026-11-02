// Dev tool, not shipped: runs the app's own resolution and fetch over real
// packages so regressions show up across many repos.
//
// Usage: cargo run --bin validate-changelogs -- [-cask] [-outdated] [N] [--filter NAME]...
//
// `--filter` (repeatable) checks only the named formulae/casks, or narrows
// `-outdated` to them.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use brew_changes::forges::{Changelog, Client, Releases, resolve_source};
use brew_changes::homebrew::{self, Info, Package, brew_info_runner};
use futures::StreamExt;
use serde::Deserialize;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    #[default]
    Fail,
    Skip,
    Manual,
    Partial,
    /// Every release in range lacks notes, so the app shows nothing useful.
    Empty,
}

#[derive(Default)]
struct Outcome {
    name: String,
    repo: String,
    source: String,
    note: String,
    status: Status,
}

impl Outcome {
    fn skipped(name: String, source: &str, note: String) -> Self {
        Self {
            name,
            repo: "/".into(),
            source: source.into(),
            note,
            status: Status::Skip,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let is_cask = arguments.iter().any(|argument| argument == "-cask");
    let outdated = arguments.iter().any(|argument| argument == "-outdated");
    let count = arguments
        .iter()
        .find_map(|argument| argument.parse().ok())
        .unwrap_or(50);
    let filters: Vec<String> = arguments
        .windows(2)
        .filter_map(|pair| match pair {
            [flag, name] if flag == "--filter" => Some(name.clone()),
            _ => None,
        })
        .collect();

    let (packages, mut results) = if outdated {
        let mut packages = homebrew::outdated().await?;
        if !filters.is_empty() {
            packages.retain(|package| filters.contains(&package.name));
        }
        (packages, vec![])
    } else if !filters.is_empty() {
        named_packages(filters).await
    } else {
        top_packages(count, is_cask).await?
    };

    let (infos, info_error) = homebrew::resolve_info(brew_info_runner(), &packages).await;
    if let Some(error) = info_error {
        eprintln!("warning: {error:#}");
    }
    brew_changes::timing::init()?;
    let client = Client::new()?;
    let checks = packages
        .into_iter()
        .zip(infos)
        .filter(|(_, info)| *info != Info::default())
        .map(|(mut package, info)| {
            let client = client.clone();
            async move {
                // Top-N packages aren't installed, so they're checked at brew's
                // stable version with nothing to trim back to.
                if package.current.is_empty() {
                    package.current = info.version.clone();
                }
                // `brew outdated` only lists these with --greedy, so the app
                // never shows them.
                if package.current == "latest" {
                    return Outcome::skipped(
                        package.name,
                        "skip-latest",
                        "unversioned cask".into(),
                    );
                }
                validate(&client, &package, &info).await
            }
        });
    results.extend(
        futures::stream::iter(checks)
            .buffer_unordered(8)
            .collect::<Vec<_>>()
            .await,
    );
    results.sort_by(|left, right| left.name.cmp(&right.name));

    for result in &results {
        println!(
            "{:<6} {:<20} {:<30} {:<18} {}",
            format!("{:?}", result.status).to_uppercase(),
            result.name,
            result.repo,
            result.source,
            result.note
        );
    }
    let count = |status| {
        results
            .iter()
            .filter(|result| result.status == status)
            .count()
    };
    let [ok, fail, skip, manual, partial, empty] = [
        Status::Ok,
        Status::Fail,
        Status::Skip,
        Status::Manual,
        Status::Partial,
        Status::Empty,
    ]
    .map(count);
    let matched = ok + manual;
    println!(
        "\n{matched}/{} matched ({ok}/{matched} auto-detected, {manual} via changelog_config.json), {partial} partial (best-effort), {empty} empty release notes, {fail} failed, {skip} skipped",
        matched + fail + partial + empty
    );
    Ok(())
}

/// Names brew knows as neither a formula nor a cask are reported as skipped.
async fn named_packages(names: Vec<String>) -> (Vec<Package>, Vec<Outcome>) {
    // Names that aren't formulae are expected to miss here; the cask pass
    // reports anything still unresolved.
    let (formulae, _) = homebrew::resolve_info_batch(brew_info_runner(), &names, false).await;
    let (formulae, others): (Vec<String>, Vec<String>) = names
        .into_iter()
        .partition(|name| formulae.contains_key(name));
    let (casks, cask_error) = homebrew::resolve_info_batch(brew_info_runner(), &others, true).await;
    if let Some(error) = cask_error {
        eprintln!("warning: {error:#}");
    }
    let (casks, unknown): (Vec<String>, Vec<String>) = others
        .into_iter()
        .partition(|name| casks.contains_key(name));
    let package = |name: String, is_cask| Package {
        name,
        is_cask,
        ..Default::default()
    };
    let packages = formulae
        .into_iter()
        .map(|name| package(name, false))
        .chain(casks.into_iter().map(|name| package(name, true)))
        .collect();
    let skipped = unknown
        .into_iter()
        .map(|name| {
            Outcome::skipped(
                name,
                "skip-unknown",
                "not a formula or cask brew knows".into(),
            )
        })
        .collect();
    (packages, skipped)
}

async fn top_packages(count: usize, is_cask: bool) -> Result<(Vec<Package>, Vec<Outcome>)> {
    #[derive(Deserialize)]
    struct Analytics {
        items: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        formula: Option<String>,
        cask: Option<String>,
    }
    let kind = if is_cask {
        "cask-install"
    } else {
        "install-on-request"
    };
    let url = format!("https://formulae.brew.sh/api/analytics/{kind}/365d.json");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    let (analytics, output) = tokio::try_join!(
        async {
            let analytics: Analytics = http
                .get(&url)
                .send()
                .await?
                .json()
                .await
                .context("fetching analytics")?;
            anyhow::Ok(analytics)
        },
        async {
            tokio::process::Command::new("brew")
                .arg("tap")
                .output()
                .await
                .context("brew tap")
        },
    )?;
    if !output.status.success() {
        bail!("brew tap: {}", output.status);
    }
    let taps: HashSet<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect();

    let mut packages = vec![];
    let mut skipped = vec![];
    for name in analytics
        .items
        .into_iter()
        .take(count)
        .filter_map(|entry| entry.formula.or(entry.cask))
    {
        // One unresolvable name makes a whole `brew info` call fail, so
        // packages from untapped third-party taps are screened out up front.
        let tap = match name.split('/').collect::<Vec<_>>().as_slice() {
            [owner, tap, _, ..] => Some(format!("{owner}/{tap}")),
            _ => None,
        };
        match tap {
            Some(tap) if !taps.contains(&tap) => skipped.push(Outcome::skipped(
                name,
                "skip-tap",
                format!("tap \"{tap}\" not installed"),
            )),
            _ => packages.push(Package {
                name,
                is_cask,
                ..Default::default()
            }),
        }
    }
    Ok((packages, skipped))
}

/// An HTML page lacking the version counts as a failure, since the app can
/// only show it as a link.
async fn validate(client: &Client, package: &Package, info: &Info) -> Outcome {
    let source = resolve_source(
        &package.name,
        &package.current,
        &[&info.homepage, &info.url, &info.head],
    );
    let changelog = client
        .fetch(&source, &package.installed, &package.current, |_| {})
        .await;
    let mut outcome = Outcome {
        name: package.name.clone(),
        repo: source
            .forge
            .as_ref()
            .map_or_else(|| "/".into(), |forge| forge.path()),
        ..Default::default()
    };
    let manual = &source.manual_url;
    match changelog {
        Changelog::NoSource => {
            outcome.source = "no-repo".into();
            outcome.note = format!(
                "homepage={} url={} head={}",
                info.homepage, info.url, info.head
            );
        }
        Changelog::ManualHtmlWithoutVersion => {
            outcome.source = "manual".into();
            outcome.note = format!("link only (HTML lacks version): {manual}");
        }
        Changelog::ManualFailed(error) => {
            outcome.source = "manual".into();
            outcome.note = format!("fetch error: {error:#}");
        }
        Changelog::Manual {
            section,
            found,
            end,
        } => {
            outcome.source = "manual".into();
            if found {
                outcome.status = Status::Manual;
            }
            outcome.note = if found {
                format!("{} bytes from {manual}, {}", section.len(), ending(end))
            } else {
                format!("version heading not located: {manual}")
            };
        }
        Changelog::File {
            filename,
            section,
            end,
        } => {
            outcome.status = Status::Ok;
            outcome.source = format!("file:{filename}");
            outcome.note = format!("{} bytes, {}", section.len(), ending(end));
        }
        Changelog::Releases {
            fallback_file,
            file_error,
            result,
        } => {
            outcome.source = "releases".into();
            outcome.note = match &result {
                Err(error) => format!("fetch error: {error:#}"),
                Ok(Releases { list, found }) => match (list.first(), list.last()) {
                    (None, _) => "no releases found".into(),
                    (Some(newest), Some(oldest)) if *found => {
                        outcome.status = Status::Ok;
                        format!(
                            "{} releases, {}..{}",
                            list.len(),
                            oldest.tag_name,
                            newest.tag_name
                        )
                    }
                    (Some(newest), _) => {
                        outcome.status = Status::Partial;
                        format!(
                            "tag for {} not found; showing {} most recent releases (latest={})",
                            package.current,
                            list.len(),
                            newest.tag_name
                        )
                    }
                },
            };
            if let Ok(Releases { list, .. }) = &result
                && !list.is_empty()
            {
                let without_notes = list.iter().filter(|release| release.lacks_notes()).count();
                if without_notes == list.len() {
                    outcome.status = Status::Empty;
                    outcome.note = format!("no release has notes; {}", outcome.note);
                } else if without_notes > 0 {
                    outcome.note = format!(
                        "{without_notes}/{} releases lack notes; {}",
                        list.len(),
                        outcome.note
                    );
                }
            }
            if let Some(error) = file_error {
                outcome.note = format!("file fetch error: {error:#}; {}", outcome.note);
            }
            if let Some(filename) = fallback_file {
                outcome.note = format!("fell back from {filename}; {}", outcome.note);
            }
        }
        Changelog::Commits(result) => {
            outcome.source = "commits".into();
            outcome.note = match result {
                Err(error) => format!("fetch error: {error:#}"),
                Ok(commits) => {
                    if !commits.list.is_empty() {
                        outcome.status = Status::Ok;
                    }
                    let cut = if commits.truncated { " (cut)" } else { "" };
                    format!("{} commits{cut}", commits.list.len())
                }
            };
        }
    }
    outcome
}

/// Shows where a section stopped, so a heading template that only matches the
/// newest release (and would show 100 truncated lines) is caught.
fn ending(end: Option<String>) -> String {
    match end {
        Some(heading) => format!("ends at {:?}", heading.trim()),
        None => "truncated".into(),
    }
}
