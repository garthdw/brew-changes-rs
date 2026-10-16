// Dev tool, not shipped: runs the app's own resolution and fetch over real
// packages so regressions show up across many repos.
//
// Usage: cargo run --bin validate-changelogs -- [-cask] [-outdated] [N]

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use brew_changes::github::{Changelog, Client, Releases, resolve_source};
use brew_changes::homebrew::{self, Info, Package, brew_info_runner};
use futures::StreamExt;
use serde::Deserialize;

#[derive(Default)]
struct Outcome {
    name: String,
    repo: String,
    source: String,
    note: String,
    found: bool,
    skipped: bool,
    manual: bool,
    partial: bool,
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

    let (packages, mut results) = if outdated {
        (homebrew::outdated().await?, vec![])
    } else {
        top_packages(count, is_cask).await?
    };

    let infos = homebrew::resolve_info(brew_info_runner(), &packages).await;
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

    let (mut ok, mut fail, mut skip, mut manual, mut partial) = (0, 0, 0, 0, 0);
    for result in &results {
        let status = if result.skipped {
            skip += 1;
            "SKIP"
        } else if result.manual {
            manual += 1;
            "MANUAL"
        } else if result.partial {
            partial += 1;
            "PARTIAL"
        } else if !result.found {
            fail += 1;
            "FAIL"
        } else {
            ok += 1;
            "OK"
        };
        println!(
            "{status:<6} {:<20} {:<30} {:<18} {}",
            result.name, result.repo, result.source, result.note
        );
    }
    let matched = ok + manual;
    println!(
        "\n{matched}/{} matched ({ok}/{matched} auto-detected, {manual} via changelog_config.json), {partial} partial (best-effort), {fail} failed, {skip} skipped",
        matched + fail + partial
    );
    Ok(())
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
    let analytics: Analytics = http
        .get(&url)
        .send()
        .await?
        .json()
        .await
        .context("fetching analytics")?;

    let output = tokio::process::Command::new("brew")
        .arg("tap")
        .output()
        .await
        .context("brew tap")?;
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
            Some(tap) if !taps.contains(&tap) => skipped.push(Outcome {
                name,
                source: "skip-tap".into(),
                repo: "/".into(),
                skipped: true,
                note: format!("tap \"{tap}\" not installed"),
                ..Default::default()
            }),
            _ => packages.push(Package {
                name,
                installed: String::new(),
                current: String::new(),
                is_cask,
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
        repo: format!("{}/{}", source.owner, source.repo),
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
            outcome.found = found;
            outcome.manual = found;
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
            outcome.found = true;
            outcome.source = format!("file:{filename}");
            outcome.note = format!("{} bytes, {}", section.len(), ending(end));
        }
        Changelog::Releases {
            fallback_file,
            result,
        } => {
            outcome.source = "releases".into();
            outcome.note = match result {
                Err(error) => format!("fetch error: {error:#}"),
                Ok(Releases { list, found }) => match (list.first(), list.last()) {
                    (None, _) => "no releases found".into(),
                    (Some(newest), Some(oldest)) if found => {
                        outcome.found = true;
                        format!(
                            "{} releases, {}..{}",
                            list.len(),
                            oldest.tag_name,
                            newest.tag_name
                        )
                    }
                    (Some(newest), _) => {
                        outcome.partial = true;
                        format!(
                            "tag for {} not found; showing {} most recent releases (latest={})",
                            package.current,
                            list.len(),
                            newest.tag_name
                        )
                    }
                },
            };
            if let Some(filename) = fallback_file {
                outcome.note = format!("fell back from {filename}; {}", outcome.note);
            }
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
