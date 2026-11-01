use anyhow::{Context, Result};
use base64::Engine;
use futures::FutureExt;
use futures::future::{BoxFuture, join_all};
use serde::Deserialize;
use tokio::sync::OnceCell;

use super::forge::{Forge, Range, names_of_kind};
use super::{Client, Commit, Release};
use crate::version::matches_tag;

const MAX_COMMIT_PAGES: usize = 3;

/// The GitHub API endpoints and the token sent only to them.
#[derive(Debug)]
pub(super) struct GitHubApi {
    api_base: String,
    raw_base: String,
    token: OnceCell<Option<String>>,
}

impl GitHubApi {
    pub(super) fn new() -> Self {
        // Lets tests/e2e.rs point the real binary at a mock server.
        match std::env::var("BREW_CHANGES_GITHUB_API") {
            Ok(base) => Self::at(&base),
            Err(_) => Self {
                api_base: "https://api.github.com".into(),
                raw_base: "https://raw.githubusercontent.com".into(),
                token: OnceCell::new(),
            },
        }
    }

    /// A mock server serving both the API and raw files (under `/raw`).
    pub(super) fn at(base: &str) -> Self {
        Self {
            api_base: base.into(),
            raw_base: format!("{base}/raw"),
            token: OnceCell::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_token(mut self, token: Option<&str>) -> Self {
        self.token = OnceCell::new_with(Some(token.map(String::from)));
        self
    }

    pub(super) fn owns(&self, url: &str) -> bool {
        url.starts_with(&format!("{}/", self.api_base))
    }

    /// Resolved once because it may spawn `gh`.
    pub(super) async fn token(&self) -> Option<&str> {
        self.token
            .get_or_init(|| async {
                for variable in ["GITHUB_TOKEN", "GH_TOKEN"] {
                    if let Ok(value) = std::env::var(variable)
                        && !value.is_empty()
                    {
                        return Some(value);
                    }
                }
                // No `gh` on PATH, or not logged in, means unauthenticated requests.
                let output = tokio::process::Command::new("gh")
                    .args(["auth", "token"])
                    .output()
                    .await
                    .ok()?;
                let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
                (output.status.success() && !token.is_empty()).then_some(token)
            })
            .await
            .as_deref()
    }
}

#[derive(Debug)]
pub struct GitHub {
    owner: String,
    repo: String,
}

impl GitHub {
    pub fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    pub(super) fn detect(url: &str) -> Option<Self> {
        parse_github_url(url).map(|parsed| Self::new(parsed.owner, parsed.repo))
    }

    fn api_url(&self, client: &Client, path: &str) -> String {
        format!(
            "{}/repos/{}/{}/{path}",
            client.github.api_base, self.owner, self.repo
        )
    }

    /// Falls back to raw content because the Contents API returns no content
    /// for files over 1 MB (aws-cli's CHANGELOG.rst).
    pub(super) async fn fetch_file_at(
        &self,
        client: &Client,
        path: &str,
        git_ref: Option<&str>,
    ) -> Result<String> {
        #[derive(Deserialize)]
        struct Contents {
            #[serde(default)]
            content: String,
        }
        let mut url = self.api_url(client, &format!("contents/{path}"));
        if let Some(git_ref) = git_ref {
            url += &format!("?ref={git_ref}");
        }
        if let Ok(contents) = client.get_json::<Contents>(&url).await
            && let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(contents.content.replace('\n', ""))
            && !decoded.is_empty()
        {
            return Ok(String::from_utf8_lossy(&decoded).into_owned());
        }
        let raw_url = format!(
            "{}/{}/{}/{}/{path}",
            client.github.raw_base,
            self.owner,
            self.repo,
            git_ref.unwrap_or("HEAD")
        );
        Ok(client.fetch_raw(&raw_url).await?.0)
    }

    async fn fetch_github_releases(
        &self,
        client: &Client,
        range: Range<'_>,
    ) -> Result<Vec<Release>> {
        #[derive(Deserialize)]
        struct Tag {
            name: String,
        }
        let url = self.api_url(client, "releases?per_page=100");
        let mut walk = client
            .fetch_pages::<Vec<Release>>(url, |release| release, range)
            .await?;
        if walk.saw_any {
            walk.releases = self
                .find_target_if_missing(client, walk.releases, range)
                .await;
        }
        let to_release = |tag: Tag| Release {
            tag_name: tag.name,
            ..Default::default()
        };
        client
            .releases_or_tags::<Vec<Tag>>(
                walk,
                self.api_url(client, "tags?per_page=100"),
                to_release,
                range,
            )
            .await
    }

    /// A just-published release can be missing from the list (its search index
    /// lags) though `/releases/latest` has it. Otherwise the target is usually
    /// older than the whole first page (llama.cpp's per-commit builds), so it's
    /// fetched by tag and returned alone.
    async fn find_target_if_missing(
        &self,
        client: &Client,
        mut filtered: Vec<Release>,
        range: Range<'_>,
    ) -> Vec<Release> {
        let Range {
            target,
            prereleases_only,
            ..
        } = range;
        if target.is_empty()
            || filtered
                .iter()
                .any(|release| matches_tag(&release.tag_name, target))
        {
            return filtered;
        }
        // `/releases/latest` is never a pre-release.
        if !prereleases_only
            && let Ok(latest) = client
                .get_json::<Release>(&self.api_url(client, "releases/latest"))
                .await
            && matches_tag(&latest.tag_name, target)
        {
            // The list is newest first and holds only releases older than the target.
            filtered.insert(0, latest);
            return filtered;
        }
        let tags = [format!("v{target}"), target.to_string()];
        // Both at once, preferring the first.
        join_all(tags.map(|tag| async move {
            client
                .get_json::<Release>(&self.api_url(client, &format!("releases/tags/{tag}")))
                .await
        }))
        .await
        .into_iter()
        .flatten()
        .find(|release| release.prerelease || !prereleases_only)
        .map_or(filtered, |release| vec![release])
    }

    /// `since` is inclusive, so it starts a second after the installed commit.
    async fn fetch_github_commits(
        &self,
        client: &Client,
        installed: &str,
        current: &str,
    ) -> Result<Vec<Commit>> {
        let commit_time = |version: &str| {
            version
                .rsplit('.')
                .next()
                .and_then(|seconds| seconds.parse::<i64>().ok())
                .with_context(|| format!("version {version:?} doesn't end in a Unix time"))
        };
        let mut query = format!(
            "commits?per_page=100&until={}",
            rfc3339(commit_time(current)?)
        );
        if !installed.is_empty() {
            query += &format!("&since={}", rfc3339(commit_time(installed)? + 1));
        }
        let mut commits = Vec::new();
        let mut next = Some(self.api_url(client, &query));
        for _ in 0..MAX_COMMIT_PAGES {
            let Some(url) = next.take() else { break };
            let (entries, next_url) = client
                .get_json_page::<Vec<CommitEntry>>(&url)
                .await
                .with_context(|| format!("listing commits for {}", self.path()))?;
            next = next_url;
            commits.extend(entries.into_iter().map(|entry| {
                Commit {
                    sha: entry.sha,
                    date: entry.commit.committer.date,
                    subject: entry
                        .commit
                        .message
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                }
            }));
        }
        Ok(commits)
    }

    /// GitHub's RFC 3339 UTC timestamps compare correctly as strings.
    async fn last_commit_date(&self, client: &Client, filename: &str) -> Option<String> {
        let url = self.api_url(client, &format!("commits?path={filename}&per_page=1"));
        let commits: Vec<CommitEntry> = client.get_json(&url).await.ok()?;
        commits
            .into_iter()
            .next()
            .map(|commit| commit.commit.committer.date)
    }
}

impl Forge for GitHub {
    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn repo_url(&self) -> String {
        format!("https://github.com/{}", self.path())
    }

    fn display_name(&self) -> String {
        self.path()
    }

    fn releases_url(&self) -> String {
        format!("{}/releases", self.repo_url())
    }

    fn list_root<'a>(&'a self, client: &'a Client) -> BoxFuture<'a, Result<Vec<String>>> {
        async move {
            let entries = client
                .get_json(&self.api_url(client, "contents/"))
                .await
                .with_context(|| format!("listing files of {}", self.path()))?;
            Ok(names_of_kind(entries, "file"))
        }
        .boxed()
    }

    fn fetch_file<'a>(
        &'a self,
        client: &'a Client,
        filename: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        self.fetch_file_at(client, filename, None).boxed()
    }

    fn fetch_releases<'a>(
        &'a self,
        client: &'a Client,
        range: Range<'a>,
    ) -> BoxFuture<'a, Result<Vec<Release>>> {
        self.fetch_github_releases(client, range).boxed()
    }

    /// The most recently committed candidate wins, being the one still maintained.
    fn choose_changelog<'a>(
        &'a self,
        client: &'a Client,
        candidates: Vec<String>,
    ) -> BoxFuture<'a, Option<String>> {
        async move {
            if candidates.len() <= 1 {
                return candidates.into_iter().next();
            }
            let dates = join_all(
                candidates
                    .iter()
                    .map(|name| self.last_commit_date(client, name)),
            )
            .await;
            // Ties and unknown dates go to the earlier candidate.
            let mut best: Option<(String, Option<String>)> = None;
            for (name, date) in candidates.into_iter().zip(dates) {
                let later = |best_date: &Option<String>| match (&date, best_date) {
                    (Some(date), Some(best_date)) => date > best_date,
                    _ => false,
                };
                if best.as_ref().is_none_or(|(_, best_date)| later(best_date)) {
                    best = Some((name, date));
                }
            }
            best.map(|(name, _)| name)
        }
        .boxed()
    }

    fn fetch_commits<'a>(
        &'a self,
        client: &'a Client,
        installed: &'a str,
        current: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Commit>>> {
        self.fetch_github_commits(client, installed, current)
            .boxed()
    }
}

struct GitHubUrl<'a> {
    owner: &'a str,
    repo: &'a str,
    rest: &'a str,
    raw: bool,
}

/// Accepts github.com (https or ssh) and raw.githubusercontent.com URLs. The
/// host must be exactly github.com (optionally www.), not a subdomain like
/// docs.github.com. Repo names can contain dots ("llama.cpp"), so only a
/// final ".git" is stripped.
fn parse_github_url(url: &str) -> Option<GitHubUrl<'_>> {
    const HOSTS: [(&str, bool); 2] = [("github.com", false), ("raw.githubusercontent.com", true)];
    let (host_end, raw) = url
        .char_indices()
        .filter(|(index, _)| {
            let before = &url[..*index];
            before.is_empty()
                || before.ends_with("//")
                || before.ends_with('@')
                || before.ends_with("//www.")
        })
        .find_map(|(index, _)| {
            HOSTS.iter().find_map(|(host, raw)| {
                let after = url[index..].strip_prefix(host)?;
                let separator_ok = after.starts_with('/') || (!raw && after.starts_with(':'));
                separator_ok.then_some((index + host.len() + 1, *raw))
            })
        })?;
    let (owner, after_owner) = url[host_end..].split_once('/')?;
    let repo_end = after_owner
        .find(['/', '?', '#'])
        .unwrap_or(after_owner.len());
    let (segment, rest) = after_owner.split_at(repo_end);
    let repo = segment
        .strip_suffix(".git")
        .filter(|repo| !repo.is_empty())
        .unwrap_or(segment);
    (!owner.is_empty() && !repo.is_empty()).then_some(GitHubUrl {
        owner,
        repo,
        rest,
        raw,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct GitHubFile<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub git_ref: &'a str,
    pub path: &'a str,
}

/// Lets a curated file URL be fetched through the authenticated API.
pub(super) fn parse_github_file_url(url: &str) -> Option<GitHubFile<'_>> {
    let parsed = parse_github_url(url)?;
    let mut rest = parsed.rest.trim_start_matches('/');
    if !parsed.raw {
        let (kind, after) = rest.split_once('/')?;
        if kind != "blob" && kind != "raw" {
            return None;
        }
        rest = after;
    }
    let (git_ref, path) = rest.split_once('/')?;
    (!git_ref.is_empty() && !path.is_empty()).then_some(GitHubFile {
        owner: parsed.owner,
        repo: parsed.repo,
        git_ref,
        path,
    })
}

/// A commit as GitHub's commits API lists it.
#[derive(Deserialize)]
struct CommitEntry {
    #[serde(default)]
    sha: String,
    commit: CommitDetails,
}

#[derive(Deserialize)]
struct CommitDetails {
    #[serde(default)]
    message: String,
    committer: Committer,
}

#[derive(Deserialize)]
struct Committer {
    date: String,
}

/// Howard Hinnant's `civil_from_days`, to avoid a date crate for one format.
fn rfc3339(unix_seconds: i64) -> String {
    let (days, seconds) = (
        unix_seconds.div_euclid(86_400),
        unix_seconds.rem_euclid(86_400),
    );
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_from_unix_time() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_793_383_622), "2026-10-30T18:07:02Z");
    }

    #[test]
    fn parse_github_file_url_cases() {
        let file = |owner, repo, git_ref, path| {
            Some(GitHubFile {
                owner,
                repo,
                git_ref,
                path,
            })
        };
        let cases = [
            (
                "https://github.com/dart-lang/sdk/blob/main/CHANGELOG.md",
                file("dart-lang", "sdk", "main", "CHANGELOG.md"),
            ),
            (
                "https://github.com/owner/repo/raw/main/nested/dir/NEWS",
                file("owner", "repo", "main", "nested/dir/NEWS"),
            ),
            (
                "https://raw.githubusercontent.com/owner/repo/main/CHANGELOG.md",
                file("owner", "repo", "main", "CHANGELOG.md"),
            ),
            ("https://curl.se/changes.html", None),
            ("https://github.com/owner/repo/releases", None),
        ];
        for (url, want) in cases {
            assert_eq!(parse_github_file_url(url), want, "{url}");
        }
    }
}
