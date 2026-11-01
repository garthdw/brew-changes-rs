use url::Url;

use crate::changelog::Order;
use crate::{config, template};

/// Where the repo is hosted. `base` is the origin ("https://gitlab.gnome.org"),
/// since self-hosted GitLab instances all serve the same API.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Forge {
    #[default]
    GitHub,
    GitLab {
        base: String,
    },
    Tangled {
        base: String,
    },
}

impl Forge {
    pub fn name(&self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab { .. } => "GitLab",
            Self::Tangled { .. } => "Tangled",
        }
    }

    /// Tangled has tags but no releases.
    pub fn releases_label(&self) -> String {
        match self {
            Self::Tangled { .. } => format!("{} tags", self.name()),
            _ => format!("{} releases", self.name()),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Source {
    /// Empty when no repo is known. A GitLab owner can be a nested group
    /// ("group/subgroup").
    pub owner: String,
    pub repo: String,
    pub forge: Forge,
    pub manual_url: String,
    pub prereleases_only: bool,
    /// The repo's commits between two versions whose last component is the
    /// Unix time of their commit (LuaJIT's rolling "2.1.1793383622").
    pub commit_log: bool,
    /// A `template` for version headings; empty uses Markdown headings.
    pub heading: String,
    /// A `template` rewriting versions before matching (raycast's "2.8.1.0"
    /// is documented as "v2.8"); empty leaves them as they are.
    pub version_format: String,
    pub order: Order,
}

impl Source {
    pub fn has_repo(&self) -> bool {
        !self.owner.is_empty()
    }

    pub fn repo_url(&self) -> Option<String> {
        let base = match &self.forge {
            Forge::GitHub => "https://github.com",
            Forge::GitLab { base } | Forge::Tangled { base } => base,
        };
        self.has_repo()
            .then(|| format!("{base}/{}/{}", self.owner, self.repo))
    }

    pub fn file_url(&self, filename: &str) -> Option<String> {
        let separator = match self.forge {
            Forge::GitLab { .. } => "/-",
            _ => "",
        };
        Some(format!(
            "{}{separator}/blob/HEAD/{filename}",
            self.repo_url()?
        ))
    }

    pub fn releases_url(&self) -> Option<String> {
        let path = match self.forge {
            Forge::GitHub => "/releases",
            Forge::GitLab { .. } => "/-/releases",
            Forge::Tangled { .. } => "/tags",
        };
        Some(format!("{}{path}", self.repo_url()?))
    }
}

/// The single place a changelog source is chosen, shared by the app and the
/// validator so the validator exercises the shipped path.
/// `urls` are tried in order for a repo; the config's `repo` is on GitHub.
pub fn resolve_source(name: &str, version: &str, urls: &[&str]) -> Source {
    let entry = config::get(name);
    let (forge, owner, repo) = if !entry.repo.is_empty() {
        let (owner, repo) = entry.repo.split_once('/').unwrap_or((&entry.repo, ""));
        (Forge::GitHub, owner.to_string(), repo.to_string())
    } else if entry.url.is_empty() {
        resolve_repo(urls).unwrap_or_default()
    } else {
        Default::default()
    };
    Source {
        owner,
        repo,
        forge,
        manual_url: template::expand(&entry.url, version),
        prereleases_only: entry.prerelease,
        commit_log: entry.commits,
        heading: entry.heading,
        version_format: entry.version,
        order: entry.order,
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
    let repo_end = after_owner
        .find(['/', '?', '#'])
        .unwrap_or(after_owner.len());
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

fn resolve_repo(urls: &[&str]) -> Option<(Forge, String, String)> {
    urls.iter().find_map(|url| match parse_github_url(url) {
        Some(parsed) => Some((
            Forge::GitHub,
            parsed.owner.to_string(),
            parsed.repo.to_string(),
        )),
        None => parse_other_forge_url(url),
    })
}

/// GitLab is any `gitlab.*` host. Its project paths can nest groups, so the
/// path runs up to GitLab's `/-/` separator or a pre-separator route
/// (`/uploads/`, `/repository/`); Tangled's is always `owner/repo`.
fn parse_other_forge_url(url: &str) -> Option<(Forge, String, String)> {
    const GITLAB_ROUTES: [&str; 2] = ["uploads", "repository"];
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let base = parsed.origin().ascii_serialization();
    let path = parsed.path().trim_matches('/');
    let mut segments: Vec<&str> = path
        .split("/-/")
        .next()?
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let forge = if host.starts_with("gitlab.") {
        // Instance routes, not projects: the API and `/-/project/<id>/uploads`.
        if matches!(segments.first(), Some(&"api" | &"-")) {
            return None;
        }
        if let Some(route) = segments
            .iter()
            .position(|segment| GITLAB_ROUTES.contains(segment))
        {
            segments.truncate(route);
        }
        Forge::GitLab { base }
    } else if host == "tangled.org" || host == "tangled.sh" {
        segments.truncate(2);
        Forge::Tangled { base }
    } else {
        return None;
    };
    let (repo, owner) = segments.split_last()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!owner.is_empty() && !repo.is_empty()).then(|| (forge, owner.join("/"), repo.to_string()))
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
    fn resolve_repo_other_forges() {
        let gitlab = |base: &str, owner: &str, repo: &str| {
            Some((
                Forge::GitLab { base: base.into() },
                owner.to_string(),
                repo.to_string(),
            ))
        };
        let tangled = |owner: &str, repo: &str| {
            Some((
                Forge::Tangled {
                    base: "https://tangled.org".into(),
                },
                owner.to_string(),
                repo.to_string(),
            ))
        };
        let cases = [
            (
                "https://gitlab.com/gitlab-org/cli",
                gitlab("https://gitlab.com", "gitlab-org", "cli"),
            ),
            (
                "https://gitlab.gnome.org/GNOME/glib/-/archive/2.86.0/glib-2.86.0.tar.gz",
                gitlab("https://gitlab.gnome.org", "GNOME", "glib"),
            ),
            (
                "https://gitlab.com/group/subgroup/project.git",
                gitlab("https://gitlab.com", "group/subgroup", "project"),
            ),
            (
                "https://gitlab.com/o/p/uploads/0123abcd/p-1.0.tar.gz",
                gitlab("https://gitlab.com", "o", "p"),
            ),
            (
                "https://gitlab.com/api/v4/projects/51547804/packages/generic/meld/1.0/x.dmg",
                None,
            ),
            ("https://gitlab.com/only-a-group", None),
            (
                "https://gitlab.com/-/project/6808260/uploads/0123abcd/akku-1.0.tar.gz",
                None,
            ),
            (
                "https://tangled.org/rockorager.dev/lsr/archive/refs%2Ftags%2Fv1.0.0.tar.gz",
                tangled("rockorager.dev", "lsr"),
            ),
            (
                "https://tangled.org/tobinio.dev/cargo-features-manager.git",
                tangled("tobinio.dev", "cargo-features-manager"),
            ),
            ("https://notgitlab.com/o/p", None),
        ];
        for (url, want) in cases {
            assert_eq!(resolve_repo(&[url]), want, "{url}");
        }
    }

    #[test]
    fn repo_links_per_forge() {
        let source = |forge| Source {
            owner: "o".into(),
            repo: "r".into(),
            forge,
            ..Default::default()
        };
        let github = source(Forge::GitHub);
        assert_eq!(github.repo_url().as_deref(), Some("https://github.com/o/r"));
        assert_eq!(
            github.file_url("NEWS").as_deref(),
            Some("https://github.com/o/r/blob/HEAD/NEWS")
        );
        assert_eq!(
            github.releases_url().as_deref(),
            Some("https://github.com/o/r/releases")
        );
        let gitlab = source(Forge::GitLab {
            base: "https://gitlab.gnome.org".into(),
        });
        assert_eq!(
            gitlab.file_url("NEWS").as_deref(),
            Some("https://gitlab.gnome.org/o/r/-/blob/HEAD/NEWS")
        );
        assert_eq!(
            gitlab.releases_url().as_deref(),
            Some("https://gitlab.gnome.org/o/r/-/releases")
        );
        let tangled = source(Forge::Tangled {
            base: "https://tangled.org".into(),
        });
        assert_eq!(
            tangled.file_url("NEWS").as_deref(),
            Some("https://tangled.org/o/r/blob/HEAD/NEWS")
        );
        assert_eq!(
            tangled.releases_url().as_deref(),
            Some("https://tangled.org/o/r/tags")
        );
        assert_eq!(Source::default().repo_url(), None);
    }

    #[test]
    fn resolve_repo_cases() {
        let some =
            |owner: &str, repo: &str| Some((Forge::GitHub, owner.to_string(), repo.to_string()));
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
            ("https://github.com/o/tool#readme", "", some("o", "tool")),
            (
                "https://github.com/o/tool?tab=readme-ov-file",
                "",
                some("o", "tool"),
            ),
            ("", "", None),
        ];
        for (homepage, url, want) in cases {
            assert_eq!(
                resolve_repo(&[homepage, url]),
                want,
                "resolve_repo({homepage:?}, {url:?})"
            );
        }
        assert_eq!(
            resolve_repo(&[
                "https://nginx.org/",
                "https://nginx.org/download/nginx-1.31.7.tar.gz",
                "https://github.com/nginx/nginx.git",
            ]),
            some("nginx", "nginx")
        );
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
                    heading: "CHANGES FROM {*} TO {version}".into(),
                    ..src("tmux", "tmux")
                },
            ),
            (
                "raycast",
                "https://raycast.com/",
                Source {
                    manual_url: "https://www.raycast.com/changelog".into(),
                    heading: "[v{version}]({*}".into(),
                    version_format: "{major.minor}".into(),
                    ..Default::default()
                },
            ),
        ];
        for (package, homepage, want) in cases {
            assert_eq!(resolve_source(package, "", &[homepage]), want, "{package}");
        }
    }

    #[test]
    fn resolve_source_expands_placeholders() {
        let source = resolve_source("ansible", "12.1.0", &[]);
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
