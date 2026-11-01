use anyhow::{Context, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::forge::{Forge, Range};
use super::releases::{drop_unrelated, null_as_default};
use super::{Client, Release, StatusError};

/// Codeberg runs Forgejo, whose releases API matches GitHub's release fields.
#[derive(Debug)]
pub struct Codeberg {
    base: String,
    owner: String,
    repo: String,
}

impl Codeberg {
    pub fn new(base: impl Into<String>, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            owner: owner.into(),
            repo: repo.into(),
        }
    }

    fn api(&self) -> String {
        format!("{}/api/v1/repos/{}", self.base, self.path())
    }
}

impl Forge for Codeberg {
    fn name(&self) -> &'static str {
        "Codeberg"
    }

    fn path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn repo_url(&self) -> String {
        format!("{}/{}", self.base, self.path())
    }

    fn file_url(&self, filename: &str) -> String {
        format!("{}/src/branch/HEAD/{filename}", self.repo_url())
    }

    fn releases_url(&self) -> String {
        format!("{}/releases", self.repo_url())
    }

    /// The git tree rather than `contents`, which looks up each entry's last
    /// commit and takes seconds on Codeberg.
    fn list_root<'a>(&'a self, client: &'a Client) -> BoxFuture<'a, Result<Vec<String>>> {
        #[derive(Deserialize)]
        struct Entry {
            path: String,
            #[serde(rename = "type")]
            kind: String,
        }
        #[derive(Deserialize)]
        struct Tree {
            tree: Vec<Entry>,
        }
        async move {
            let tree: Tree = client
                .get_json(&format!("{}/git/trees/HEAD", self.api()))
                .await
                .with_context(|| format!("listing files of {}", self.repo_url()))?;
            Ok(tree
                .tree
                .into_iter()
                .filter(|entry| entry.kind == "blob")
                .map(|entry| entry.path)
                .collect())
        }
        .boxed()
    }

    fn fetch_file<'a>(
        &'a self,
        client: &'a Client,
        filename: &'a str,
    ) -> BoxFuture<'a, Result<String>> {
        async move {
            let url = format!("{}/raw/{filename}", self.api());
            Ok(client.fetch_raw(&url).await?.0)
        }
        .boxed()
    }

    /// Like GitHub's: tags only when there are no releases at all, or
    /// releases are turned off for the repo. Pages hold at most 50 entries.
    fn fetch_releases<'a>(
        &'a self,
        client: &'a Client,
        range: Range<'a>,
    ) -> BoxFuture<'a, Result<Vec<Release>>> {
        #[derive(Deserialize)]
        struct Tag {
            name: String,
            #[serde(default, deserialize_with = "null_as_default")]
            message: String,
        }
        let Range {
            installed,
            target,
            prereleases_only,
        } = range;
        async move {
            let api = self.api();
            let walk = match client
                .fetch_pages::<Vec<Release>>(
                    format!("{api}/releases?limit=50"),
                    |release| release,
                    installed,
                    target,
                    prereleases_only,
                )
                .await
            {
                Ok(walk) => Some(walk),
                Err(error)
                    if error
                        .downcast_ref::<StatusError>()
                        .is_some_and(|error| error.status == 404) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            if let Some(walk) = walk.filter(|walk| walk.saw_any) {
                return Ok(drop_unrelated(
                    walk.releases,
                    walk.reached_installed,
                    installed,
                    target,
                ));
            }
            let to_release = |tag: Tag| Release {
                tag_name: tag.name,
                body: tag.message,
                ..Default::default()
            };
            Ok(client
                .fetch_pages::<Vec<Tag>>(
                    format!("{api}/tags?limit=50"),
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
