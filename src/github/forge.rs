use anyhow::{Context, Result, anyhow};
use futures::FutureExt;
use futures::future::{BoxFuture, join_all, ready};
use serde::Deserialize;

use super::{Client, Commit, Release};

/// A repo on a code forge. The forge is chosen once, in `resolve_source`; a
/// new forge is one more implementation plus its URL detection.
pub trait Forge: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;
    /// "owner/repo", where a GitLab owner can be a nested group.
    fn path(&self) -> String;
    fn repo_url(&self) -> String;

    /// The repo URL without its scheme, since other forges are self-hosted.
    fn display_name(&self) -> String {
        let url = self.repo_url();
        url.split_once("://")
            .map_or_else(|| url.clone(), |(_, rest)| rest.to_string())
    }

    fn file_url(&self, filename: &str) -> String {
        format!("{}/blob/HEAD/{filename}", self.repo_url())
    }

    fn releases_url(&self) -> String;

    fn releases_label(&self) -> String {
        format!("{} releases", self.name())
    }

    /// Looks up whatever the forge's API needs before the repo can be read.
    fn resolve<'a>(&'a self, _client: &'a Client) -> BoxFuture<'a, Result<()>> {
        ready(Ok(())).boxed()
    }

    /// Only regular files, so a `NEWS/` directory isn't taken for a changelog.
    fn list_root<'a>(&'a self, client: &'a Client) -> BoxFuture<'a, Result<Vec<String>>>;

    fn fetch_file<'a>(
        &'a self,
        client: &'a Client,
        filename: &'a str,
    ) -> BoxFuture<'a, Result<String>>;

    fn fetch_releases<'a>(
        &'a self,
        client: &'a Client,
        range: Range<'a>,
    ) -> BoxFuture<'a, Result<Vec<Release>>>;

    /// `candidates` are in order of preference.
    fn choose_changelog<'a>(
        &'a self,
        _client: &'a Client,
        candidates: Vec<String>,
    ) -> BoxFuture<'a, Option<String>> {
        ready(candidates.into_iter().next()).boxed()
    }

    fn fetch_commits<'a>(
        &'a self,
        _client: &'a Client,
        _installed: &'a str,
        _current: &'a str,
    ) -> BoxFuture<'a, Result<Vec<Commit>>> {
        ready(Err(anyhow!(
            "commit lists are only supported for GitHub repos, not {}",
            self.name()
        )))
        .boxed()
    }
}

impl PartialEq for dyn Forge {
    fn eq(&self, other: &Self) -> bool {
        (self.name(), self.repo_url()) == (other.name(), other.repo_url())
    }
}

impl Eq for dyn Forge {}

#[derive(Debug, Clone, Copy)]
pub struct Range<'a> {
    pub installed: &'a str,
    pub target: &'a str,
    pub prereleases_only: bool,
}

/// A directory listing entry as GitHub and GitLab return it.
#[derive(Deserialize)]
pub(super) struct Entry {
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

pub(super) fn names_of_kind(entries: Vec<Entry>, kind: &str) -> Vec<String> {
    entries
        .into_iter()
        .filter(|entry| entry.kind == kind)
        .map(|entry| entry.name)
        .collect()
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
                .get_json(&client.repo_url(&self.owner, &self.repo, "contents/"))
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
        client
            .fetch_github_file(&self.owner, &self.repo, filename, None)
            .boxed()
    }

    fn fetch_releases<'a>(
        &'a self,
        client: &'a Client,
        range: Range<'a>,
    ) -> BoxFuture<'a, Result<Vec<Release>>> {
        client
            .fetch_releases(
                &self.owner,
                &self.repo,
                range.installed,
                range.target,
                range.prereleases_only,
            )
            .boxed()
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
                    .map(|name| client.last_commit_date(&self.owner, &self.repo, name)),
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
        client
            .fetch_commits(&self.owner, &self.repo, installed, current)
            .boxed()
    }
}
