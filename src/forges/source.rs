use std::sync::Arc;

use super::codeberg::Codeberg;
use super::forge::{Forge, ForgeUrl};
use super::github::GitHub;
use super::gitlab::GitLab;
use super::tangled::Tangled;

use crate::changelog::Order;
use crate::{config, template};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Source {
    /// `None` when no repo is known.
    pub forge: Option<Arc<dyn Forge>>,
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
    #[cfg(test)]
    pub fn with_forge(forge: impl Forge + 'static) -> Self {
        Self {
            forge: Some(Arc::new(forge)),
            ..Default::default()
        }
    }

    pub fn repo_url(&self) -> Option<String> {
        self.forge.as_ref().map(|forge| forge.repo_url())
    }

    pub fn file_url(&self, filename: &str) -> Option<String> {
        self.forge.as_ref().map(|forge| forge.file_url(filename))
    }

    pub fn releases_url(&self) -> Option<String> {
        self.forge.as_ref().map(|forge| forge.releases_url())
    }

    /// Without a repo, messages fall back to GitHub's wording.
    pub fn forge_name(&self) -> &'static str {
        self.forge.as_ref().map_or("GitHub", |forge| forge.name())
    }

    pub fn releases_label(&self) -> String {
        self.forge
            .as_ref()
            .map_or_else(|| "GitHub releases".into(), |forge| forge.releases_label())
    }
}

/// The single place a changelog source is chosen, shared by the app and the
/// validator so the validator exercises the shipped path.
/// `urls` are tried in order for a repo. The config's `repo` is a repo URL,
/// or "owner/repo" on GitHub.
pub fn resolve_source(name: &str, version: &str, urls: &[&str]) -> Source {
    let entry = config::get(name);
    let forge = if entry.repo.contains("://") {
        resolve_repo(&[&entry.repo])
    } else if !entry.repo.is_empty() {
        let (owner, repo) = entry.repo.split_once('/').unwrap_or((&entry.repo, ""));
        Some(Arc::new(GitHub::new(owner, repo)) as Arc<dyn Forge>)
    } else if entry.url.is_empty() {
        resolve_repo(urls)
    } else {
        None
    };
    Source {
        forge,
        manual_url: template::expand(&entry.url, version),
        prereleases_only: entry.prerelease,
        commit_log: entry.commits,
        heading: entry.heading,
        version_format: entry.version,
        order: entry.order,
    }
}

fn resolve_repo(urls: &[&str]) -> Option<Arc<dyn Forge>> {
    urls.iter().find_map(|url| detect_forge(url))
}

/// GitHub matches the URL as text, since its ssh URLs aren't a `Url`.
fn detect_forge(url: &str) -> Option<Arc<dyn Forge>> {
    fn shared(forge: impl Forge + 'static) -> Arc<dyn Forge> {
        Arc::new(forge)
    }
    GitHub::detect(url).map(shared).or_else(|| {
        let url = ForgeUrl::parse(url)?;
        GitLab::detect(&url)
            .map(shared)
            .or_else(|| Tangled::detect(&url).map(shared))
            .or_else(|| Codeberg::detect(&url).map(shared))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repo_other_forges() {
        let gitlab = |base: &str, owner: &str, repo: &str| {
            Some(Arc::new(GitLab::new(base, owner, repo)) as Arc<dyn Forge>)
        };
        let codeberg = |owner: &str, repo: &str| {
            Some(Arc::new(Codeberg::new("https://codeberg.org", owner, repo)) as Arc<dyn Forge>)
        };
        let tangled = |owner: &str, repo: &str| {
            Some(Arc::new(Tangled::new("https://tangled.org", owner, repo)) as Arc<dyn Forge>)
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
            (
                "https://codeberg.org/dnkl/foot/releases/download/1.28.0/foot-1.28.0.tar.gz",
                codeberg("dnkl", "foot"),
            ),
            (
                "https://codeberg.org/chrisn/audiowaveform/archive/1.11.1.tar.gz",
                codeberg("chrisn", "audiowaveform"),
            ),
            (
                "https://codeberg.org/Freso/python-onion-location.git",
                codeberg("Freso", "python-onion-location"),
            ),
            ("https://codeberg.org/only-an-owner", None),
            ("https://oath-toolkit.codeberg.page/", None),
        ];
        for (url, want) in cases {
            assert_eq!(resolve_repo(&[url]), want, "{url}");
        }
    }

    #[test]
    fn repo_links_per_forge() {
        let github = Source::with_forge(GitHub::new("o", "r"));
        assert_eq!(github.repo_url().as_deref(), Some("https://github.com/o/r"));
        assert_eq!(
            github.file_url("NEWS").as_deref(),
            Some("https://github.com/o/r/blob/HEAD/NEWS")
        );
        assert_eq!(
            github.releases_url().as_deref(),
            Some("https://github.com/o/r/releases")
        );
        let gitlab = Source::with_forge(GitLab::new("https://gitlab.gnome.org", "o", "r"));
        assert_eq!(
            gitlab.file_url("NEWS").as_deref(),
            Some("https://gitlab.gnome.org/o/r/-/blob/HEAD/NEWS")
        );
        assert_eq!(
            gitlab.releases_url().as_deref(),
            Some("https://gitlab.gnome.org/o/r/-/releases")
        );
        let tangled = Source::with_forge(Tangled::new("https://tangled.org", "o", "r"));
        assert_eq!(
            tangled.file_url("NEWS").as_deref(),
            Some("https://tangled.org/o/r/blob/HEAD/NEWS")
        );
        assert_eq!(
            tangled.releases_url().as_deref(),
            Some("https://tangled.org/o/r/tags")
        );
        let codeberg = Source::with_forge(Codeberg::new("https://codeberg.org", "o", "r"));
        assert_eq!(
            codeberg.file_url("NEWS").as_deref(),
            Some("https://codeberg.org/o/r/src/branch/HEAD/NEWS")
        );
        assert_eq!(
            codeberg.releases_url().as_deref(),
            Some("https://codeberg.org/o/r/releases")
        );
        assert_eq!(Source::default().repo_url(), None);
    }

    #[test]
    fn resolve_repo_cases() {
        let some =
            |owner: &str, repo: &str| Some(Arc::new(GitHub::new(owner, repo)) as Arc<dyn Forge>);
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
        let src = |owner: &str, repo: &str| Source::with_forge(GitHub::new(owner, repo));
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
                    version_format: "{version}-pre".into(),
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
                "glib",
                "https://developer.gnome.org/glib/",
                Source::with_forge(GitLab::new("https://gitlab.gnome.org", "GNOME", "glib")),
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
    fn versioned_formula_uses_its_base_entry() {
        let source = resolve_source("python@3.12", "3.12.7", &[]);
        assert_eq!(
            source.manual_url,
            "https://docs.python.org/3.12/whatsnew/changelog.html"
        );
    }

    #[test]
    fn resolve_source_expands_placeholders() {
        let source = resolve_source("ansible", "12.1.0", &[]);
        assert_eq!(
            source.manual_url,
            "https://github.com/ansible-community/ansible-build-data/blob/main/12/CHANGELOG-v12.rst"
        );
    }
}
