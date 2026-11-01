use anyhow::{Context, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::forge::{Entry, Forge, ForgeUrl, Range, Repo, names_of_kind};
use super::releases::null_as_default;
use super::{Client, Release};

/// Self-hosted instances all serve the same API.
#[derive(Debug)]
pub struct GitLab(Repo);

impl GitLab {
    pub fn new(base: impl Into<String>, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self(Repo::new(base, owner, repo))
    }

    /// Any `gitlab.*` host. Project paths can nest groups, so the path runs
    /// up to GitLab's `/-/` separator or a pre-separator route.
    pub(super) fn detect(url: &ForgeUrl) -> Option<Self> {
        const ROUTES: [&str; 2] = ["uploads", "repository"];
        if !url.host.starts_with("gitlab.") {
            return None;
        }
        let segments = &url.segments;
        // Instance routes, not projects: the API and `/-/project/<id>/uploads`.
        if matches!(segments.first().map(String::as_str), Some("api" | "-")) {
            return None;
        }
        let end = segments
            .iter()
            .position(|segment| ROUTES.contains(&segment.as_str()))
            .unwrap_or(segments.len());
        Repo::from_segments(&url.base, &segments[..end]).map(Self)
    }

    fn project(&self) -> String {
        format!(
            "{}/api/v4/projects/{}",
            self.0.base,
            self.path().replace('/', "%2F")
        )
    }
}

impl Forge for GitLab {
    fn name(&self) -> &'static str {
        "GitLab"
    }

    fn path(&self) -> String {
        self.0.path()
    }

    fn repo_url(&self) -> String {
        self.0.url()
    }

    fn file_url(&self, filename: &str) -> String {
        format!("{}/-/blob/HEAD/{filename}", self.repo_url())
    }

    fn releases_url(&self) -> String {
        format!("{}/-/releases", self.repo_url())
    }

    fn list_root<'a>(&'a self, client: &'a Client) -> BoxFuture<'a, Result<Vec<String>>> {
        async move {
            let entries: Vec<Entry> = client
                .get_json(&format!("{}/repository/tree?per_page=100", self.project()))
                .await
                .with_context(|| format!("listing files of {}", self.repo_url()))?;
            Ok(names_of_kind(entries, "blob"))
        }
        .boxed()
    }

    fn fetch_file<'a>(
        &'a self,
        client: &'a Client,
        filename: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        async move {
            let url = format!("{}/-/raw/HEAD/{filename}", self.repo_url());
            Ok(client.fetch_raw(&url).await?.0)
        }
        .boxed()
    }

    /// Like GitHub's: tags only when there are no releases at all.
    fn fetch_releases<'a>(
        &'a self,
        client: &'a Client,
        range: Range<'a>,
    ) -> BoxFuture<'a, Result<Vec<Release>>> {
        #[derive(Deserialize)]
        struct GitLabRelease {
            tag_name: String,
            #[serde(default, deserialize_with = "null_as_default")]
            description: String,
            #[serde(default, deserialize_with = "null_as_default")]
            released_at: String,
        }
        #[derive(Deserialize)]
        struct GitLabTag {
            name: String,
            #[serde(default, deserialize_with = "null_as_default")]
            message: String,
            #[serde(default)]
            release: Option<TagRelease>,
        }
        #[derive(Deserialize)]
        struct TagRelease {
            #[serde(default, deserialize_with = "null_as_default")]
            description: String,
        }
        async move {
            let project = self.project();
            let walk = client
                .fetch_pages::<Vec<GitLabRelease>>(
                    format!("{project}/releases?per_page=100"),
                    |release| Release {
                        tag_name: release.tag_name,
                        body: release.description,
                        published_at: release.released_at,
                        prerelease: false,
                    },
                    range,
                )
                .await?;
            let to_release = |tag: GitLabTag| Release {
                tag_name: tag.name,
                body: tag
                    .release
                    .map(|release| release.description)
                    .filter(|description| !description.is_empty())
                    .unwrap_or(tag.message),
                ..Default::default()
            };
            client
                .releases_or_tags::<Vec<GitLabTag>>(
                    walk,
                    format!("{project}/repository/tags?per_page=100&order_by=version&sort=desc"),
                    to_release,
                    range,
                )
                .await
        }
        .boxed()
    }
}
