mod codeberg;
mod fetch;
mod forge;
mod github;
mod gitlab;
mod releases;
mod source;
mod tangled;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use htmd::HtmlToMarkdown;
use htmd::options::{BulletListMarker, HrStyle, Options};
use serde::de::DeserializeOwned;
use url::Url;

use crate::{record_timing, timing_start};

pub use fetch::{Changelog, Commit, Releases, Stage};
pub use forge::Forge;
pub use github::GitHub;
pub use gitlab::GitLab;
pub use releases::Release;
pub use source::{Source, resolve_source};

// Without a timeout a stalled connection (VPN, captive portal) hangs forever.
const TIMEOUT: Duration = Duration::from_secs(15);
const RAW_FETCH_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    retry_delay: Duration,
    github: Arc<github::GitHubApi>,
}

/// Lets a forge tell a particular status, like Forgejo's 404 for a repo
/// with releases turned off, from other failures.
#[derive(Debug)]
struct StatusError {
    url: String,
    status: u16,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GET {}: status {}", self.url, self.status)
    }
}

impl std::error::Error for StatusError {}

struct RawFetchError {
    error: anyhow::Error,
    retryable: bool,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_github(github::GitHubApi::new())
    }

    fn with_github(github: github::GitHubApi) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent("brew-changes")
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            retry_delay: Duration::from_millis(300),
            github: Arc::new(github),
        })
    }

    /// Looks up the GitHub token in the background (it may spawn `gh`), so
    /// the first fetch doesn't wait for it.
    pub fn resolve_github_token(&self) {
        let github = self.github.clone();
        tokio::spawn(async move {
            github.token().await;
        });
    }

    #[cfg(test)]
    fn for_test(base: &str) -> Self {
        Self::for_test_with_token(base, None)
    }

    #[cfg(test)]
    fn for_test_with_token(base: &str, token: Option<&str>) -> Self {
        let mut client =
            Self::with_github(github::GitHubApi::at(base).with_token(token)).expect("test client");
        client.retry_delay = Duration::from_millis(1);
        client
    }

    /// The GitHub token goes only to GitHub's API, never to another forge.
    async fn get(&self, url: &str, github_accept: &str) -> reqwest::RequestBuilder {
        let mut request = self.http.get(url);
        if self.github.owns(url) {
            request = request.header("Accept", github_accept);
            if let Some(token) = self.github.token().await {
                request = request.bearer_auth(token);
            }
        }
        request
    }

    /// Also returns the Link header's next-page URL, which GitLab sends too.
    async fn get_json_page<T: DeserializeOwned>(&self, url: &str) -> Result<(T, Option<String>)> {
        let is_github = self.github.owns(url);
        let request = self.get(url, "application/vnd.github+json").await;
        timing_start!(start);
        let response = request.send().await?;
        let status = response.status();
        record_timing!(start, "GET {url} {status} (headers)");
        let rate_limited = response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|remaining| remaining == "0");
        if is_github && rate_limited && matches!(status.as_u16(), 403 | 429) {
            bail!(
                "GET {url}: GitHub API rate limit exceeded; set GITHUB_TOKEN or run `gh auth login`"
            );
        }
        if status != reqwest::StatusCode::OK {
            return Err(StatusError {
                url: url.into(),
                status: status.as_u16(),
            }
            .into());
        }
        let next = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .and_then(releases::next_page_url);
        let body = response.bytes().await?;
        record_timing!(start, "GET {url} {} bytes", body.len());
        let parsed = serde_json::from_slice(&body).with_context(|| format!("parsing {url}"))?;
        Ok((parsed, next))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        Ok(self.get_json_page(url).await?.0)
    }

    /// Returns the text and whether it was HTML (converted to Markdown).
    /// Only network errors and 5xx are retried: raw.githubusercontent.com has
    /// been seen to 503 intermittently, while a 404 won't change.
    async fn fetch_raw(&self, url: &str) -> Result<(String, bool)> {
        let mut attempt = 1;
        loop {
            match self.fetch_raw_once(url).await {
                Ok(fetched) => return Ok(fetched),
                Err(failure) if failure.retryable && attempt < RAW_FETCH_ATTEMPTS => {
                    tokio::time::sleep(self.retry_delay).await;
                }
                Err(failure) => return Err(failure.error),
            }
            attempt += 1;
        }
    }

    async fn fetch_raw_once(
        &self,
        url: &str,
    ) -> std::result::Result<(String, bool), RawFetchError> {
        let retryable = |error: reqwest::Error| RawFetchError {
            error: error.into(),
            retryable: true,
        };
        let request = self.get(url, "application/vnd.github.raw").await;
        timing_start!(start);
        let response = request.send().await.map_err(retryable)?;
        let status = response.status();
        record_timing!(start, "GET {url} {status} (headers)");
        if status != reqwest::StatusCode::OK {
            return Err(RawFetchError {
                error: StatusError {
                    url: url.into(),
                    status: status.as_u16(),
                }
                .into(),
                retryable: status.is_server_error(),
            });
        }
        let is_html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| content_type.contains("html"));
        // Gitiles (googlesource.com) has no raw endpoint: `?format=TEXT`
        // serves the file base64-encoded, while its HTML view puts each line
        // in a table row.
        let is_gitiles_text = !is_html && response.headers().contains_key("x-gitiles-object-type");
        let body = response.text().await.map_err(retryable)?;
        record_timing!(start, "GET {url} {} bytes", body.len());
        if is_gitiles_text {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(body.trim())
                .map_err(anyhow::Error::from)
                .and_then(|bytes| Ok(String::from_utf8(bytes)?))
                .map_err(|error| RawFetchError {
                    error: error.context(format!("GET {url}: decoding gitiles text")),
                    retryable: false,
                })?;
            return Ok((decoded, false));
        }
        if !is_html {
            return Ok((body, false));
        }
        html_to_markdown(&body, url)
            .map(|markdown| (markdown, true))
            .map_err(|error| RawFetchError {
                error,
                retryable: false,
            })
    }
}

/// `<h2>` headings become ATX headings that `trim_to_range` can match.
fn html_to_markdown(html: &str, page_url: &str) -> Result<String> {
    let converter = HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            hr_style: HrStyle::Dashes,
            ..Options::default()
        })
        .skip_tags(vec!["script", "style", "head"])
        .build();
    let markdown = converter.convert(html)?;
    Ok(match Url::parse(page_url) {
        Ok(base) => absolutize_links(&markdown, &base),
        Err(_) => markdown,
    })
}

fn absolutize_links(markdown: &str, base: &Url) -> String {
    let mut output = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(start) = rest.find("](") {
        let (before, after) = rest.split_at(start + 2);
        output.push_str(before);
        let end = after
            .find(|character: char| character == ')' || character.is_whitespace())
            .unwrap_or(after.len());
        let (target, remainder) = after.split_at(end);
        match (Url::parse(target), base.join(target)) {
            (Err(_), Ok(resolved)) => output.push_str(resolved.as_str()),
            _ => output.push_str(target),
        }
        rest = remainder;
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod test_support {
    use super::Client;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(super) async fn serve(server: &MockServer, route: &str, body: impl serde::Serialize) {
        Mock::given(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    pub(super) async fn setup() -> (MockServer, Client) {
        let server = MockServer::start().await;
        let client = Client::for_test(&server.uri());
        (server, client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_raw_plain_text() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("## 1.2.3\n\nsome notes\n", "text/plain; charset=utf-8"),
            )
            .mount(&server)
            .await;
        let got = Client::for_test(&server.uri())
            .fetch_raw(&server.uri())
            .await
            .unwrap();
        assert_eq!(got, ("## 1.2.3\n\nsome notes\n".into(), false));
    }

    #[tokio::test]
    async fn fetch_raw_html_is_converted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><body><h2>1.2.3</h2></body></html>",
                "text/html; charset=utf-8",
            ))
            .mount(&server)
            .await;
        let got = Client::for_test(&server.uri())
            .fetch_raw(&server.uri())
            .await
            .unwrap();
        assert_eq!(got, ("## 1.2.3".into(), true));
    }

    #[tokio::test]
    async fn fetch_raw_gitiles_text_is_decoded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("djEuMgogIGZpeAo=", "text/plain; charset=utf-8")
                    .insert_header("x-gitiles-object-type", "blob"),
            )
            .mount(&server)
            .await;
        let got = Client::for_test(&server.uri())
            .fetch_raw(&server.uri())
            .await
            .unwrap();
        assert_eq!(got, ("v1.2\n  fix\n".into(), false));
    }

    #[tokio::test]
    async fn fetch_raw_stalled_server_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let mut client = Client::for_test(&server.uri());
        client.http = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        assert!(client.fetch_raw(&server.uri()).await.is_err());
    }

    #[tokio::test]
    async fn fetch_raw_retries_5xx_but_not_404() {
        let server = MockServer::start().await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(path("/flaky"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;
        Mock::given(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::for_test(&server.uri());
        let flaky = client.fetch_raw(&format!("{}/flaky", server.uri())).await;
        assert_eq!(flaky.unwrap().0, "ok");
        let missing = client.fetch_raw(&format!("{}/missing", server.uri())).await;
        assert!(missing.is_err());
    }

    #[tokio::test]
    async fn rate_limit_is_explained() {
        let server = MockServer::start().await;
        Mock::given(path("/limited"))
            .respond_with(ResponseTemplate::new(403).insert_header("x-ratelimit-remaining", "0"))
            .mount(&server)
            .await;
        Mock::given(path("/forbidden"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let client = Client::for_test(&server.uri());
        let error = |route: &'static str| {
            let client = client.clone();
            let url = format!("{}{route}", server.uri());
            async move {
                format!(
                    "{:#}",
                    client
                        .get_json::<serde_json::Value>(&url)
                        .await
                        .unwrap_err()
                )
            }
        };
        let limited = error("/limited").await;
        assert!(limited.contains("rate limit exceeded"), "{limited}");
        assert!(limited.contains("GITHUB_TOKEN"), "{limited}");
        let forbidden = error("/forbidden").await;
        assert!(forbidden.ends_with("status 403"), "{forbidden}");
    }

    #[tokio::test]
    async fn other_forge_rate_limit_does_not_blame_github() {
        let other_forge = MockServer::start().await;
        Mock::given(path("/limited"))
            .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-remaining", "0"))
            .mount(&other_forge)
            .await;
        let error = Client::for_test("http://127.0.0.1:9")
            .get_json::<serde_json::Value>(&format!("{}/limited", other_forge.uri()))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").ends_with("status 429"), "{error:#}");
    }

    #[test]
    fn relative_links_are_resolved() {
        let markdown = html_to_markdown(
            r##"<p><a href="/news/2.0">two</a> <a href="https://x.org/a">abs</a> <a href="#top">top</a></p>"##,
            "https://example.com/changes.html",
        )
        .unwrap();
        assert_eq!(
            markdown,
            "[two](https://example.com/news/2.0) [abs](https://x.org/a) [top](https://example.com/changes.html#top)"
        );
    }
}
