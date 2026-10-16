use crate::{template, version};

pub const MAX_CONTENT_LINES: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub struct Section {
    pub text: String,
    /// False when `new_version`'s heading can't be located (e.g. Node.js's
    /// index-style changelog), so callers can try another source; `text` is
    /// then the first `MAX_CONTENT_LINES` as a last resort.
    pub found: bool,
    /// The heading that ended the section, or `None` when it was truncated.
    pub end: Option<String>,
}

/// `heading` is a `template` for changelogs whose version headings aren't
/// Markdown headings (tmux's CHANGES) or mention other versions (vlc's
/// "Changes between 3.0.23 and 3.0.24:"). With a template and nothing
/// installed, the next release's heading ends the section.
pub fn trim_to_range(content: &str, new_version: &str, installed: &str, heading: &str) -> Section {
    let lines: Vec<&str> = content.split('\n').collect();

    let Some(start) = find_heading(&lines, Some(new_version), heading) else {
        let head = lines[..MAX_CONTENT_LINES.min(lines.len())].join("\n");
        return Section {
            text: format!(
                "{head}\n\n... (couldn't locate version headings, showing first {MAX_CONTENT_LINES} lines)"
            ),
            found: false,
            end: None,
        };
    };

    // Search only past start, since the new-version heading can itself mention
    // the installed version (auto-changelog's compare links).
    let after = &lines[start + 1..];
    let installed = if installed.is_empty() && !heading.is_empty() {
        None
    } else {
        Some(installed)
    };
    if let Some(end) = find_heading(after, installed, heading) {
        return Section {
            text: lines[start..=start + end].join("\n"),
            found: true,
            end: after.get(end).map(|line| line.to_string()),
        };
    }

    let end = start + MAX_CONTENT_LINES;
    let mut text = lines[start..end.min(lines.len())].join("\n");
    if end < lines.len() {
        text += &format!(
            "\n\n... (truncated, showing {MAX_CONTENT_LINES} lines from the {new_version} heading)"
        );
    }
    Section {
        text,
        found: true,
        end: None,
    }
}

/// `None` matches any version, only with a `heading` template. Without one,
/// each of `version::line_matchers` is tried over all lines before the next.
fn find_heading(lines: &[&str], version: Option<&str>, heading: &str) -> Option<usize> {
    if version.is_some_and(str::is_empty) {
        return None;
    }
    if !heading.is_empty() {
        return lines
            .iter()
            .position(|line| template::heading_matches(heading, line, version));
    }
    let version = version?;
    version::line_matchers(version)
        .into_iter()
        .find_map(|(needle, allow_longer)| {
            lines.iter().enumerate().position(|(index, line)| {
                let is_atx = line.starts_with('#');
                let is_setext = !line.trim().is_empty()
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| is_setext_underline(next));
                (is_atx || is_setext) && version::in_line(line, needle, allow_longer)
            })
        })
}

fn is_setext_underline(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().next().is_some_and(|first| {
        (first == '=' || first == '-') && trimmed.chars().all(|character| character == first)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn trim(content: &str, new_version: &str, installed: &str) -> (String, bool) {
        let section = trim_to_range(content, new_version, installed, "");
        (section.text, section.found)
    }

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn between_headings() {
        let content = "# Changelog\n\n## 2.0.0\nline a\nline b\n## 1.0.0\nline c";
        assert_eq!(
            trim(content, "2.0.0", "1.0.0"),
            ("## 2.0.0\nline a\nline b".into(), true)
        );
    }

    #[test]
    fn installed_heading_missing() {
        let content = format!("## 2.0.0\n{}", numbered(150).join("\n"));
        let (got, found) = trim(&content, "2.0.0", "1.0.0");
        assert!(found);
        assert!(got.starts_with("## 2.0.0\nline 0"));
        assert!(got.contains("truncated, showing 100 lines from the 2.0.0 heading"));
        assert_eq!(got.split('\n').count(), MAX_CONTENT_LINES + 2);
    }

    #[test]
    fn new_version_heading_missing() {
        let (got, found) = trim(&numbered(150).join("\n"), "9.9.9", "1.0.0");
        assert!(!found);
        assert!(got.contains("couldn't locate version headings, showing first 100 lines"));
        assert!(got.starts_with("line 0\n"));
    }

    #[test]
    fn installed_heading_before_new_version_is_ignored() {
        let content = "## 1.0.0\nold stuff\n## 2.0.0\nnew stuff";
        assert_eq!(
            trim(content, "2.0.0", "1.0.0"),
            ("## 2.0.0\nnew stuff".into(), true)
        );
    }

    #[test]
    fn empty_installed_version() {
        assert_eq!(
            trim("## 2.0.0\nline a", "2.0.0", ""),
            ("## 2.0.0\nline a".into(), true)
        );
    }

    #[test]
    fn find_heading_ignores_non_heading_lines() {
        let lines = [
            "See 2.0.0 in the body text, not a heading",
            "## 2.0.0 actual heading",
        ];
        assert_eq!(find_heading(&lines, Some("2.0.0"), ""), Some(1));
    }

    #[test]
    fn find_heading_not_found() {
        assert_eq!(
            find_heading(&["## 1.0.0", "## 3.0.0"], Some("2.0.0"), ""),
            None
        );
    }

    #[test]
    fn find_heading_setext() {
        let lines = ["2.0.0", "=====", "body", "1.0.0", "-----"];
        assert_eq!(find_heading(&lines, Some("2.0.0"), ""), Some(0));
        assert_eq!(find_heading(&lines, Some("1.0.0"), ""), Some(3));
    }

    #[test]
    fn template_skips_next_release_heading_mentioning_version() {
        let content = [
            "Changes between 3.0.24 and 3.0.25:",
            "newer",
            "Changes between 3.0.23 and 3.0.24:",
            "line a",
            "- regression from 3.0.23 fixed",
            "Changes between 3.0.22 and 3.0.23:",
            "line b",
        ]
        .join("\n");
        let section = trim_to_range(
            &content,
            "3.0.24",
            "3.0.23",
            "Changes between {*} and {version}:",
        );
        assert_eq!(
            section,
            Section {
                text: "Changes between 3.0.23 and 3.0.24:\nline a\n- regression from 3.0.23 fixed"
                    .into(),
                found: true,
                end: Some("Changes between 3.0.22 and 3.0.23:".into()),
            }
        );
    }

    #[test]
    fn template_without_installed_ends_at_next_release() {
        let content = "CHANGES FROM 3.7c TO 3.8\n* new\nCHANGES FROM 3.7b TO 3.7c\n* old";
        let section = trim_to_range(content, "3.8", "", "CHANGES FROM {*} TO {version}");
        assert_eq!(section.text, "CHANGES FROM 3.7c TO 3.8\n* new");
        assert_eq!(section.end.as_deref(), Some("CHANGES FROM 3.7b TO 3.7c"));
    }

    #[test]
    fn template_ignores_markdown_headings() {
        let content = "## 2.0.0\nVersion 2.0.0 (2026)\nnew\nVersion 1.0.0 (2025)\nold";
        let section = trim_to_range(content, "2.0.0", "1.0.0", "Version {version} ({*}");
        assert_eq!(section.text, "Version 2.0.0 (2026)\nnew");
    }

    #[test]
    fn end_heading_reported() {
        let section = trim_to_range("## 2.0.0\nnew\n## 1.0.0\nold", "2.0.0", "1.0.0", "");
        assert_eq!(section.end.as_deref(), Some("## 1.0.0"));
        let section = trim_to_range("## 2.0.0\nnew", "2.0.0", "1.0.0", "");
        assert_eq!(section.end, None);
    }

    #[test]
    fn trailing_zero_omitted_in_headings() {
        let content = "## GCC 16.2\nfixes\n## GCC 16.1\nolder\n";
        assert_eq!(
            trim(content, "16.2.0", "16.1.0"),
            ("## GCC 16.2\nfixes".into(), true)
        );
    }

    #[test]
    fn two_part_version_not_shortened_to_major() {
        assert_eq!(
            find_heading(&["## 1 big release", "## 1.0"], Some("1.0"), ""),
            Some(1)
        );
        assert_eq!(find_heading(&["## 1 big release"], Some("1.0"), ""), None);
    }

    #[test]
    fn trailing_zero_retry_does_not_match_longer_version() {
        let content = "## 2.6.0\nnewest\n## 2.5.1\npatch\n## 2.5\nolder\n";
        assert_eq!(
            trim(content, "2.6.0", "2.5.0"),
            ("## 2.6.0\nnewest\n## 2.5.1\npatch".into(), true)
        );
    }

    #[test]
    fn installed_heading_prefers_exact_match_over_patch() {
        let content = "## 1.8\nnewest\n## 1.7.1\npatch\n## 1.7\nolder\n";
        assert_eq!(
            trim(content, "1.8", "1.7"),
            ("## 1.8\nnewest\n## 1.7.1\npatch".into(), true)
        );
    }

    #[test]
    fn new_heading_mentions_installed_version() {
        let content = [
            "#### [0.51.0](https://github.com/ast-grep/ast-grep/compare/0.50.0...0.51.0)",
            "- new",
            "#### [0.50.0](https://github.com/ast-grep/ast-grep/compare/0.45.3...0.50.0)",
            "- old",
        ]
        .join("\n");
        assert_eq!(
            trim(&content, "0.51.0", "0.50.0"),
            (
                "#### [0.51.0](https://github.com/ast-grep/ast-grep/compare/0.50.0...0.51.0)\n- new".into(),
                true
            )
        );
    }
}
