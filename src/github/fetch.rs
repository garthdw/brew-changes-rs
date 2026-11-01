use anyhow::Result;

use super::forge::{Forge, Range};
use super::github_com::{GitHub, parse_github_file_url};
use super::{Client, Release, Source};
use crate::changelog::{Order, Section, trim_to_range};
use crate::template;
use crate::version::matches_tag;

const BASE_NAMES: [&str; 4] = ["CHANGELOG", "CHANGES", "HISTORY", "NEWS"];
const EXTENSIONS: [&str; 3] = [".md", ".rst", ""];

#[derive(Debug)]
pub enum Changelog {
    NoSource,
    /// When the version heading isn't located, `section` is the opening lines.
    Manual {
        section: String,
        found: bool,
        /// The heading that ended the section; `None` when truncated.
        end: Option<String>,
    },
    /// Distinct from `ManualFailed` because the opening lines of such a page
    /// are site navigation, not worth showing as an excerpt.
    ManualHtmlWithoutVersion,
    ManualFailed(anyhow::Error),
    File {
        filename: String,
        section: String,
        end: Option<String>,
    },
    Releases {
        /// The changelog file that didn't document the version, so the UI can
        /// still link to it.
        fallback_file: Option<String>,
        /// Why the changelog file couldn't be listed or fetched, so an
        /// outage isn't mistaken for the file not documenting the version.
        file_error: Option<anyhow::Error>,
        result: Result<Releases>,
    },
    /// Newest first.
    Commits(Result<Vec<Commit>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub date: String,
    pub subject: String,
}

#[derive(Debug, Default)]
pub struct Releases {
    pub list: Vec<Release>,
    pub found: bool,
}

impl Changelog {
    pub fn into_filename(self) -> Option<String> {
        match self {
            Self::File { filename, .. } => Some(filename),
            Self::Releases { fallback_file, .. } => fallback_file,
            _ => None,
        }
    }
}

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
        // An empty installed version means nothing is installed, so it stays empty.
        let format = |version: &str| {
            if source.version_format.is_empty() || version.is_empty() {
                version.to_string()
            } else {
                template::expand(&source.version_format, version)
            }
        };
        let (installed, current) = (format(installed), format(current));
        if !source.manual_url.is_empty() {
            return self
                .fetch_manual_section(
                    &source.manual_url,
                    &current,
                    &installed,
                    &source.heading,
                    source.order,
                )
                .await;
        }
        let Some(forge) = &source.forge else {
            return Changelog::NoSource;
        };
        if source.commit_log {
            return Changelog::Commits(forge.fetch_commits(self, &installed, &current).await);
        }
        if let Err(error) = forge.resolve(self).await {
            return Changelog::Releases {
                fallback_file: None,
                file_error: None,
                result: Err(error),
            };
        }

        let mut fallback_file = None;
        let mut file_error = None;
        // A failed listing or file fetch still tries releases, which may work.
        match self.resolve_changelog_filename(forge.as_ref()).await {
            Ok(Some(filename)) => {
                on_stage(Stage::File(filename.clone()));
                match forge.fetch_file(self, &filename).await {
                    Ok(content) => {
                        if let Section {
                            text: section,
                            found: true,
                            end,
                        } = trim_to_range(
                            &content,
                            &current,
                            &installed,
                            &source.heading,
                            source.order,
                        ) {
                            return Changelog::File {
                                filename,
                                section,
                                end,
                            };
                        }
                    }
                    Err(error) => file_error = Some(error),
                }
                fallback_file = Some(filename);
            }
            Ok(None) => {}
            Err(error) => file_error = Some(error),
        }

        on_stage(Stage::Releases);
        let result = forge
            .fetch_releases(
                self,
                Range {
                    installed: &installed,
                    target: &current,
                    prereleases_only: source.prereleases_only,
                },
            )
            .await
            .map(|list| Releases {
                found: list
                    .iter()
                    .any(|release| matches_tag(&release.tag_name, &current)),
                list,
            });
        Changelog::Releases {
            fallback_file,
            file_error,
            result,
        }
    }

    async fn fetch_manual_section(
        &self,
        url: &str,
        new_version: &str,
        installed: &str,
        heading: &str,
        order: Order,
    ) -> Changelog {
        let fetched = match parse_github_file_url(url) {
            Some(file) => GitHub::new(file.owner, file.repo)
                .fetch_file_at(self, file.path, Some(file.git_ref))
                .await
                .map(|content| (content, false)),
            None => self.fetch_raw(url).await,
        };
        let (content, is_html) = match fetched {
            Ok(fetched) => fetched,
            Err(error) => return Changelog::ManualFailed(error),
        };
        let Section { text, found, end } =
            trim_to_range(&content, new_version, installed, heading, order);
        if is_html && !found {
            return Changelog::ManualHtmlWithoutVersion;
        }
        Changelog::Manual {
            section: text,
            found,
            end,
        }
    }

    /// One root listing instead of probing each filename, which adds seconds
    /// on repos where the first candidates miss. Names match in any case
    /// (htop's `ChangeLog`), preferring the uppercase one when both exist.
    async fn resolve_changelog_filename(&self, forge: &dyn Forge) -> Result<Option<String>> {
        let files = forge.list_root(self).await?;
        let candidates: Vec<String> = BASE_NAMES
            .iter()
            .flat_map(|base| {
                EXTENSIONS
                    .iter()
                    .map(move |extension| format!("{base}{extension}"))
            })
            .filter_map(|name| {
                files
                    .iter()
                    .find(|file| **file == name)
                    .or_else(|| files.iter().find(|file| file.eq_ignore_ascii_case(&name)))
                    .cloned()
            })
            .collect();
        Ok(forge.choose_changelog(self, candidates).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Release;
    use crate::github::codeberg::Codeberg;
    use crate::github::gitlab::GitLab;
    use crate::github::tangled::Tangled;
    use crate::github::test_support::{serve, setup};
    use base64::Engine;
    use serde_json::json;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn base64_encode(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    const GITHUB_REPO: fn() -> GitHub = || GitHub::new("o", "r");

    fn listing(names: &[&str]) -> serde_json::Value {
        names
            .iter()
            .map(|name| json!({"name": name, "path": name, "type": "file"}))
            .collect()
    }

    fn commit_on(date: &str) -> serde_json::Value {
        json!([{"commit": {"committer": {"date": date}}}])
    }

    fn manual(changelog: Changelog) -> (String, bool) {
        match changelog {
            Changelog::Manual { section, found, .. } => (section, found),
            other => panic!("not a manual section: {other:?}"),
        }
    }

    fn releases(changelog: Changelog) -> (Option<String>, Releases) {
        match changelog {
            Changelog::Releases {
                fallback_file,
                result: Ok(releases),
                ..
            } => (fallback_file, releases),
            other => panic!("not releases: {other:?}"),
        }
    }

    fn repo() -> Source {
        Source::with_forge(GITHUB_REPO())
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
            .fetch_manual_section(
                &format!("{}/notes", server.uri()),
                "2.0.0",
                "1.0.0",
                "",
                Order::NewestFirst,
            )
            .await;
        assert_eq!(manual(got), ("## 2.0.0\nnew stuff".into(), true));
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
            .fetch_manual_section(&url, "2.0.0", "1.0.0", "", Order::NewestFirst)
            .await;
        assert_eq!(
            manual(got),
            ("## Version 2.0.0\n\n- new stuff\n".into(), true)
        );

        let got = client
            .fetch_manual_section(&url, "3.0.0", "2.0.0", "", Order::NewestFirst)
            .await;
        assert!(
            matches!(got, Changelog::ManualHtmlWithoutVersion),
            "{got:?}"
        );
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
                "",
                Order::NewestFirst,
            )
            .await;
        assert_eq!(manual(got), ("## 2.0.0\nnew stuff".into(), true));
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
            GITHUB_REPO()
                .fetch_file_at(&client, "BIG.rst", None)
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
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .unwrap()
                .as_deref(),
            Some("CHANGELOG.md")
        );
    }

    #[tokio::test]
    async fn filename_matches_any_case() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["README.md", "ChangeLog"]),
        )
        .await;
        assert_eq!(
            client
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .unwrap()
                .as_deref(),
            Some("ChangeLog")
        );
    }

    #[tokio::test]
    async fn filename_prefers_uppercase_when_cases_differ() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["Changelog.md", "CHANGELOG.md"]),
        )
        .await;
        assert_eq!(
            client
                .resolve_changelog_filename(&GITHUB_REPO())
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
                .resolve_changelog_filename(&GITHUB_REPO())
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
            client
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .unwrap(),
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
            client
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .unwrap(),
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
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .unwrap()
                .as_deref(),
            Some("NEWS.md")
        );
    }

    #[tokio::test]
    async fn filename_unreadable_date_goes_to_earlier_candidate() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/contents/",
            listing(&["CHANGELOG.md", "NEWS.md"]),
        )
        .await;
        Mock::given(path("/repos/o/r/commits"))
            .and(query_param("path", "CHANGELOG.md"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(path("/repos/o/r/commits"))
            .and(query_param("path", "NEWS.md"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(commit_on("2026-01-01T00:00:00Z")),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .unwrap()
                .as_deref(),
            Some("CHANGELOG.md")
        );
    }

    #[tokio::test]
    async fn filename_listing_error() {
        let (server, client) = setup().await;
        Mock::given(path("/repos/o/r/contents/"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(
            client
                .resolve_changelog_filename(&GITHUB_REPO())
                .await
                .is_err()
        );
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
        let Changelog::File {
            filename, section, ..
        } = got
        else {
            panic!("not a file: {got:?}");
        };
        assert_eq!(filename, "CHANGELOG.md");
        assert_eq!(section, "## 2.0.0\nnotes\n");
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
        assert!(matches!(got, Changelog::File { .. }), "{got:?}");
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
        let (fallback_file, releases) = releases(got);
        assert!(releases.found);
        assert_eq!(fallback_file.as_deref(), Some("CHANGELOG.md"));
        assert_eq!(releases.list.len(), 1);
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
        let Changelog::Releases {
            fallback_file: None,
            file_error: Some(_),
            result: Ok(Releases { found: true, .. }),
        } = got
        else {
            panic!("expected found releases with a listing error: {got:?}");
        };
    }

    #[tokio::test]
    async fn fetch_file_error_is_kept_for_the_ui() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/contents/", listing(&["CHANGELOG.md"])).await;
        for route in [
            "/repos/o/r/contents/CHANGELOG.md",
            "/raw/o/r/HEAD/CHANGELOG.md",
        ] {
            Mock::given(path(route))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
        }
        serve(
            &server,
            "/repos/o/r/releases",
            json!([{"tag_name": "v1.0.0"}]),
        )
        .await;
        let got = client.fetch(&repo(), "1.0.0", "2.0.0", |_| {}).await;
        let Changelog::Releases {
            fallback_file: Some(filename),
            file_error: Some(error),
            result: Ok(_),
        } = got
        else {
            panic!("expected a file error: {got:?}");
        };
        assert_eq!(filename, "CHANGELOG.md");
        assert!(format!("{error:#}").contains("status 500"), "{error:#}");
    }

    #[tokio::test]
    async fn version_format_leaves_nothing_installed_empty() {
        let (_server, client) = fetch_server(
            "",
            json!([
                {"tag_name": "v2.1.0-pre", "prerelease": true},
                {"tag_name": "v2.0.0-pre", "prerelease": true},
            ]),
        )
        .await;
        let got = client
            .fetch(
                &Source {
                    prereleases_only: true,
                    version_format: "{version}-pre".into(),
                    ..repo()
                },
                "",
                "2.1.0",
                |_| {},
            )
            .await;
        let (_, releases) = releases(got);
        assert_eq!(releases.list.len(), 1, "{:?}", releases.list);
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
                    version_format: "{version}-pre".into(),
                    ..repo()
                },
                "1.0.0",
                "2.1.0",
                |_| {},
            )
            .await;
        let (_, releases) = releases(got);
        assert_eq!(
            releases
                .list
                .iter()
                .map(|release| release.tag_name.as_str())
                .collect::<Vec<_>>(),
            ["v2.1.0-pre"]
        );
        assert!(releases.found);
    }

    #[tokio::test]
    async fn fetch_version_format_rewrites_versions() {
        let (server, client) = setup().await;
        Mock::given(path("/changelog"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("v2.8\nnew stuff\nv2.7\nold stuff\n"),
            )
            .mount(&server)
            .await;
        let source = Source {
            manual_url: format!("{}/changelog", server.uri()),
            heading: "v{version}".into(),
            version_format: "{major.minor}".into(),
            ..Default::default()
        };
        let got = client.fetch(&source, "2.7.3.0", "2.8.1.0", |_| {}).await;
        assert_eq!(manual(got), ("v2.8\nnew stuff".into(), true));
    }

    #[tokio::test]
    async fn fetch_commit_log_lists_commits_between_version_timestamps() {
        let (server, client) = setup().await;
        Mock::given(path("/repos/o/r/commits"))
            .and(query_param("since", "2026-10-21T13:09:25Z"))
            .and(query_param("until", "2026-10-30T18:07:02Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "sha": "ea9eb7edb5eac4a9d175d74523db34fac0fa5180",
                "commit": {
                    "message": "Don't FOLD ldexp.\n\nDetails.",
                    "committer": {"date": "2026-10-30T18:07:02Z"},
                },
            }])))
            .mount(&server)
            .await;
        let source = Source {
            commit_log: true,
            ..repo()
        };
        let got = client
            .fetch(&source, "2.1.1792588164", "2.1.1793383622", |_| {})
            .await;
        let Changelog::Commits(Ok(commits)) = got else {
            panic!("not commits: {got:?}");
        };
        assert_eq!(
            commits,
            [Commit {
                sha: "ea9eb7edb5eac4a9d175d74523db34fac0fa5180".into(),
                date: "2026-10-30T18:07:02Z".into(),
                subject: "Don't FOLD ldexp.".into(),
            }]
        );
    }

    #[tokio::test]
    async fn fetch_commit_log_rejects_versions_without_a_timestamp() {
        let (_server, client) = setup().await;
        let source = Source {
            commit_log: true,
            ..repo()
        };
        let got = client.fetch(&source, "1.0", "2.0.x", |_| {}).await;
        assert!(matches!(got, Changelog::Commits(Err(_))), "{got:?}");
    }

    fn gitlab(server: &MockServer) -> Source {
        Source::with_forge(GitLab::new(server.uri(), "o", "r"))
    }

    const GITLAB_PROJECT: &str = "/api/v4/projects/o%2Fr";

    #[tokio::test]
    async fn gitlab_changelog_file() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{GITLAB_PROJECT}/repository/tree"),
            json!([{"name": "src", "type": "tree"}, {"name": "NEWS", "type": "blob"}]),
        )
        .await;
        Mock::given(path("/o/r/-/raw/HEAD/NEWS"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("## 2.0.0\nnotes\n## 1.0.0\nold\n", "text/plain"),
            )
            .mount(&server)
            .await;
        let got = client
            .fetch(&gitlab(&server), "1.0.0", "2.0.0", |_| {})
            .await;
        let Changelog::File {
            filename, section, ..
        } = got
        else {
            panic!("not a file: {got:?}");
        };
        assert_eq!(
            (filename.as_str(), section.as_str()),
            ("NEWS", "## 2.0.0\nnotes")
        );
    }

    #[tokio::test]
    async fn gitlab_releases() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{GITLAB_PROJECT}/repository/tree"),
            json!([]),
        )
        .await;
        serve(
            &server,
            &format!("{GITLAB_PROJECT}/releases"),
            json!([
                {"tag_name": "v2.0.0", "description": "new", "released_at": "2026-01-01T00:00:00Z"},
                {"tag_name": "v1.0.0", "description": "old"},
            ]),
        )
        .await;
        let got = client
            .fetch(&gitlab(&server), "1.0.0", "2.0.0", |_| {})
            .await;
        let (_, releases) = releases(got);
        assert!(releases.found);
        assert_eq!(
            releases.list,
            [Release {
                tag_name: "v2.0.0".into(),
                body: "new".into(),
                published_at: "2026-01-01T00:00:00Z".into(),
                ..Default::default()
            }]
        );
    }

    #[tokio::test]
    async fn gitlab_tags_when_no_releases() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{GITLAB_PROJECT}/repository/tree"),
            json!([]),
        )
        .await;
        serve(&server, &format!("{GITLAB_PROJECT}/releases"), json!([])).await;
        Mock::given(path(format!("{GITLAB_PROJECT}/repository/tags")))
            .and(query_param("order_by", "version"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "v2.0.0", "message": "tagged", "release": {"description": "notes"}},
                {"name": "v1.5.0", "message": "tagged 1.5", "release": null},
                {"name": "v1.0.0", "message": ""},
            ])))
            .mount(&server)
            .await;
        let got = client
            .fetch(&gitlab(&server), "1.0.0", "2.0.0", |_| {})
            .await;
        let (_, releases) = releases(got);
        assert_eq!(
            releases
                .list
                .iter()
                .map(|release| (release.tag_name.as_str(), release.body.as_str()))
                .collect::<Vec<_>>(),
            [("v2.0.0", "notes"), ("v1.5.0", "tagged 1.5")]
        );
    }

    const CODEBERG_REPO: &str = "/api/v1/repos/o/r";

    #[tokio::test]
    async fn codeberg_changelog_file() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/git/trees/HEAD"),
            json!({"tree": [{"path": "doc", "type": "tree"}, {"path": "CHANGELOG.md", "type": "blob"}]}),
        )
        .await;
        Mock::given(path(format!("{CODEBERG_REPO}/raw/CHANGELOG.md")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("## 2.0.0\nnotes\n## 1.0.0\nold\n", "text/plain"),
            )
            .mount(&server)
            .await;
        let source = Source::with_forge(Codeberg::new(server.uri(), "o", "r"));
        let got = client.fetch(&source, "1.0.0", "2.0.0", |_| {}).await;
        let Changelog::File {
            filename, section, ..
        } = got
        else {
            panic!("not a file: {got:?}");
        };
        assert_eq!(
            (filename.as_str(), section.as_str()),
            ("CHANGELOG.md", "## 2.0.0\nnotes")
        );
    }

    #[tokio::test]
    async fn codeberg_releases() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/git/trees/HEAD"),
            json!({"tree": []}),
        )
        .await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/releases"),
            json!([
                {"tag_name": "1.28.0", "body": "new", "published_at": "2026-09-02T13:02:54+02:00", "prerelease": false},
                {"tag_name": "1.27.0", "body": "old"},
            ]),
        )
        .await;
        let source = Source::with_forge(Codeberg::new(server.uri(), "o", "r"));
        let got = client.fetch(&source, "1.27.0", "1.28.0", |_| {}).await;
        let (_, releases) = releases(got);
        assert!(releases.found);
        assert_eq!(
            releases.list,
            [Release {
                tag_name: "1.28.0".into(),
                body: "new".into(),
                published_at: "2026-09-02T13:02:54+02:00".into(),
                ..Default::default()
            }]
        );
    }

    #[tokio::test]
    async fn codeberg_tags_when_no_releases() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/git/trees/HEAD"),
            json!({"tree": []}),
        )
        .await;
        serve(&server, &format!("{CODEBERG_REPO}/releases"), json!([])).await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/tags"),
            json!([
                {"name": "v2.0.0", "message": "Release 2.0.0\n\nnotes\n"},
                {"name": "v1.0.0", "message": ""},
            ]),
        )
        .await;
        let source = Source::with_forge(Codeberg::new(server.uri(), "o", "r"));
        let got = client.fetch(&source, "1.0.0", "2.0.0", |_| {}).await;
        let (_, releases) = releases(got);
        assert_eq!(
            releases
                .list
                .iter()
                .map(|release| (release.tag_name.as_str(), release.body.as_str()))
                .collect::<Vec<_>>(),
            [("v2.0.0", "Release 2.0.0\n\nnotes\n")]
        );
    }

    /// Forgejo answers 404 for releases when a repo turns them off.
    #[tokio::test]
    async fn codeberg_tags_when_releases_are_disabled() {
        let (server, client) = setup().await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/git/trees/HEAD"),
            json!({"tree": []}),
        )
        .await;
        Mock::given(path(format!("{CODEBERG_REPO}/releases")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        serve(
            &server,
            &format!("{CODEBERG_REPO}/tags"),
            json!([{"name": "v2.0.0", "message": "notes"}, {"name": "v1.0.0"}]),
        )
        .await;
        let source = Source::with_forge(Codeberg::new(server.uri(), "o", "r"));
        let got = client.fetch(&source, "1.0.0", "2.0.0", |_| {}).await;
        let (_, releases) = releases(got);
        assert!(releases.found);
        assert_eq!(
            releases
                .list
                .iter()
                .map(|release| release.tag_name.as_str())
                .collect::<Vec<_>>(),
            ["v2.0.0"]
        );
    }

    #[tokio::test]
    async fn github_token_is_not_sent_to_other_forges() {
        let (gitlab_server, _) = setup().await;
        let github_server = MockServer::start().await;
        let client = Client::for_test_with_token(&github_server.uri(), Some("secret"));
        serve(
            &gitlab_server,
            &format!("{GITLAB_PROJECT}/repository/tree"),
            json!([]),
        )
        .await;
        serve(
            &gitlab_server,
            &format!("{GITLAB_PROJECT}/releases"),
            json!([]),
        )
        .await;
        client
            .fetch(&gitlab(&gitlab_server), "1.0.0", "2.0.0", |_| {})
            .await;
        let requests = gitlab_server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|request| !request.headers.contains_key("authorization")),
            "{requests:?}"
        );
    }

    /// Tangled's API takes the repo's DID, which its raw-file redirect reveals.
    async fn tangled_server() -> (MockServer, Client, Source) {
        let (server, client) = setup().await;
        Mock::given(path("/o/r/raw/HEAD/README.md"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                format!(
                    "{}/xrpc/org.tangled.temp.git.getBlob?repo=did%3Aplc%3Aabc&ref=HEAD&path=README.md",
                    server.uri()
                ),
            ))
            .mount(&server)
            .await;
        Mock::given(path("/xrpc/org.tangled.temp.git.getBlob"))
            .and(query_param("path", "README.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let source = Source::with_forge(Tangled::new(server.uri(), "o", "r"));
        (server, client, source)
    }

    #[tokio::test]
    async fn tangled_changelog_file() {
        let (server, client, source) = tangled_server().await;
        Mock::given(path("/xrpc/org.tangled.temp.git.getTree"))
            .and(query_param("repo", "did:plc:abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"entries": [
                {"name": "src", "mode": "0040000"},
                {"name": "CHANGELOG.md", "mode": "0100644"},
            ]})))
            .mount(&server)
            .await;
        Mock::given(path("/xrpc/org.tangled.temp.git.getBlob"))
            .and(query_param("repo", "did:plc:abc"))
            .and(query_param("path", "CHANGELOG.md"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("## 0.12.0\nnew\n## 0.11.1\nold\n", "text/plain"),
            )
            .mount(&server)
            .await;
        let got = client.fetch(&source, "0.11.1", "0.12.0", |_| {}).await;
        let Changelog::File { section, .. } = got else {
            panic!("not a file: {got:?}");
        };
        assert_eq!(section, "## 0.12.0\nnew");
    }

    #[tokio::test]
    async fn tangled_tags() {
        let (server, client, source) = tangled_server().await;
        serve(
            &server,
            "/xrpc/org.tangled.temp.git.getTree",
            json!({"entries": []}),
        )
        .await;
        Mock::given(path("/xrpc/org.tangled.temp.git.listTags"))
            .and(query_param("repo", "did:plc:abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tags": [
                {
                    "name": "v2.0.0",
                    "annotation": {
                        "message": "two\n-----BEGIN SSH SIGNATURE-----\nxx\n-----END SSH SIGNATURE-----\n",
                        "tagger": {"when": "2026-06-19T21:15:47.000000-05:00"},
                    },
                },
                {"name": "v1.0.0", "author": {"when": "2025-01-01T00:00:00Z"}},
            ]})))
            .mount(&server)
            .await;
        let got = client.fetch(&source, "1.0.0", "2.0.0", |_| {}).await;
        let (_, releases) = releases(got);
        assert!(releases.found);
        assert_eq!(
            releases.list,
            [Release {
                tag_name: "v2.0.0".into(),
                body: "two".into(),
                published_at: "2026-06-19T21:15:47.000000-05:00".into(),
                ..Default::default()
            }]
        );
    }

    #[tokio::test]
    async fn tangled_did_is_resolved_once() {
        let (server, client, source) = tangled_server().await;
        serve(
            &server,
            "/xrpc/org.tangled.temp.git.getTree",
            json!({"entries": []}),
        )
        .await;
        serve(
            &server,
            "/xrpc/org.tangled.temp.git.listTags",
            json!({"tags": []}),
        )
        .await;
        for _ in 0..2 {
            let got = client.fetch(&source, "1.0.0", "2.0.0", |_| {}).await;
            releases(got);
        }
        let resolves = server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path() == "/o/r/raw/HEAD/README.md")
            .count();
        assert_eq!(resolves, 1);
    }

    #[tokio::test]
    async fn tangled_unresolved_repo_is_a_releases_error() {
        let (server, client) = setup().await;
        Mock::given(path("/o/r/raw/HEAD/README.md"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let source = Source::with_forge(Tangled::new(server.uri(), "o", "r"));
        let mut stages = Vec::new();
        let got = client
            .fetch(&source, "1.0.0", "2.0.0", |stage| stages.push(stage))
            .await;
        let Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Err(error),
        } = got
        else {
            panic!("expected a releases error: {got:?}");
        };
        assert!(
            format!("{error:#}").contains("resolving the Tangled repo o/r"),
            "{error:#}"
        );
        assert_eq!(stages, []);
    }

    #[tokio::test]
    async fn fetch_no_repo() {
        let got = Client::for_test("http://127.0.0.1:9")
            .fetch(&Source::default(), "1.0.0", "2.0.0", |_| {})
            .await;
        assert!(matches!(got, Changelog::NoSource), "{got:?}");
    }
}
