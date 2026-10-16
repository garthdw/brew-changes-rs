// Dev tool, not shipped: runs the app's own resolution and fetch over real
// packages so regressions show up across many repos.
//
// Usage: cargo run --bin validate-changelogs -- [-cask] [-outdated] [N]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use brew_changes::github::{Client, HtmlWithoutVersion, Origin, resolve_source};
use brew_changes::homebrew::{self, Info, Package, brew_info_runner};
use futures::future::join_all;
use serde::Deserialize;
use tokio::sync::Semaphore;

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
    let semaphore = Arc::new(Semaphore::new(8));
    let checks = packages
        .into_iter()
        .zip(infos)
        .filter(|(_, info)| *info != Info::default())
        .map(|(mut package, info)| {
            let client = client.clone();
            let semaphore = semaphore.clone();
            async move {
                let _permit = semaphore
                    .acquire()
                    .await
                    .context("validator semaphore closed")?;
                // Top-N packages aren't installed, so they're checked at brew's
                // stable version with nothing to trim back to.
                if package.current.is_empty() {
                    package.current = info.version.clone();
                }
                Ok::<_, anyhow::Error>(validate(&client, &package, &info).await)
            }
        });
    for check in join_all(checks).await {
        results.push(check?);
    }
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
    let source = resolve_source(&package.name, &package.current, &info.homepage, &info.url);
    let changelog = client
        .fetch(&source, &package.installed, &package.current, |_| {})
        .await;
    let mut outcome = Outcome {
        name: package.name.clone(),
        repo: format!("{}/{}", source.owner, source.repo),
        found: changelog.found,
        ..Default::default()
    };
    match changelog.origin {
        Origin::None => {
            outcome.source = "no-repo".into();
            outcome.note = format!("homepage={} url={}", info.homepage, info.url);
        }
        Origin::Manual => {
            outcome.source = "manual".into();
            outcome.manual = changelog.found;
            outcome.note = match &changelog.error {
                Some(error) if error.is::<HtmlWithoutVersion>() => {
                    format!("link only (HTML lacks version): {}", source.manual_url)
                }
                Some(error) => format!("fetch error: {error:#}"),
                None if !changelog.found => {
                    format!("version heading not located: {}", source.manual_url)
                }
                None => format!(
                    "{} bytes from {}",
                    changelog.section.len(),
                    source.manual_url
                ),
            };
        }
        Origin::File => {
            outcome.source = format!("file:{}", changelog.filename.as_deref().unwrap_or_default());
            outcome.note = format!("{} bytes", changelog.section.len());
        }
        Origin::Releases => {
            outcome.source = "releases".into();
            let releases = &changelog.releases;
            outcome.note = match (&changelog.error, releases.first(), releases.last()) {
                (Some(error), ..) => format!("fetch error: {error:#}"),
                (None, None, _) => "no releases found".into(),
                (None, Some(newest), Some(oldest)) if changelog.found => format!(
                    "{} releases, {}..{}",
                    releases.len(),
                    oldest.tag_name,
                    newest.tag_name
                ),
                (None, Some(newest), _) => {
                    outcome.partial = true;
                    format!(
                        "tag for {} not found; showing {} most recent releases (latest={})",
                        package.current,
                        releases.len(),
                        newest.tag_name
                    )
                }
            };
            if let Some(filename) = &changelog.filename {
                outcome.note = format!("fell back from {filename}; {}", outcome.note);
            }
        }
    }
    outcome
}
