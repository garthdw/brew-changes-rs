use crate::config;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Source {
    /// Empty when no GitHub repo is known.
    pub owner: String,
    pub repo: String,
    pub manual_url: String,
    pub lax_headings: bool,
    pub prereleases_only: bool,
    /// Non-zero shortens versions before matching (raycast's "2.8.1.0" is
    /// documented as "v2.8").
    pub version_parts: usize,
}

impl Source {
    pub fn has_repo(&self) -> bool {
        !self.owner.is_empty()
    }

    pub fn github_url(&self) -> Option<String> {
        self.has_repo()
            .then(|| format!("https://github.com/{}/{}", self.owner, self.repo))
    }
}

/// The single place a changelog source is chosen, shared by the app and the
/// validator so the validator exercises the shipped path.
pub fn resolve_source(name: &str, version: &str, homepage: &str, url: &str) -> Source {
    let entry = config::get(name);
    let (owner, repo) = if !entry.repo.is_empty() {
        let (owner, repo) = entry.repo.split_once('/').unwrap_or((&entry.repo, ""));
        (owner.to_string(), repo.to_string())
    } else if entry.url.is_empty() {
        resolve_repo(homepage, url).unwrap_or_default()
    } else {
        Default::default()
    };
    Source {
        owner,
        repo,
        manual_url: config::expand_url(&entry.url, version),
        lax_headings: entry.lax_headings,
        prereleases_only: entry.prerelease,
        version_parts: entry.version_parts,
    }
}

struct GitHubUrl<'a> {
    owner: &'a str,
    repo: &'a str,
    rest: &'a str,
    raw: bool,
}

/// Accepts github.com (https or ssh) and raw.githubusercontent.com URLs. The
/// host must be exactly github.com (optionally www.), not a subdomain like
/// docs.github.com. Repo names can contain dots ("llama.cpp"), so only a
/// final ".git" is stripped.
fn parse_github_url(url: &str) -> Option<GitHubUrl<'_>> {
    const HOSTS: [(&str, bool); 2] = [("github.com", false), ("raw.githubusercontent.com", true)];
    let (host_end, raw) = url
        .char_indices()
        .filter(|(index, _)| {
            let before = &url[..*index];
            before.is_empty()
                || before.ends_with("//")
                || before.ends_with('@')
                || before.ends_with("//www.")
        })
        .find_map(|(index, _)| {
            HOSTS.iter().find_map(|(host, raw)| {
                let after = url[index..].strip_prefix(host)?;
                let separator_ok = after.starts_with('/') || (!raw && after.starts_with(':'));
                separator_ok.then_some((index + host.len() + 1, *raw))
            })
        })?;
    let (owner, after_owner) = url[host_end..].split_once('/')?;
    let repo_end = after_owner.find('/').unwrap_or(after_owner.len());
    let (segment, rest) = after_owner.split_at(repo_end);
    let repo = segment
        .strip_suffix(".git")
        .filter(|repo| !repo.is_empty())
        .unwrap_or(segment);
    (!owner.is_empty() && !repo.is_empty()).then_some(GitHubUrl {
        owner,
        repo,
        rest,
        raw,
    })
}

fn resolve_repo(homepage: &str, url: &str) -> Option<(String, String)> {
    [homepage, url]
        .into_iter()
        .find_map(parse_github_url)
        .map(|parsed| (parsed.owner.to_string(), parsed.repo.to_string()))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct GitHubFile<'a> {
    pub owner: &'a str,
    pub repo: &'a str,
    pub git_ref: &'a str,
    pub path: &'a str,
}

/// Lets a curated file URL be fetched through the authenticated API.
pub(super) fn parse_github_file_url(url: &str) -> Option<GitHubFile<'_>> {
    let parsed = parse_github_url(url)?;
    let mut rest = parsed.rest.trim_start_matches('/');
    if !parsed.raw {
        let (kind, after) = rest.split_once('/')?;
        if kind != "blob" && kind != "raw" {
            return None;
        }
        rest = after;
    }
    let (git_ref, path) = rest.split_once('/')?;
    (!git_ref.is_empty() && !path.is_empty()).then_some(GitHubFile {
        owner: parsed.owner,
        repo: parsed.repo,
        git_ref,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repo_cases() {
        let some = |owner: &str, repo: &str| Some((owner.to_string(), repo.to_string()));
        let cases = [
            (
                "https://github.com/owner/repo",
                "https://example.com/tarball.tar.gz",
                some("owner", "repo"),
            ),
            (
                "https://example.com",
                "https://github.com/owner/repo.git",
                some("owner", "repo"),
            ),
            (
                "https://github.com/owner/repo/issues",
                "",
                some("owner", "repo"),
            ),
            ("", "git@github.com:owner/repo.git", some("owner", "repo")),
            (
                "",
                "https://github.com/ggml-org/llama.cpp.git",
                some("ggml-org", "llama.cpp"),
            ),
            ("https://example.com", "https://gitlab.com/owner/repo", None),
            (
                "https://docs.github.com/en/copilot/how-tos/copilot-cli",
                "https://github.com/github/copilot-cli/releases/download/v1.0.0/copilot.tar.gz",
                some("github", "copilot-cli"),
            ),
            (
                "https://www.github.com/owner/repo",
                "",
                some("owner", "repo"),
            ),
            (
                "",
                "https://raw.githubusercontent.com/owner/repo/main/x",
                some("owner", "repo"),
            ),
            ("", "", None),
        ];
        for (homepage, url, want) in cases {
            assert_eq!(
                resolve_repo(homepage, url),
                want,
                "resolve_repo({homepage:?}, {url:?})"
            );
        }
    }

    #[test]
    fn resolve_source_cases() {
        let src = |owner: &str, repo: &str| Source {
            owner: owner.into(),
            repo: repo.into(),
            ..Default::default()
        };
        let cases = [
            (
                "ripgrep",
                "https://github.com/BurntSushi/ripgrep",
                src("BurntSushi", "ripgrep"),
            ),
            (
                "rust",
                "https://github.com/rust-lang/rust",
                Source {
                    manual_url: "https://github.com/rust-lang/rust/blob/master/RELEASES.md".into(),
                    ..Default::default()
                },
            ),
            ("pnpm", "https://pnpm.io", src("pnpm", "pnpm")),
            (
                "zed@preview",
                "https://zed.dev",
                Source {
                    prereleases_only: true,
                    ..src("zed-industries", "zed")
                },
            ),
            (
                "tmux",
                "https://github.com/tmux/tmux",
                Source {
                    lax_headings: true,
                    ..src("tmux", "tmux")
                },
            ),
            (
                "raycast",
                "https://raycast.com/",
                Source {
                    manual_url: "https://www.raycast.com/changelog".into(),
                    lax_headings: true,
                    version_parts: 2,
                    ..Default::default()
                },
            ),
        ];
        for (package, homepage, want) in cases {
            assert_eq!(resolve_source(package, "", homepage, ""), want, "{package}");
        }
    }

    #[test]
    fn resolve_source_expands_placeholders() {
        let source = resolve_source("ansible", "12.1.0", "", "");
        assert_eq!(
            source.manual_url,
            "https://github.com/ansible-community/ansible-build-data/blob/main/12/CHANGELOG-v12.rst"
        );
    }

    #[test]
    fn parse_github_file_url_cases() {
        let file = |owner, repo, git_ref, path| {
            Some(GitHubFile {
                owner,
                repo,
                git_ref,
                path,
            })
        };
        let cases = [
            (
                "https://github.com/dart-lang/sdk/blob/main/CHANGELOG.md",
                file("dart-lang", "sdk", "main", "CHANGELOG.md"),
            ),
            (
                "https://github.com/owner/repo/raw/main/nested/dir/NEWS",
                file("owner", "repo", "main", "nested/dir/NEWS"),
            ),
            (
                "https://raw.githubusercontent.com/owner/repo/main/CHANGELOG.md",
                file("owner", "repo", "main", "CHANGELOG.md"),
            ),
            ("https://curl.se/changes.html", None),
            ("https://github.com/owner/repo/releases", None),
        ];
        for (url, want) in cases {
            assert_eq!(parse_github_file_url(url), want, "{url}");
        }
    }
}
