use crate::version;

pub const MAX_CONTENT_LINES: usize = 100;

/// The bool is false when `new_version`'s heading can't be located (e.g.
/// Node.js's index-style changelog), so callers can try another source; the
/// first `MAX_CONTENT_LINES` are still returned as a last resort.
///
/// `lax` accepts any non-blank line as a heading, for files like tmux's
/// CHANGES that mark sections with plain lines.
pub fn trim_to_range(
    content: &str,
    new_version: &str,
    installed: &str,
    lax: bool,
) -> (String, bool) {
    let lines: Vec<&str> = content.split('\n').collect();

    let Some(start) = find_heading(&lines, new_version, lax) else {
        let head = lines[..MAX_CONTENT_LINES.min(lines.len())].join("\n");
        return (
            format!(
                "{head}\n\n... (couldn't locate version headings, showing first {MAX_CONTENT_LINES} lines)"
            ),
            false,
        );
    };

    // Search only past start, since the new-version heading can itself mention
    // the installed version (auto-changelog's compare links).
    if let Some(end) = find_heading(&lines[start + 1..], installed, lax) {
        return (lines[start..start + 1 + end].join("\n"), true);
    }

    let end = start + MAX_CONTENT_LINES;
    let mut result = lines[start..end.min(lines.len())].join("\n");
    if end < lines.len() {
        result += &format!(
            "\n\n... (truncated, showing {MAX_CONTENT_LINES} lines from the {new_version} heading)"
        );
    }
    (result, true)
}

/// Each of `version::line_matchers` is tried over all lines before the next.
fn find_heading(lines: &[&str], version: &str, lax: bool) -> Option<usize> {
    if version.is_empty() {
        return None;
    }
    version::line_matchers(version)
        .into_iter()
        .find_map(|(needle, allow_longer)| {
            lines.iter().enumerate().position(|(index, line)| {
                let non_blank = !line.trim().is_empty();
                let is_atx = line.starts_with('#');
                let is_setext = non_blank
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| is_setext_underline(next));
                (is_atx || is_setext || (lax && non_blank))
                    && version::in_line(line, needle, allow_longer)
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

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn between_headings() {
        let content = "# Changelog\n\n## 2.0.0\nline a\nline b\n## 1.0.0\nline c";
        assert_eq!(
            trim_to_range(content, "2.0.0", "1.0.0", false),
            ("## 2.0.0\nline a\nline b".into(), true)
        );
    }

    #[test]
    fn installed_heading_missing() {
        let content = format!("## 2.0.0\n{}", numbered(150).join("\n"));
        let (got, found) = trim_to_range(&content, "2.0.0", "1.0.0", false);
        assert!(found);
        assert!(got.starts_with("## 2.0.0\nline 0"));
        assert!(got.contains("truncated, showing 100 lines from the 2.0.0 heading"));
        assert_eq!(got.split('\n').count(), MAX_CONTENT_LINES + 2);
    }

    #[test]
    fn new_version_heading_missing() {
        let (got, found) = trim_to_range(&numbered(150).join("\n"), "9.9.9", "1.0.0", false);
        assert!(!found);
        assert!(got.contains("couldn't locate version headings, showing first 100 lines"));
        assert!(got.starts_with("line 0\n"));
    }

    #[test]
    fn installed_heading_before_new_version_is_ignored() {
        let content = "## 1.0.0\nold stuff\n## 2.0.0\nnew stuff";
        assert_eq!(
            trim_to_range(content, "2.0.0", "1.0.0", false),
            ("## 2.0.0\nnew stuff".into(), true)
        );
    }

    #[test]
    fn empty_installed_version() {
        assert_eq!(
            trim_to_range("## 2.0.0\nline a", "2.0.0", "", false),
            ("## 2.0.0\nline a".into(), true)
        );
    }

    #[test]
    fn find_heading_ignores_non_heading_lines() {
        let lines = [
            "See 2.0.0 in the body text, not a heading",
            "## 2.0.0 actual heading",
        ];
        assert_eq!(find_heading(&lines, "2.0.0", false), Some(1));
    }

    #[test]
    fn find_heading_not_found() {
        assert_eq!(
            find_heading(&["## 1.0.0", "## 3.0.0"], "2.0.0", false),
            None
        );
    }

    #[test]
    fn find_heading_setext() {
        let lines = ["2.0.0", "=====", "body", "1.0.0", "-----"];
        assert_eq!(find_heading(&lines, "2.0.0", false), Some(0));
        assert_eq!(find_heading(&lines, "1.0.0", false), Some(3));
    }

    #[test]
    fn lax_matches_unmarked_line() {
        let lines = ["CHANGES FROM 1.0.0 TO 2.0.0", "* did a thing"];
        assert_eq!(find_heading(&lines, "2.0.0", false), None);
        assert_eq!(find_heading(&lines, "2.0.0", true), Some(0));
    }

    #[test]
    fn lax_ignores_blank_lines() {
        assert_eq!(
            find_heading(&["", "  ", "2.0.0 (2026-01-01)"], "2.0.0", true),
            Some(2)
        );
    }

    #[test]
    fn lax_between_unmarked_headings() {
        let content = "2.0.0 (2026-01-01)\nline a\n1.0.0 (2025-01-01)\nline b";
        assert_eq!(
            trim_to_range(content, "2.0.0", "1.0.0", true),
            ("2.0.0 (2026-01-01)\nline a".into(), true)
        );
    }

    #[test]
    fn trailing_zero_omitted_in_headings() {
        let content = "## GCC 16.2\nfixes\n## GCC 16.1\nolder\n";
        assert_eq!(
            trim_to_range(content, "16.2.0", "16.1.0", false),
            ("## GCC 16.2\nfixes".into(), true)
        );
    }

    #[test]
    fn two_part_version_not_shortened_to_major() {
        assert_eq!(
            find_heading(&["## 1 big release", "## 1.0"], "1.0", false),
            Some(1)
        );
        assert_eq!(find_heading(&["## 1 big release"], "1.0", false), None);
    }

    #[test]
    fn trailing_zero_retry_does_not_match_longer_version() {
        let content = "## 2.6.0\nnewest\n## 2.5.1\npatch\n## 2.5\nolder\n";
        assert_eq!(
            trim_to_range(content, "2.6.0", "2.5.0", false),
            ("## 2.6.0\nnewest\n## 2.5.1\npatch".into(), true)
        );
    }

    #[test]
    fn installed_heading_prefers_exact_match_over_patch() {
        let content = "## 1.8\nnewest\n## 1.7.1\npatch\n## 1.7\nolder\n";
        assert_eq!(
            trim_to_range(content, "1.8", "1.7", false),
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
            trim_to_range(&content, "0.51.0", "0.50.0", false),
            (
                "#### [0.51.0](https://github.com/ast-grep/ast-grep/compare/0.50.0...0.51.0)\n- new".into(),
                true
            )
        );
    }
}
