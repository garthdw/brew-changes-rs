use std::collections::HashMap;
use std::process::Output;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use futures::future::{BoxFuture, join_all};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::Semaphore;

// A single unresolvable name fails a whole `brew info` call, so batches bound
// the blast radius before bisection.
const INFO_BATCH_SIZE: usize = 50;
const INFO_CONCURRENCY: usize = 8;

/// Versions are in upstream form (see `upstream_version`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub installed: String,
    pub current: String,
    pub is_cask: bool,
}

#[derive(Deserialize)]
struct OutdatedEntry {
    name: String,
    #[serde(default)]
    installed_versions: Vec<String>,
    current_version: String,
}

#[derive(Deserialize)]
struct OutdatedResult {
    #[serde(default)]
    formulae: Vec<OutdatedEntry>,
    #[serde(default)]
    casks: Vec<OutdatedEntry>,
}

pub async fn outdated() -> Result<Vec<Package>> {
    let output = Command::new("brew")
        .args(["outdated", "--json=v2"])
        .output()
        .await
        .context("brew outdated")?;
    if !output.status.success() {
        bail!("brew outdated: {}", output.status);
    }
    parse_outdated(&output.stdout)
}

fn parse_outdated(json: &[u8]) -> Result<Vec<Package>> {
    let result: OutdatedResult =
        serde_json::from_slice(json).context("parsing brew outdated output")?;
    let to_packages = |entries: Vec<OutdatedEntry>, is_cask| {
        entries.into_iter().map(move |entry| Package {
            installed: upstream_version(
                entry.installed_versions.first().map_or("", String::as_str),
                is_cask,
            ),
            current: upstream_version(&entry.current_version, is_cask),
            name: entry.name,
            is_cask,
        })
    };
    Ok(to_packages(result.formulae, false)
        .chain(to_packages(result.casks, true))
        .collect())
}

/// Upstream tags never include a formula's "_N" revision ("2.51.0_1") or a
/// cask's ",build" (docker-desktop's "4.95.0,242831").
pub fn upstream_version(version: &str, is_cask: bool) -> String {
    if is_cask {
        return version.split(',').next().unwrap_or_default().to_string();
    }
    match version.rsplit_once('_') {
        Some((base, revision))
            if !revision.is_empty() && revision.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => version.to_string(),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Info {
    pub homepage: String,
    pub url: String,
    pub head: String,
    pub version: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct UrlField {
    url: String,
}

#[derive(Deserialize)]
struct FormulaInfo {
    full_name: String,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    versions: FormulaVersions,
    #[serde(default)]
    urls: FormulaUrls,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FormulaVersions {
    stable: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FormulaUrls {
    stable: Option<UrlField>,
    head: Option<UrlField>,
}

#[derive(Deserialize)]
struct CaskInfo {
    full_token: String,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct InfoResult {
    #[serde(default)]
    formulae: Vec<FormulaInfo>,
    #[serde(default)]
    casks: Vec<CaskInfo>,
}

fn parse_info(json: &[u8]) -> Result<HashMap<String, Info>> {
    let result: InfoResult = serde_json::from_slice(json)?;
    let formulae = result.formulae.into_iter().map(|formula| {
        let info = Info {
            homepage: formula.homepage.unwrap_or_default(),
            url: formula.urls.stable.unwrap_or_default().url,
            head: formula.urls.head.unwrap_or_default().url,
            version: formula.versions.stable.unwrap_or_default(),
        };
        (formula.full_name, info)
    });
    let casks = result.casks.into_iter().map(|cask| {
        let info = Info {
            homepage: cask.homepage.unwrap_or_default(),
            url: cask.url.unwrap_or_default(),
            version: upstream_version(&cask.version.unwrap_or_default(), true),
            ..Default::default()
        };
        (cask.full_token, info)
    });
    Ok(formulae.chain(casks).collect())
}

/// Only `Exit` (brew ran but some name didn't resolve) is worth bisecting.
pub enum RunError {
    Exit,
    Other(anyhow::Error),
}

/// Injectable so bisection can be tested without brew.
pub type InfoRunner =
    dyn Fn(Vec<String>, bool) -> BoxFuture<'static, Result<Vec<u8>, RunError>> + Send + Sync;

pub fn brew_info_runner() -> Arc<InfoRunner> {
    Arc::new(|names, is_cask| {
        Box::pin(async move {
            let flag = if is_cask { "--cask" } else { "--formula" };
            let output = Command::new("brew")
                .args(["info", "--json=v2", flag])
                .args(&names)
                .output()
                .await
                .map_err(|error| RunError::Other(error.into()))?;
            if output.status.success() {
                Ok(output.stdout)
            } else {
                Err(RunError::Exit)
            }
        })
    })
}

/// Errors only if nothing resolved and a batch failed.
pub async fn resolve_info_batch(
    run: Arc<InfoRunner>,
    names: &[String],
    is_cask: bool,
) -> Result<HashMap<String, Info>> {
    // Shared with bisection, which would otherwise multiply concurrent brews.
    let permits = Arc::new(Semaphore::new(INFO_CONCURRENCY));
    let results = join_all(
        names
            .chunks(INFO_BATCH_SIZE)
            .map(|chunk| resolve_chunk(run.clone(), permits.clone(), chunk.to_vec(), is_cask)),
    )
    .await;
    let mut infos = HashMap::new();
    let mut first_error = None;
    for result in results {
        match result {
            Ok(resolved) => infos.extend(resolved),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) if infos.is_empty() => Err(error),
        _ => Ok(infos),
    }
}

/// brew exits nonzero with no output if any single name fails to resolve (a
/// renamed formula), so a failing chunk is bisected to drop just the bad
/// names.
fn resolve_chunk(
    run: Arc<InfoRunner>,
    permits: Arc<Semaphore>,
    chunk: Vec<String>,
    is_cask: bool,
) -> BoxFuture<'static, Result<HashMap<String, Info>>> {
    Box::pin(async move {
        let result = {
            let _permit = permits.acquire().await?;
            run(chunk.clone(), is_cask).await
        };
        match result {
            Ok(output) => parse_info(&output)
                .with_context(|| format!("parsing brew info output for {chunk:?}")),
            Err(_) if chunk.len() == 1 => Ok(HashMap::new()),
            Err(RunError::Other(error)) => Err(error),
            Err(RunError::Exit) => {
                let (left, right) = chunk.split_at(chunk.len() / 2);
                let (left, right) = tokio::join!(
                    resolve_chunk(run.clone(), permits.clone(), left.to_vec(), is_cask),
                    resolve_chunk(run, permits, right.to_vec(), is_cask)
                );
                // A half that fails outright just loses its names; the rest
                // of the list still gets metadata.
                Ok(left
                    .unwrap_or_default()
                    .into_iter()
                    .chain(right.unwrap_or_default())
                    .collect())
            }
        }
    })
}

/// Aligned with `packages`. Unresolved packages get empty metadata rather
/// than failing the whole list.
pub async fn resolve_info(run: Arc<InfoRunner>, packages: &[Package]) -> Vec<Info> {
    let names = |cask: bool| {
        packages
            .iter()
            .filter(|package| package.is_cask == cask)
            .map(|package| package.name.clone())
            .collect::<Vec<_>>()
    };
    let (formulae, casks) = (names(false), names(true));
    let (formula_infos, cask_infos) = tokio::join!(
        resolve_info_batch(run.clone(), &formulae, false),
        resolve_info_batch(run, &casks, true)
    );
    let (formula_infos, cask_infos) = (
        formula_infos.unwrap_or_default(),
        cask_infos.unwrap_or_default(),
    );
    packages
        .iter()
        .map(|package| {
            if package.is_cask {
                &cask_infos
            } else {
                &formula_infos
            }
            .get(&package.name)
            .cloned()
            .unwrap_or_default()
        })
        .collect()
}

fn upgrade_args<'a>(names: &[&'a str], is_cask: bool) -> Vec<&'a str> {
    ["upgrade"]
        .into_iter()
        .chain(is_cask.then_some("--cask"))
        .chain(names.iter().copied())
        .collect()
}

/// Output is captured because the UI owns the terminal; the error carries it
/// so the UI can show it.
pub async fn upgrade_quiet(name: &str, is_cask: bool) -> Result<()> {
    let args = upgrade_args(&[name], is_cask);
    let command = format!("brew {}", args.join(" "));
    let mut brew = Command::new("brew");
    brew.args(&args);
    // Without a controlling terminal, sudo fails fast instead of prompting
    // over the UI and competing for keystrokes.
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        brew.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = brew.output().await.with_context(|| command.clone())?;
    if output.status.success() {
        return Ok(());
    }
    Err(upgrade_failure(&command, &output))
}

fn upgrade_failure(command: &str, output: &Output) -> anyhow::Error {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let hint = if combined.contains("a terminal is required") {
        "\n\nThis upgrade needs a password; use upgrade all, which runs in the terminal."
    } else {
        ""
    };
    anyhow!("{command}: {}\n{}{hint}", output.status, combined.trim())
}

/// Casks are attempted even if the formula upgrade fails, and every failure
/// is reported.
pub async fn upgrade_all(formulae: &[String], casks: &[String]) -> Result<()> {
    let mut failures = vec![];
    for (names, is_cask) in [(formulae, false), (casks, true)] {
        if names.is_empty() {
            continue;
        }
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let args = upgrade_args(&names, is_cask);
        let command = format!("brew {}", args.join(" "));
        match Command::new("brew").args(&args).status().await {
            Ok(status) if status.success() => {}
            Ok(status) => failures.push(format!("{command}: {status}")),
            Err(error) => failures.push(format!("{command}: {error}")),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(failures.join("\nerror: ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn outdated_parsing() {
        let raw = br#"{
            "formulae": [
                {"name": "git", "installed_versions": ["2.40.0_1", "2.39.0"], "current_version": "2.41.0_2"},
                {"name": "new", "installed_versions": [], "current_version": "1.0"}
            ],
            "casks": [{"name": "docker-desktop", "installed_versions": ["4.94.0,1"], "current_version": "4.95.0,242831"}]
        }"#;
        let package = |name: &str, installed: &str, current: &str, is_cask| Package {
            name: name.into(),
            installed: installed.into(),
            current: current.into(),
            is_cask,
        };
        assert_eq!(
            parse_outdated(raw).unwrap(),
            [
                package("git", "2.40.0", "2.41.0", false),
                package("new", "", "1.0", false),
                package("docker-desktop", "4.94.0", "4.95.0", true),
            ]
        );
        assert!(parse_outdated(b"nope").is_err());
    }

    #[test]
    fn info_parsing_keeps_stable_and_head_urls() {
        let raw = br#"{
            "formulae": [{
                "name": "opencode", "full_name": "anomalyco/tap/opencode", "homepage": "https://example.com",
                "versions": {"stable": "1.2.3"},
                "urls": {"stable": {"url": "https://example.com/stable.tar.gz"}, "head": {"url": "https://example.com/head.tar.gz"}}
            }, {
                "full_name": "headonly", "homepage": null,
                "urls": {"head": {"url": "https://example.com/head.git"}}
            }],
            "casks": [{"token": "app", "full_token": "someuser/tap/app", "homepage": "https://example.com", "url": "https://example.com/app.dmg", "version": "2.0,99"}]
        }"#;
        let infos = parse_info(raw).unwrap();
        assert_eq!(
            infos["anomalyco/tap/opencode"],
            Info {
                homepage: "https://example.com".into(),
                url: "https://example.com/stable.tar.gz".into(),
                head: "https://example.com/head.tar.gz".into(),
                version: "1.2.3".into()
            }
        );
        assert_eq!(infos["headonly"].head, "https://example.com/head.git");
        assert_eq!(
            infos["someuser/tap/app"],
            Info {
                homepage: "https://example.com".into(),
                url: "https://example.com/app.dmg".into(),
                version: "2.0".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn upstream_version_cases() {
        for (v, is_cask, want) in [
            ("4.95.0,242831", true, "4.95.0"),
            ("latest", true, "latest"),
            ("2.51.0_1", false, "2.51.0"),
            ("1.2.3", false, "1.2.3"),
        ] {
            assert_eq!(upstream_version(v, is_cask), want);
        }
    }

    /// A fake `brew info` that fails any call including a name in `bad`, and
    /// records every call's names.
    fn fake_runner(
        bad: &'static [&'static str],
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    ) -> Arc<InfoRunner> {
        Arc::new(move |names: Vec<String>, _| {
            calls.lock().unwrap().push(names.clone());
            Box::pin(async move {
                if names.iter().any(|name| bad.contains(&name.as_str())) {
                    return Err(RunError::Exit);
                }
                let formulae: Vec<_> = names
                    .iter()
                    .map(
                        |name| serde_json::json!({"full_name": name, "homepage": format!("https://{name}")}),
                    )
                    .collect();
                Ok(serde_json::to_vec(&serde_json::json!({ "formulae": formulae })).unwrap())
            })
        })
    }

    #[tokio::test]
    async fn bisects_failing_batch_and_drops_bad_names() {
        let calls = Arc::default();
        let names: Vec<String> = ["a", "b", "bad", "c"].map(String::from).to_vec();
        let infos = resolve_info_batch(fake_runner(&["bad"], calls), &names, false)
            .await
            .unwrap();
        let mut got: Vec<_> = infos.keys().cloned().collect();
        got.sort();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn batches_at_most_fifty_names() {
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let names: Vec<String> = (0..120).map(|i| format!("p{i}")).collect();
        let infos = resolve_info_batch(fake_runner(&[], calls.clone()), &names, false)
            .await
            .unwrap();
        assert_eq!(infos.len(), 120);
        let mut sizes: Vec<_> = calls.lock().unwrap().iter().map(Vec::len).collect();
        sizes.sort();
        assert_eq!(sizes, [20, 50, 50]);
    }

    #[tokio::test]
    async fn bisection_stays_within_concurrency_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let run: Arc<InfoRunner> = Arc::new({
            let in_flight = in_flight.clone();
            let peak = peak.clone();
            move |_, _| {
                let in_flight = in_flight.clone();
                let peak = peak.clone();
                Box::pin(async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    Err(RunError::Exit)
                })
            }
        });
        let names: Vec<String> = (0..120).map(|i| format!("p{i}")).collect();
        let infos = resolve_info_batch(run, &names, false).await.unwrap();
        assert!(infos.is_empty());
        assert!(peak.load(Ordering::SeqCst) <= INFO_CONCURRENCY);
    }

    #[tokio::test]
    async fn run_failure_is_not_bisected() {
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let recorded = calls.clone();
        let run: Arc<InfoRunner> = Arc::new(move |names, _| {
            recorded.lock().unwrap().push(names);
            Box::pin(async { Err(RunError::Other(anyhow!("no brew"))) })
        });
        let names: Vec<String> = ["a", "b"].map(String::from).to_vec();
        assert!(resolve_info_batch(run, &names, false).await.is_err());
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn upgrade_args_cases() {
        assert_eq!(upgrade_args(&["git"], false), ["upgrade", "git"]);
        assert_eq!(
            upgrade_args(&["a", "b"], true),
            ["upgrade", "--cask", "a", "b"]
        );
    }

    #[test]
    fn upgrade_failure_includes_output_and_password_hint() {
        use std::os::unix::process::ExitStatusExt;
        let failure = |stderr: &str| {
            upgrade_failure(
                "brew upgrade --cask app",
                &Output {
                    status: std::process::ExitStatus::from_raw(1 << 8),
                    stdout: b"==> Upgrading app\n".to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                },
            )
            .to_string()
        };
        assert_eq!(
            failure("Error: boom\n"),
            "brew upgrade --cask app: exit status: 1\n==> Upgrading app\nError: boom"
        );
        assert!(
            failure("sudo: a terminal is required to read the password\n")
                .ends_with("use upgrade all, which runs in the terminal.")
        );
    }

    #[tokio::test]
    async fn resolve_info_aligns_with_packages() {
        let package = |name: &str, is_cask| Package {
            name: name.into(),
            installed: "".into(),
            current: "".into(),
            is_cask,
        };
        let packages = [
            package("a", false),
            package("bad", false),
            package("c", true),
        ];
        let infos = resolve_info(fake_runner(&["bad"], Arc::default()), &packages).await;
        assert_eq!(
            infos
                .iter()
                .map(|info| info.homepage.as_str())
                .collect::<Vec<_>>(),
            ["https://a", "", "https://c"]
        );
    }
}
