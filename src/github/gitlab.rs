use anyhow::{Context, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::forge::{Entry, Forge, Range, names_of_kind};
use super::releases::{drop_unrelated, null_as_default};
use super::{Client, Release};

/// `base` is the origin ("https://gitlab.gnome.org"), since self-hosted
/// instances all serve the same API.
#[derive(Debug)]
pub struct GitLab {
    base: String,
    owner: String,
    repo: String,
}

impl GitLab {
    pub fn new(base: impl Into<String>, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    fn project(&self) -> String {
        format!(
            "{}/api/v4/projects/{}",
            self.base,
            self.path().replace('/', "%2F")
        )
    }
}

impl Forge for GitLab {
    fn name(&self) -> &'static str {
        "GitLab"
    }

    fn path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn repo_url(&self) -> String {
        format!("{}/{}", self.base, self.path())
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
        let Range {
            installed,
            target,
            prereleases_only,
        } = range;
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
                    installed,
                    target,
                    prereleases_only,
                )
                .await?;
            if walk.saw_any {
                return Ok(drop_unrelated(
                    walk.releases,
                    walk.reached_installed,
                    installed,
                    target,
                ));
            }
            let to_release = |tag: GitLabTag| Release {
                tag_name: tag.name,
                body: tag
                    .release
                    .map(|release| release.description)
                    .filter(|description| !description.is_empty())
                    .unwrap_or(tag.message),
                ..Default::default()
            };
            Ok(client
                .fetch_pages::<Vec<GitLabTag>>(
                    format!("{project}/repository/tags?per_page=100&order_by=version&sort=desc"),
                    to_release,
                    installed,
                    target,
                    prereleases_only,
                )
                .await?
                .releases)
        }
        .boxed()
    }
}
