mod fetch;
mod releases;
mod source;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use htmd::HtmlToMarkdown;
use htmd::options::{BulletListMarker, HrStyle, Options};
use serde::de::DeserializeOwned;
use tokio::sync::OnceCell;
use url::Url;

pub use fetch::{Changelog, Releases, Stage};
pub use releases::Release;
pub use source::{Source, resolve_source};

// Without a timeout a stalled connection (VPN, captive portal) hangs forever.
const TIMEOUT: Duration = Duration::from_secs(15);
const RAW_FETCH_ATTEMPTS: usize = 3;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    api_base: String,
    raw_base: String,
    retry_delay: Duration,
    token: Arc<OnceCell<Option<String>>>,
}

struct RawFetchError {
    error: anyhow::Error,
    retryable: bool,
}

impl Client {
    pub fn new() -> Result<Self> {
        Self::with_bases(
            "https://api.github.com",
            "https://raw.githubusercontent.com",
        )
    }

    fn with_bases(api_base: &str, raw_base: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent("brew-changes")
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            api_base: api_base.into(),
            raw_base: raw_base.into(),
            retry_delay: Duration::from_millis(300),
            token: Arc::default(),
        })
    }

    #[cfg(test)]
    fn for_test(base: &str) -> Self {
        let mut client = Self::with_bases(base, &format!("{base}/raw")).expect("test client");
        client.retry_delay = Duration::from_millis(1);
        client.token = Arc::new(OnceCell::new_with(Some(None)));
        client
    }

    /// Resolved once because it may spawn `gh`.
    async fn token(&self) -> Option<&str> {
        self.token
            .get_or_init(|| async {
                for variable in ["GITHUB_TOKEN", "GH_TOKEN"] {
                    if let Ok(value) = std::env::var(variable)
                        && !value.is_empty()
                    {
                        return Some(value);
                    }
                }
                // No `gh` on PATH, or not logged in, means unauthenticated requests.
                let output = tokio::process::Command::new("gh")
                    .args(["auth", "token"])
                    .output()
                    .await
                    .ok()?;
                let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
                (output.status.success() && !token.is_empty()).then_some(token)
            })
            .await
            .as_deref()
    }

    /// Also returns the Link header's next-page URL.
    async fn get_json_page<T: DeserializeOwned>(&self, url: &str) -> Result<(T, Option<String>)> {
        let mut request = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json");
        if let Some(token) = self.token().await {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            bail!("GET {url}: status {}", status.as_u16());
        }
        let next = response
            .headers()
            .get(reqwest::header::LINK)
            .and_then(|value| value.to_str().ok())
            .and_then(releases::next_page_url);
        let body = response.bytes().await?;
        let parsed = serde_json::from_slice(&body).with_context(|| format!("parsing {url}"))?;
        Ok((parsed, next))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        Ok(self.get_json_page(url).await?.0)
    }

    fn repo_url(&self, owner: &str, repo: &str, path: &str) -> String {
        format!("{}/repos/{owner}/{repo}/{path}", self.api_base)
    }

    /// Falls back to raw content because the Contents API returns no content
    /// for files over 1 MB (aws-cli's CHANGELOG.rst).
    async fn fetch_github_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: Option<&str>,
    ) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Contents {
            #[serde(default)]
            content: String,
        }
        let mut url = self.repo_url(owner, repo, &format!("contents/{path}"));
        if let Some(git_ref) = git_ref {
            url += &format!("?ref={git_ref}");
        }
        if let Ok(contents) = self.get_json::<Contents>(&url).await
            && let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(contents.content.replace('\n', ""))
            && !decoded.is_empty()
        {
            return Ok(String::from_utf8_lossy(&decoded).into_owned());
        }
        let raw_url = format!(
            "{}/{owner}/{repo}/{}/{path}",
            self.raw_base,
            git_ref.unwrap_or("HEAD")
        );
        Ok(self.fetch_raw(&raw_url).await?.0)
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
        let response = self.http.get(url).send().await.map_err(retryable)?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            return Err(RawFetchError {
                error: anyhow!("GET {url}: status {}", status.as_u16()),
                retryable: status.is_server_error(),
            });
        }
        let is_html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| content_type.contains("html"));
        let body = response.text().await.map_err(retryable)?;
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
