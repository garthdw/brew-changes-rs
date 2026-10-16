use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::Client;
use crate::version::matches_tag;

const MAX_RELEASE_PAGES: usize = 3;
const RELEASE_CAP: usize = 20;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct Release {
    pub tag_name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub published_at: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub body: String,
    #[serde(default)]
    pub prerelease: bool,
}

// GitHub sends `null` for an empty release body.
fn null_as_default<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

impl Client {
    /// Falls back to plain tags only when there are no releases at all, for
    /// repos that just tag commits (docker/cli, vim).
    pub async fn fetch_releases(
        &self,
        owner: &str,
        repo: &str,
        installed: &str,
        target: &str,
        prereleases_only: bool,
    ) -> Result<Vec<Release>> {
        let url = self.repo_url(owner, repo, "releases?per_page=100");
        let (filtered, saw_any) = self
            .fetch_pages(
                url,
                |release: Release| release,
                installed,
                target,
                prereleases_only,
            )
            .await?;
        if !saw_any {
            let url = self.repo_url(owner, repo, "tags?per_page=100");
            let to_release = |tag: Tag| Release {
                tag_name: tag.name,
                ..Default::default()
            };
            return Ok(self
                .fetch_pages(url, to_release, installed, target, prereleases_only)
                .await?
                .0);
        }
        Ok(self
            .find_target_if_missing(owner, repo, filtered, target, prereleases_only)
            .await)
    }

    /// A just-published release can be missing from the list (its search index
    /// lags) though `/releases/latest` has it. Otherwise the target is usually
    /// older than the whole first page (llama.cpp's per-commit builds), so it's
    /// fetched by tag and returned alone.
    async fn find_target_if_missing(
        &self,
        owner: &str,
        repo: &str,
        mut filtered: Vec<Release>,
        target: &str,
        prereleases_only: bool,
    ) -> Vec<Release> {
        if target.is_empty()
            || filtered
                .iter()
                .any(|release| matches_tag(&release.tag_name, target))
        {
            return filtered;
        }
        let tags = if prereleases_only {
            [format!("v{target}-pre"), format!("{target}-pre")]
        } else {
            if let Ok(latest) = self
                .get_json::<Release>(&self.repo_url(owner, repo, "releases/latest"))
                .await
                && matches_tag(&latest.tag_name, target)
            {
                filtered.push(latest);
                return filtered;
            }
            [format!("v{target}"), target.to_string()]
        };
        for tag in tags {
            if let Ok(release) = self
                .get_json::<Release>(&self.repo_url(owner, repo, &format!("releases/tags/{tag}")))
                .await
                && (release.prerelease || !prereleases_only)
            {
                return vec![release];
            }
        }
        filtered
    }

    /// The bool reports whether the endpoint returned any entries at all.
    async fn fetch_pages<T: DeserializeOwned>(
        &self,
        url: String,
        to_release: impl Fn(T) -> Release,
        installed: &str,
        target: &str,
        prereleases_only: bool,
    ) -> Result<(Vec<Release>, bool)> {
        let pages = if target.is_empty() {
            MAX_RELEASE_PAGES
        } else {
            1
        };
        let mut filtered = Vec::new();
        let mut saw_any = false;
        let mut next = Some(url);
        for _ in 0..pages {
            let Some(url) = next.take() else { break };
            let (entries, next_url) = self.get_json_page::<Vec<T>>(&url).await?;
            next = next_url;
            saw_any |= !entries.is_empty();
            for release in entries.into_iter().map(&to_release) {
                if prereleases_only && !release.prerelease {
                    continue;
                }
                if accumulate(&mut filtered, release, installed, target) {
                    return Ok((filtered, saw_any));
                }
            }
        }
        if !target.is_empty() {
            filtered.truncate(RELEASE_CAP);
        }
        Ok((filtered, saw_any))
    }
}

/// Returns whether to stop. With an installed version set, a target match
/// must not stop the walk: releases skipped between installed and target
/// still belong in the result.
fn accumulate(
    filtered: &mut Vec<Release>,
    release: Release,
    installed: &str,
    target: &str,
) -> bool {
    if matches_tag(&release.tag_name, installed) {
        return true;
    }
    let is_target = matches_tag(&release.tag_name, target);
    filtered.push(release);
    (installed.is_empty() && is_target) || (target.is_empty() && filtered.len() >= RELEASE_CAP)
}

pub(super) fn next_page_url(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (target, params) = part.trim().split_once(';')?;
        let is_next = params
            .split(';')
            .any(|param| param.trim() == r#"rel="next""#);
        let url = target.trim().strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn tag(name: &str) -> Release {
        Release {
            tag_name: name.into(),
            ..Default::default()
        }
    }

    fn tags(releases: &[Release]) -> Vec<&str> {
        releases
            .iter()
            .map(|release| release.tag_name.as_str())
            .collect()
    }

    async fn serve(server: &MockServer, route: &str, body: impl serde::Serialize) {
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

    #[tokio::test]
    async fn stops_at_installed_across_pages() {
        let (server, client) = setup().await;
        Mock::given(path("/repos/o/r/releases"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json([tag("v1.0.0"), tag("v0.5.0")]))
            .mount(&server)
            .await;
        Mock::given(path("/repos/o/r/releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "Link",
                        format!(
                            r#"<{}/repos/o/r/releases?per_page=100&page=2>; rel="next""#,
                            server.uri()
                        ),
                    )
                    .set_body_json([tag("v3.0.0"), tag("v2.0.0")]),
            )
            .mount(&server)
            .await;
        let got = client
            .fetch_releases("o", "r", "1.0.0", "", false)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v3.0.0", "v2.0.0"]);
    }

    #[tokio::test]
    async fn target_beyond_twenty_but_within_first_page() {
        let (server, client) = setup().await;
        let page: Vec<_> = (1..=50)
            .rev()
            .map(|number| tag(&format!("v{number}.0.0")))
            .collect();
        serve(&server, "/repos/o/r/releases", page).await;
        let got = client
            .fetch_releases("o", "r", "", "25.0.0", false)
            .await
            .unwrap();
        assert_eq!(got.len(), 26);
        assert_eq!(got.last().unwrap().tag_name, "v25.0.0");
    }

    #[tokio::test]
    async fn skipped_version_includes_intermediate_release() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("v0.34.2"), tag("v0.34.1"), tag("v0.34.0")],
        )
        .await;
        let got = client
            .fetch_releases("o", "r", "0.34.0", "0.34.2", false)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v0.34.2", "v0.34.1"]);
    }

    #[tokio::test]
    async fn target_not_found_falls_back_to_twenty_without_a_second_page() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases/latest", tag("v100.0.0")).await;
        let page: Vec<_> = (1..=100)
            .rev()
            .map(|number| tag(&format!("v{number}.0.0")))
            .collect();
        Mock::given(path("/repos/o/r/releases"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "Link",
                        r#"<https://example.com/should-not-be-fetched>; rel="next""#,
                    )
                    .set_body_json(page),
            )
            .mount(&server)
            .await;
        let got = client
            .fetch_releases("o", "r", "", "999.0.0", false)
            .await
            .unwrap();
        // list + latest + two by-tag lookups
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
        assert_eq!(got.len(), 20);
        assert_eq!(got[0].tag_name, "v100.0.0");
    }

    #[tokio::test]
    async fn target_missing_from_list_but_latest_is_appended() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases/latest", tag("v4.6.7")).await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("v4.6.6"), tag("v4.6.5")],
        )
        .await;
        let got = client
            .fetch_releases("o", "r", "4.6.6", "4.6.7", false)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v4.6.7"]);
    }

    #[tokio::test]
    async fn target_beyond_first_page_fetched_by_tag() {
        let (server, client) = setup().await;
        let page: Vec<_> = (1..=100)
            .rev()
            .map(|number| tag(&format!("b{}", 1000 + number)))
            .collect();
        serve(&server, "/repos/o/r/releases/latest", tag("b1100")).await;
        serve(&server, "/repos/o/r/releases/tags/v0.6.0", tag("v0.6.0")).await;
        serve(&server, "/repos/o/r/releases", page).await;
        let got = client
            .fetch_releases("o", "r", "0.5.0", "0.6.0", false)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v0.6.0"]);
    }

    #[tokio::test]
    async fn prerelease_target_beyond_first_page_fetched_by_pre_tag() {
        let (server, client) = setup().await;
        let pre = |name: &str| Release {
            prerelease: true,
            ..tag(name)
        };
        let page: Vec<_> = (1..=100)
            .rev()
            .map(|number| pre(&format!("v2.{number}.0-pre")))
            .collect();
        serve(&server, "/repos/o/r/releases/tags/v1.25.0", tag("v1.25.0")).await;
        serve(
            &server,
            "/repos/o/r/releases/tags/v1.25.0-pre",
            pre("v1.25.0-pre"),
        )
        .await;
        serve(&server, "/repos/o/r/releases", page).await;
        let got = client
            .fetch_releases("o", "r", "1.24.0", "1.25.0", true)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v1.25.0-pre"]);
    }

    #[tokio::test]
    async fn tag_prefix_other_than_v() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("jq-1.8.2"), tag("jq-1.8.1"), tag("jq-1.8.0")],
        )
        .await;
        let got = client
            .fetch_releases("o", "r", "1.8.1", "", false)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["jq-1.8.2"]);
    }

    #[tokio::test]
    async fn falls_back_to_tags_when_no_releases() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases", json!([])).await;
        serve(
            &server,
            "/repos/o/r/tags",
            json!([{"name": "v2.0.0"}, {"name": "v1.0.0"}]),
        )
        .await;
        let got = client
            .fetch_releases("o", "r", "1.0.0", "", false)
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v2.0.0"]);
    }

    #[tokio::test]
    async fn no_tags_fallback_when_releases_exist_but_none_are_newer() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases", [tag("v1.0.0")]).await;
        Mock::given(path("/repos/o/r/tags"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let got = client
            .fetch_releases("o", "r", "1.0.0", "", false)
            .await
            .unwrap();
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn error_status() {
        let (server, client) = setup().await;
        Mock::given(path("/repos/o/r/releases"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(
            client
                .fetch_releases("o", "r", "1.0.0", "", false)
                .await
                .is_err()
        );
    }

    #[test]
    fn null_body_parses() {
        let release: Release =
            serde_json::from_value(json!({"tag_name": "v1", "body": null, "published_at": null}))
                .unwrap();
        assert_eq!(release, tag("v1"));
    }

    #[test]
    fn next_page_url_cases() {
        let cases = [
            ("", None),
            (
                r#"<https://a/releases?page=2>; rel="next""#,
                Some("https://a/releases?page=2"),
            ),
            (
                r#"<https://a/releases?page=2>; rel="next", <https://a/releases?page=5>; rel="last""#,
                Some("https://a/releases?page=2"),
            ),
            (
                r#"<https://a/releases?page=1>; rel="prev", <https://a/releases?page=5>; rel="last""#,
                None,
            ),
        ];
        for (link, want) in cases {
            assert_eq!(next_page_url(link).as_deref(), want, "{link}");
        }
    }
}
