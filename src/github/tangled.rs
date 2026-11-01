use anyhow::{Context, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::OnceCell;

use super::forge::{Forge, Range};
use super::releases::null_as_default;
use super::{Client, Release};

#[derive(Debug)]
pub struct Tangled {
    base: String,
    owner: String,
    repo: String,
    api: OnceCell<Api>,
}

#[derive(Debug)]
struct Api {
    xrpc: String,
    did: String,
}

impl Tangled {
    pub fn new(base: impl Into<String>, owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            owner: owner.into(),
            repo: repo.into(),
            api: OnceCell::new(),
        }
    }

    /// Tangled's API takes the repo's DID rather than its name; the web raw
    /// file route redirects to the API with it, whether or not the file exists.
    async fn api(&self, client: &Client) -> Result<&Api> {
        self.api
            .get_or_try_init(|| async {
                let response = client
                    .http
                    .get(format!("{}/raw/HEAD/README.md", self.repo_url()))
                    .send()
                    .await?;
                let api = response.url();
                let did = api
                    .query_pairs()
                    .find(|(key, _)| key == "repo")
                    .map(|(_, did)| did.into_owned())
                    .with_context(|| format!("no repo DID in {api}"))?;
                anyhow::Ok(Api {
                    xrpc: format!("{}/xrpc", api.origin().ascii_serialization()),
                    did,
                })
            })
            .await
            .with_context(|| format!("resolving the Tangled repo {}", self.path()))
    }
}

impl Forge for Tangled {
    fn name(&self) -> &'static str {
        "Tangled"
    }

    fn path(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    fn repo_url(&self) -> String {
        format!("{}/{}", self.base, self.path())
    }

    fn releases_url(&self) -> String {
        format!("{}/tags", self.repo_url())
    }

    /// Tangled has tags but no releases.
    fn releases_label(&self) -> String {
        format!("{} tags", self.name())
    }

    fn resolve<'a>(&'a self, client: &'a Client) -> BoxFuture<'a, Result<()>> {
        async move { self.api(client).await.map(|_| ()) }.boxed()
    }

    fn list_root<'a>(&'a self, client: &'a Client) -> BoxFuture<'a, Result<Vec<String>>> {
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            mode: String,
        }
        #[derive(Deserialize)]
        struct Tree {
            entries: Vec<Entry>,
        }
        async move {
            let Api { xrpc, did } = self.api(client).await?;
            let tree: Tree = client
                .get_json(&format!(
                    "{xrpc}/org.tangled.temp.git.getTree?repo={did}&ref=HEAD&path="
                ))
                .await
                .with_context(|| format!("listing files of {did}"))?;
            Ok(tree
                .entries
                .into_iter()
                // Git modes: 0100644/0100755 are files, 0040000 directories.
                .filter(|entry| entry.mode.starts_with("010"))
                .map(|entry| entry.name)
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
            let Api { xrpc, did } = self.api(client).await?;
            let url =
                format!("{xrpc}/org.tangled.temp.git.getBlob?repo={did}&ref=HEAD&path={filename}");
            Ok(client.fetch_raw(&url).await?.0)
        }
        .boxed()
    }

    fn fetch_releases<'a>(
        &'a self,
        client: &'a Client,
        range: Range<'a>,
    ) -> BoxFuture<'a, Result<Vec<Release>>> {
        async move {
            let Api { xrpc, did } = self.api(client).await?;
            Ok(client
                .fetch_pages::<TangledTags>(
                    format!("{xrpc}/org.tangled.temp.git.listTags?repo={did}&limit=100"),
                    TangledTag::into_release,
                    range.installed,
                    range.target,
                    range.prereleases_only,
                )
                .await?
                .releases)
        }
        .boxed()
    }
}

#[derive(Deserialize)]
struct TangledTags {
    tags: Vec<TangledTag>,
}

impl IntoIterator for TangledTags {
    type Item = TangledTag;
    type IntoIter = std::vec::IntoIter<TangledTag>;

    fn into_iter(self) -> Self::IntoIter {
        self.tags.into_iter()
    }
}

#[derive(Deserialize)]
struct TangledTag {
    name: String,
    #[serde(default)]
    annotation: Option<TangledAnnotation>,
    #[serde(default)]
    author: Option<TangledSignature>,
}

#[derive(Deserialize)]
struct TangledAnnotation {
    #[serde(default, deserialize_with = "null_as_default")]
    message: String,
    #[serde(default)]
    tagger: Option<TangledSignature>,
}

#[derive(Deserialize)]
struct TangledSignature {
    #[serde(default, deserialize_with = "null_as_default")]
    when: String,
}

impl TangledTag {
    /// Signed tags carry their SSH/GPG signature in the message.
    fn into_release(self) -> Release {
        let (body, tagger) = match self.annotation {
            Some(annotation) => (
                annotation
                    .message
                    .split("-----BEGIN ")
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                annotation.tagger,
            ),
            None => (String::new(), None),
        };
        Release {
            tag_name: self.name,
            body,
            published_at: tagger
                .or(self.author)
                .map(|signature| signature.when)
                .unwrap_or_default(),
            prerelease: false,
        }
    }
}
