use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::future::{BoxFuture, ready};
use serde::Deserialize;
use url::Url;

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

/// A repo on a forge that can be self-hosted, where `base` is the origin
/// ("https://gitlab.gnome.org").
#[derive(Debug)]
pub(super) struct Repo {
    pub(super) base: String,
    owner: String,
    name: String,
}

impl Repo {
    pub(super) fn new(
        base: impl Into<String>,
        owner: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// The last segment is the repo, the ones before it its owner.
    pub(super) fn from_segments(base: &str, segments: &[String]) -> Option<Self> {
        let (repo, owner) = segments.split_last()?;
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        (!owner.is_empty() && !repo.is_empty()).then(|| Self::new(base, owner.join("/"), repo))
    }

    pub(super) fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    pub(super) fn url(&self) -> String {
        format!("{}/{}", self.base, self.path())
    }
}

/// A URL on a forge that can be self-hosted, split for each forge's `detect`.
pub(super) struct ForgeUrl {
    pub(super) host: String,
    pub(super) base: String,
    /// The path up to GitLab's `/-/` separator.
    pub(super) segments: Vec<String>,
}

impl ForgeUrl {
    pub(super) fn parse(url: &str) -> Option<Self> {
        let parsed = Url::parse(url).ok()?;
        let segments = parsed
            .path()
            .trim_matches('/')
            .split("/-/")
            .next()?
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(String::from)
            .collect();
        Some(Self {
            host: parsed.host_str()?.into(),
            base: parsed.origin().ascii_serialization(),
            segments,
        })
    }
}
