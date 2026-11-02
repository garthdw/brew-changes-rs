use anyhow::Result;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::Client;
use super::forge::Range;
use std::cmp::Ordering;

use crate::version::{compare, matches_tag, tag_prefix, tag_version};

const MAX_RELEASE_PAGES: usize = 3;
const RELEASE_CAP: usize = 10;

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

impl Release {
    /// ImageMagick's release bodies are just the version.
    pub fn lacks_notes(&self) -> bool {
        let body = self.body.trim();
        body.is_empty() || body.trim_start_matches('v') == self.tag_name.trim_start_matches('v')
    }
}

// GitHub sends `null` for an empty release body.
pub(super) fn null_as_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

impl Client {
    /// Falls back to plain tags only when there are no releases at all, for
    /// repos that just tag commits (docker/cli, vim).
    pub(super) async fn releases_or_tags<Tags: DeserializeOwned + IntoIterator>(
        &self,
        releases: Walk,
        tags_url: String,
        to_release: impl Fn(Tags::Item) -> Release,
        range: Range<'_>,
    ) -> Result<Vec<Release>> {
        if releases.saw_any {
            return Ok(releases.related(range));
        }
        Ok(self
            .fetch_pages::<Tags>(tags_url, to_release, range)
            .await?
            .releases)
    }

    pub(super) async fn fetch_pages<Page: DeserializeOwned + IntoIterator>(
        &self,
        url: String,
        to_release: impl Fn(Page::Item) -> Release,
        range: Range<'_>,
    ) -> Result<Walk> {
        let Range {
            installed,
            target,
            prereleases_only,
        } = range;
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
            let (page, next_url) = self.get_json_page::<Page>(&url).await?;
            next = next_url;
            let entries: Vec<Page::Item> = page.into_iter().collect();
            saw_any |= !entries.is_empty();
            for release in entries.into_iter().map(&to_release) {
                if prereleases_only && !release.prerelease {
                    continue;
                }
                let reached_installed = matches_tag(&release.tag_name, installed);
                if accumulate(&mut filtered, release, installed, target) {
                    return Ok(Walk {
                        releases: filtered,
                        saw_any,
                        reached_installed,
                    });
                }
            }
        }
        if !target.is_empty() {
            filtered.truncate(RELEASE_CAP);
        }
        Ok(Walk {
            releases: filtered,
            saw_any,
            reached_installed: false,
        })
    }
}

#[derive(Default)]
pub(super) struct Walk {
    pub(super) releases: Vec<Release>,
    /// Whether the endpoint returned any entries at all.
    pub(super) saw_any: bool,
    reached_installed: bool,
}

impl Walk {
    /// Without the installed or target tag the releases belong to another
    /// version line, like opencode's v1 releases for its tag-only v2.
    fn related(self, range: Range<'_>) -> Vec<Release> {
        let unrelated = !range.installed.is_empty()
            && !self.reached_installed
            && !self
                .releases
                .iter()
                .any(|release| matches_tag(&release.tag_name, range.target));
        if unrelated { Vec::new() } else { self.releases }
    }
}

/// Returns whether to stop. With an installed version set, a target match
/// must not stop the walk: releases skipped between installed and target
/// still belong in the result. Releases before the target are newer than
/// what brew installs, so they're dropped.
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
    if is_target {
        filtered.clear();
    }
    filtered.push(release);
    (installed.is_empty() && is_target) || (target.is_empty() && filtered.len() >= RELEASE_CAP)
}

/// Lists ordered by date interleave maintained branches (Forgejo's v17.0.1,
/// v16.0.7, v15.0.11, v17.0.0, v16.0.6), so releases tagged like the target
/// but versioned outside (installed, target] are dropped. Other tags, like
/// another monorepo component's, can't be compared and are kept.
pub(super) fn within_range(list: Vec<Release>, installed: &str, target: &str) -> Vec<Release> {
    let Some(prefix) = list
        .iter()
        .find_map(|release| tag_prefix(&release.tag_name, target))
        .map(str::to_string)
    else {
        return list;
    };
    list.into_iter()
        .filter(|release| {
            tag_version(&release.tag_name, &prefix).is_none_or(|version| {
                compare(version, target) != Ordering::Greater
                    && (installed.is_empty() || compare(version, installed) == Ordering::Greater)
            })
        })
        .collect()
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
    use crate::forges::forge::Forge;
    use crate::forges::github::GitHub;
    use crate::forges::test_support::{serve, setup};
    use serde_json::json;
    use wiremock::matchers::{path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    fn github() -> GitHub {
        GitHub::new("o", "r")
    }

    fn tag(name: &str) -> Release {
        Release {
            tag_name: name.into(),
            ..Default::default()
        }
    }

    fn range<'a>(installed: &'a str, target: &'a str, prereleases_only: bool) -> Range<'a> {
        Range {
            installed,
            target,
            prereleases_only,
        }
    }

    fn tags(releases: &[Release]) -> Vec<&str> {
        releases
            .iter()
            .map(|release| release.tag_name.as_str())
            .collect()
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
        let got = github()
            .fetch_releases(&client, range("1.0.0", "", false))
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
        let got = github()
            .fetch_releases(&client, range("", "25.0.0", false))
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v25.0.0"]);
    }

    #[tokio::test]
    async fn releases_newer_than_target_are_dropped() {
        let (server, client) = setup().await;
        serve(
            &server,
            "/repos/o/r/releases",
            [
                tag("v2.2.0"),
                tag("v2.1.0"),
                tag("v2.0.0"),
                tag("v1.5.0"),
                tag("v1.0.0"),
            ],
        )
        .await;
        let got = github()
            .fetch_releases(&client, range("1.0.0", "2.0.0", false))
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v2.0.0", "v1.5.0"]);
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
        let got = github()
            .fetch_releases(&client, range("0.34.0", "0.34.2", false))
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v0.34.2", "v0.34.1"]);
    }

    #[tokio::test]
    async fn target_not_found_falls_back_to_ten_without_a_second_page() {
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
        let got = github()
            .fetch_releases(&client, range("", "999.0.0", false))
            .await
            .unwrap();
        // list + latest + two by-tag lookups
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
        assert_eq!(got.len(), 10);
        assert_eq!(got[0].tag_name, "v100.0.0");
    }

    #[tokio::test]
    async fn unrelated_releases_dropped_when_neither_installed_nor_target_found() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases/latest", tag("v1.19.3")).await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("v1.19.3"), tag("v1.19.2")],
        )
        .await;
        let got = github()
            .fetch_releases(&client, range("2.0.30", "2.0.31", false))
            .await
            .unwrap();
        assert!(got.is_empty(), "{got:?}");
    }

    #[tokio::test]
    async fn partial_kept_when_installed_reached() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases/latest", tag("v2.0.32")).await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("v2.0.32"), tag("v2.0.30")],
        )
        .await;
        let got = github()
            .fetch_releases(&client, range("2.0.30", "2.0.31", false))
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v2.0.32"]);
    }

    #[tokio::test]
    async fn target_missing_from_list_but_latest_is_prepended() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases/latest", tag("v4.6.7")).await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("v4.6.6"), tag("v4.6.5")],
        )
        .await;
        let got = github()
            .fetch_releases(&client, range("4.6.5", "4.6.7", false))
            .await
            .unwrap();
        assert_eq!(tags(&got), ["v4.6.7", "v4.6.6"]);
    }

    #[tokio::test]
    async fn target_missing_from_list_but_latest_is_found() {
        let (server, client) = setup().await;
        serve(&server, "/repos/o/r/releases/latest", tag("v4.6.7")).await;
        serve(
            &server,
            "/repos/o/r/releases",
            [tag("v4.6.6"), tag("v4.6.5")],
        )
        .await;
        let got = github()
            .fetch_releases(&client, range("4.6.6", "4.6.7", false))
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
        let got = github()
            .fetch_releases(&client, range("0.5.0", "0.6.0", false))
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
        let got = github()
            .fetch_releases(&client, range("1.24.0-pre", "1.25.0-pre", true))
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
        let got = github()
            .fetch_releases(&client, range("1.8.1", "", false))
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
        let got = github()
            .fetch_releases(&client, range("1.0.0", "", false))
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
        let got = github()
            .fetch_releases(&client, range("1.0.0", "", false))
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
            github()
                .fetch_releases(&client, range("1.0.0", "", false))
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
    fn notes_missing_when_empty_or_only_the_version() {
        let release = |tag_name: &str, body: &str| Release {
            body: body.into(),
            ..tag(tag_name)
        };
        assert!(release("v1.2", "").lacks_notes());
        assert!(release("v1.2", " \r\n").lacks_notes());
        assert!(release("7.1.2-35", "7.1.2-35").lacks_notes());
        assert!(release("v1.2", "1.2\n").lacks_notes());
        assert!(!release("v1.2", "- fix a crash").lacks_notes());
    }

    #[test]
    fn within_range_cases() {
        let cases: [(&[&str], &str, &str, &[&str]); 4] = [
            (
                &["v2.0.0", "v3.0.0", "v2.0.0-rc1", "v1.0.0"],
                "1.0.0",
                "2.0.0",
                &["v2.0.0", "v2.0.0-rc1"],
            ),
            (
                &["v2.0.0", "v3.0.0", "v0.9.0"],
                "",
                "2.0.0",
                &["v2.0.0", "v0.9.0"],
            ),
            // Matched only ignoring leading zeros, so there's no prefix to compare by.
            (
                &["2026.07.04", "2027.01.01"],
                "2026.6.1",
                "2026.7.4",
                &["2026.07.04", "2027.01.01"],
            ),
            // Not found: the newest are shown as they are.
            (
                &["v3.0.0", "v2.5.0"],
                "1.0.0",
                "2.0.0",
                &["v3.0.0", "v2.5.0"],
            ),
        ];
        for (list, installed, target, want) in cases {
            let got = within_range(
                list.iter().map(|name| tag(name)).collect(),
                installed,
                target,
            );
            assert_eq!(tags(&got), want, "{list:?} {installed:?} {target:?}");
        }
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
