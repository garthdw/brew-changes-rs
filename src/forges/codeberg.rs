use anyhow::{Context, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Deserialize;

use super::forge::{Forge, ForgeUrl, Range, Repo};
use super::releases::{Walk, null_as_default};
use super::{Client, Release, StatusError};

/// Codeberg runs Forgejo, whose releases API matches GitHub's release fields.
#[derive(Debug)]
pub struct Codeberg(Repo);

impl Codeberg {
    #[cfg(test)]
    pub fn new(base: impl Into<String>, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self(Repo::new(base, owner, repo))
    }

    pub(super) fn detect(url: &ForgeUrl) -> Option<Self> {
        if url.host != "codeberg.org" {
            return None;
        }
        Repo::from_segments(&url.base, url.segments.get(..2)?).map(Self)
    }

    fn api(&self) -> String {
        format!("{}/api/v1/repos/{}", self.0.base, self.path())
    }
}

impl Forge for Codeberg {
    fn name(&self) -> &'static str {
        "Codeberg"
    }

    fn path(&self) -> String {
        self.0.path()
    }

    fn repo_url(&self) -> String {
        self.0.url()
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
        async move {
            let api = self.api();
            let walk = match client
                .fetch_pages::<Vec<Release>>(
                    format!("{api}/releases?limit=50"),
                    |release| release,
                    range,
                )
                .await
            {
                Ok(walk) => walk,
                Err(error)
                    if error
                        .downcast_ref::<StatusError>()
                        .is_some_and(|error| error.status == 404) =>
                {
                    Walk::default()
                }
                Err(error) => return Err(error),
            };
            let to_release = |tag: Tag| Release {
                tag_name: tag.name,
                body: tag.message,
                ..Default::default()
            };
            client
                .releases_or_tags::<Vec<Tag>>(
                    walk,
                    format!("{api}/tags?limit=50"),
                    to_release,
                    range,
                )
                .await
        }
        .boxed()
    }
}
