use anyhow::{Context, Result};
use futures::future::join_all;
use serde::Deserialize;

use super::source::parse_github_file_url;
use super::{Client, Release, Source};
use crate::changelog::trim_to_range;
use crate::version::{matches_tag, shorten};

const BASE_NAMES: [&str; 4] = ["CHANGELOG", "CHANGES", "HISTORY", "NEWS"];
const EXTENSIONS: [&str; 3] = [".md", ".rst", ""];

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    #[default]
    None,
    Manual,
    File,
    Releases,
}

#[derive(Debug, Default)]
pub struct Changelog {
    pub origin: Origin,
    /// Set even when releases were used because the file didn't document the
    /// version, so the UI can still link to it.
    pub filename: Option<String>,
    pub section: String,
    pub releases: Vec<Release>,
    pub found: bool,
    pub error: Option<anyhow::Error>,
}

/// Distinct from other fetch errors because the opening lines of such a page
/// are site navigation, not worth showing as an excerpt.
#[derive(Debug)]
pub struct HtmlWithoutVersion;

impl std::fmt::Display for HtmlWithoutVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("changelog is an HTML page that doesn't document this version")
    }
}

impl std::error::Error for HtmlWithoutVersion {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Set by the UI when a fetch starts; `fetch` reports only the later ones.
    Changelog,
    File(String),
    Releases,
}

impl Client {
    /// `on_stage` is called before each network stage after the first, so the
    /// UI can show progress.
    pub async fn fetch(
        &self,
        source: &Source,
        installed: &str,
        current: &str,
        mut on_stage: impl FnMut(Stage),
    ) -> Changelog {
        let (installed, current) = if source.version_parts > 0 {
            (
                shorten(installed, source.version_parts),
                shorten(current, source.version_parts),
            )
        } else {
            (installed.to_string(), current.to_string())
        };
        if !source.manual_url.is_empty() {
            return match self
                .fetch_manual_section(
                    &source.manual_url,
                    &current,
                    &installed,
                    source.lax_headings,
                )
                .await
            {
                Ok((section, found)) => Changelog {
                    origin: Origin::Manual,
                    section,
                    found,
                    ..Default::default()
                },
                Err(error) => Changelog {
                    origin: Origin::Manual,
                    error: Some(error),
                    ..Default::default()
                },
            };
        }
        if !source.has_repo() {
            return Changelog::default();
        }

        let mut changelog = Changelog {
            origin: Origin::Releases,
            ..Default::default()
        };
        // A failed listing is treated as "no file"; releases may still work.
        if let Ok(Some(filename)) = self
            .resolve_changelog_filename(&source.owner, &source.repo)
            .await
        {
            on_stage(Stage::File(filename.clone()));
            if let Ok(content) = self
                .fetch_github_file(&source.owner, &source.repo, &filename, None)
                .await
                && let (section, true) =
                    trim_to_range(&content, &current, &installed, source.lax_headings)
            {
                return Changelog {
                    origin: Origin::File,
                    filename: Some(filename),
                    section,
                    found: true,
                    ..changelog
                };
            }
            changelog.filename = Some(filename);
        }

        on_stage(Stage::Releases);
        match self
            .fetch_releases(
                &source.owner,
                &source.repo,
                &installed,
                &current,
                source.prereleases_only,
            )
            .await
        {
            Ok(releases) => {
                changelog.found = releases
                    .iter()
                    .any(|release| matches_tag(&release.tag_name, &current));
                changelog.releases = releases;
            }
            Err(error) => changelog.error = Some(error),
        }
        changelog
    }

    async fn fetch_manual_section(
        &self,
        url: &str,
        new_version: &str,
        installed: &str,
        lax: bool,
    ) -> Result<(String, bool)> {
        let (content, is_html) = match parse_github_file_url(url) {
            Some(file) => (
                self.fetch_github_file(file.owner, file.repo, file.path, Some(file.git_ref))
                    .await?,
                false,
            ),
            None => self.fetch_raw(url).await?,
        };
        let (section, found) = trim_to_range(&content, new_version, installed, lax);
        if is_html && !found {
            return Err(HtmlWithoutVersion.into());
        }
        Ok((section, found))
    }

    /// One root listing instead of probing each filename, which adds seconds
    /// on repos where the first candidates miss. The most recently committed
    /// candidate wins, being the one still maintained.
    async fn resolve_changelog_filename(&self, owner: &str, repo: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Entry {
            name: String,
            #[serde(rename = "type")]
            kind: String,
        }
        let entries: Vec<Entry> = self
            .get_json(&self.repo_url(owner, repo, "contents/"))
            .await
            .with_context(|| format!("listing contents for {owner}/{repo}"))?;
        let matches: Vec<String> = BASE_NAMES
            .iter()
            .flat_map(|base| {
                EXTENSIONS
                    .iter()
                    .map(move |extension| format!("{base}{extension}"))
            })
            .filter(|name| {
                entries
                    .iter()
                    .any(|entry| entry.kind == "file" && entry.name == *name)
            })
            .collect();
        if matches.len() <= 1 {
            return Ok(matches.into_iter().next());
        }
        let dates = join_all(
            matches
                .iter()
                .map(|name| self.last_commit_date(owner, repo, name)),
        )
        .await;
        // Ties and unknown dates go to the earlier candidate.
        let mut best: Option<(String, Option<String>)> = None;
        for (name, date) in matches.into_iter().zip(dates) {
            if best.as_ref().is_none_or(|(_, best_date)| date > *best_date) {
                best = Some((name, date));
            }
        }
        Ok(best.map(|(name, _)| name))
    }

    /// GitHub's RFC 3339 UTC timestamps compare correctly as strings.
    async fn last_commit_date(&self, owner: &str, repo: &str, filename: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Commit {
            commit: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            committer: Committer,
        }
        #[derive(Deserialize)]
        struct Committer {
            date: String,
        }
        let url = self.repo_url(owner, repo, &format!("commits?path={filename}&per_page=1"));
        let commits: Vec<Commit> = self.get_json(&url).await.ok()?;
        commits
            .into_iter()
            .next()
            .map(|commit| commit.commit.committer.date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base64_encode(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    async fn serve(server: &MockServer, route: &str, body: serde_json::Value) {
        Mock::given(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn setup() -> (MockServer, Client) {
        let server = MockServer::start().await;
        let client = Client::for_test(&server.uri());
        (server, client)
    }

    fn listing(names: &[&str]) -> serde_json::Value {
        names
            .iter()
            .map(|name| json!({"name": name, "type": "file"}))
            .collect()
    }

    fn commit_on(date: &str) -> serde_json::Value {
        json!([{"commit": {"committer": {"date": date}}}])
    }

    fn repo() -> Source {
        Source {
            owner: "o".into(),
            repo: "r".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn manual_non_github() {
        let (server, client) = setup().await;
        Mock::given(path("/notes"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("## 2.0.0\nnew stuff\n## 1.0.0\nold stuff\n", "text/plain"),
            )
            .mount(&server)
            .await;
        let got = client
            .fetch_manual_section(&format!("{}/notes", server.uri()), "2.0.0", "1.0.0", false)
            .await
            .unwrap();
        assert_eq!(got, ("## 2.0.0\nnew stuff".into(), true));
    }

    #[tokio::test]
    async fn manual_html_is_converted_to_markdown() {
        let (server, client) = setup().await;
        let html = "<html><body><nav>Home</nav><h2>Version 2.0.0</h2><ul><li>new stuff</li></ul><h2>Version 1.0.0</h2><p>old stuff</p></body></html>";
        Mock::given(path("/notes"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(html, "text/html; charset=utf-8"))
            .mount(&server)
            .await;
        let url = format!("{}/notes", server.uri());
        let got = client
            .fetch_manual_section(&url, "2.0.0", "1.0.0", false)
            .await
            .unwrap();
        assert_eq!(got, ("## Version 2.0.0\n\n- new stuff\n".into(), true));

        let error = client
            .fetch_manual_section(&url, "3.0.0", "2.0.0", false)
            .await
            .unwrap_err();
        assert!(error.is::<HtmlWithoutVersion>());
    }

    #[tokio::test]
    async fn manual_github_blob_url_uses_contents_api() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/owner/repo/contents/CHANGELOG.md",
            json!({"content": base64_encode("## 2.0.0\nnew stuff\n## 1.0.0\nold\n")}),
        )
        .await;
        let got = client
            .fetch_manual_section(
                "https://github.com/owner/repo/blob/main/CHANGELOG.md",
                "2.0.0",
                "1.0.0",
                false,
            )
            .await
            .unwrap();
        assert_eq!(got, ("## 2.0.0\nnew stuff".into(), true));
    }

    #[tokio::test]
    async fn github_file_falls_back_to_raw() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/BIG.rst",
            json!({"content": ""}),
        )
        .await;
        Mock::given(path("/raw/o/r/HEAD/BIG.rst"))
            .respond_with(ResponseTemplate::new(200).set_body_string("big"))
            .mount(&server)
            .await;
        assert_eq!(
            client
                .fetch_github_file("o", "r", "BIG.rst", None)
                .await
                .unwrap(),
            "big"
        );
    }

    #[tokio::test]
    async fn filename_picks_first_match() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["README.md", "CHANGELOG.md"]),
        )
        .await;
        assert_eq!(
            client
                .resolve_changelog_filename("o", "r")
                .await
                .unwrap()
                .as_deref(),
            Some("CHANGELOG.md")
        );
    }

    #[tokio::test]
    async fn filename_falls_through_to_later_name() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["README.md", "NEWS"]),
        )
        .await;
        assert_eq!(
            client
                .resolve_changelog_filename("o", "r")
                .await
                .unwrap()
                .as_deref(),
            Some("NEWS")
        );
    }

    #[tokio::test]
    async fn filename_ignores_directories() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            json!([{"name": "CHANGELOG", "type": "dir"}]),
        )
        .await;
        assert_eq!(
            client.resolve_changelog_filename("o", "r").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn filename_none_found() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["README.md", "LICENSE"]),
        )
        .await;
        assert_eq!(
            client.resolve_changelog_filename("o", "r").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn filename_multiple_matches_picks_most_recently_committed() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["CHANGELOG.md", "NEWS.md"]),
        )
        .await;
        for (file, date) in [
            ("CHANGELOG.md", "2020-01-01T00:00:00Z"),
            ("NEWS.md", "2026-01-01T00:00:00Z"),
        ] {
            Mock::given(path("/repos/o/r/commits"))
                .and(query_param("path", file))
                .respond_with(ResponseTemplate::new(200).set_body_json(commit_on(date)))
                .mount(&server)
                .await;
        }
        assert_eq!(
            client
                .resolve_changelog_filename("o", "r")
                .await
                .unwrap()
                .as_deref(),
            Some("NEWS.md")
        );
    }

    #[tokio::test]
    async fn filename_listing_error() {
        let (server, client) = setup().await;
        Mock::given(path("/repos/o/r/contents/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(client.resolve_changelog_filename("o", "r").await.is_err());
    }

    /// A repo whose CHANGELOG.md only documents `changelog_version`.
    async fn fetch_server(
        changelog_version: &str,
        releases: serde_json::Value,
    ) -> (MockServer, Client) {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/contents/", listing(&["CHANGELOG.md"])).await;
        let content = base64_encode(&format!("## {changelog_version}\nnotes\n"));
        serve(
            &server,
            "/repos/o/r/contents/CHANGELOG.md",
            json!({"content": content}),
        )
        .await;
        serve(&server, "/repos/o/r/releases", releases).await;
        (server, client)
    }

    #[tokio::test]
    async fn fetch_uses_changelog_file() {
        let (_server, client) = fetch_server("2.0.0", json!([])).await;
        let mut stages = vec![];
        let got = client
            .fetch(&repo(), "1.0.0", "2.0.0", |stage| stages.push(stage))
            .await;
        assert_eq!(got.origin, Origin::File);
        assert!(got.found);
        assert_eq!(got.section, "## 2.0.0\nnotes\n");
        assert_eq!(stages, [Stage::File("CHANGELOG.md".into())]);
    }

    #[tokio::test]
    async fn fetch_prefers_file_documenting_target_over_newer_releases() {
        let (_server, client) = fetch_server(
            "2.0.0",
            json!([{"tag_name": "v1.19.1", "published_at": "2027-01-01T00:00:00Z"}]),
        )
        .await;
        let got = client.fetch(&repo(), "1.0.0", "2.0.0", |_| {}).await;
        assert_eq!(got.origin, Origin::File);
    }

    #[tokio::test]
    async fn fetch_file_lacking_version_falls_back_to_releases() {
        let (_server, client) = fetch_server(
            "1.0.0",
            json!([{"tag_name": "v2.0.0"}, {"tag_name": "v1.0.0"}]),
        )
        .await;
        let mut stages = vec![];
        let got = client
            .fetch(&repo(), "1.0.0", "2.0.0", |stage| stages.push(stage))
            .await;
        assert_eq!(got.origin, Origin::Releases);
        assert!(got.found);
        assert_eq!(got.filename.as_deref(), Some("CHANGELOG.md"));
        assert_eq!(got.releases.len(), 1);
        assert_eq!(
            stages,
            [Stage::File("CHANGELOG.md".into()), Stage::Releases]
        );
    }

    #[tokio::test]
    async fn fetch_listing_error_goes_to_releases() {
        let (server, client) = setup().await;
        Mock::given(path("/repos/o/r/contents/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        serve(
            &server,
            "/repos/o/r/releases",
            json!([{"tag_name": "v2.0.0"}]),
        )
        .await;
        let got = client.fetch(&repo(), "1.0.0", "2.0.0", |_| {}).await;
        assert_eq!(got.origin, Origin::Releases);
        assert!(got.found);
        assert_eq!(got.filename, None);
    }

    #[tokio::test]
    async fn fetch_prereleases_only() {
        let (_server, client) = fetch_server(
            "1.0.0",
            json!([
                {"tag_name": "v2.1.0-pre", "prerelease": true},
                {"tag_name": "v2.0.0"},
                {"tag_name": "v1.0.0-pre", "prerelease": true},
            ]),
        )
        .await;
        let got = client
            .fetch(
                &Source {
                    prereleases_only: true,
                    ..repo()
                },
                "1.0.0",
                "2.1.0",
                |_| {},
            )
            .await;
        assert_eq!(
            got.releases
                .iter()
                .map(|release| release.tag_name.as_str())
                .collect::<Vec<_>>(),
            ["v2.1.0-pre"]
        );
        assert!(got.found);
    }

    #[tokio::test]
    async fn fetch_version_parts_shortens_versions() {
        let (server, client) = setup().await;
        Mock::given(path("/changelog"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("v2.8\nnew stuff\nv2.7\nold stuff\n"),
            )
            .mount(&server)
            .await;
        let source = Source {
            manual_url: format!("{}/changelog", server.uri()),
            lax_headings: true,
            version_parts: 2,
            ..Default::default()
        };
        let got = client.fetch(&source, "2.7.3.0", "2.8.1.0", |_| {}).await;
        assert!(got.found);
        assert_eq!(got.section, "v2.8\nnew stuff");
    }

    #[tokio::test]
    async fn fetch_no_repo() {
        let got = Client::for_test("http://127.0.0.1:9")
            .fetch(&Source::default(), "1.0.0", "2.0.0", |_| {})
            .await;
        assert_eq!(got.origin, Origin::None);
    }
}
