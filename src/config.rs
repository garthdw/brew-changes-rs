use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub url: String,
    pub repo: String,
    pub lax_headings: bool,
    pub prerelease: bool,
    pub version_parts: usize,
}

static ENTRIES: LazyLock<HashMap<String, Entry>> =
    LazyLock::new(|| parse(include_str!("changelog_config.json")));

// A malformed entry is skipped rather than losing the whole table.
fn parse(data: &str) -> HashMap<String, Entry> {
    let Ok(raw) = serde_json::from_str::<HashMap<String, serde_json::Value>>(data) else {
        return HashMap::new();
    };
    raw.into_iter()
        .filter(|(name, _)| name != "_comment")
        .filter_map(|(name, value)| Some((name, serde_json::from_value(value).ok()?)))
        .collect()
}

pub fn get(name: &str) -> Entry {
    ENTRIES.get(name).cloned().unwrap_or_default()
}

pub fn expand_url(url: &str, version: &str) -> String {
    let parts: Vec<&str> = version.split('.').collect();
    let major_minor = parts.iter().take(2).copied().collect::<Vec<_>>().join(".");
    url.replace("{version}", version)
        .replace("{major.minor}", &major_minor)
        .replace("{major}", parts.first().copied().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_entries() {
        assert!(!get("rust").url.is_empty());
        assert!(get("tmux").lax_headings);
        let zed = get("zed@preview");
        assert_eq!(zed.repo, "zed-industries/zed");
        assert!(zed.prerelease);
        assert_eq!(get("_comment"), Entry::default());
        assert_eq!(get("definitely-not-a-real-package"), Entry::default());
    }

    #[test]
    fn malformed_entry_is_skipped() {
        let parsed = parse(r#"{"good": {"url": "u"}, "bad": {"url": 1}}"#);
        assert_eq!(parsed.len(), 1);
        assert!(parse("not json").is_empty());
    }

    #[test]
    fn expand() {
        assert_eq!(
            expand_url(
                "https://example.com/v{major}/{major.minor}/notes-{version}.html",
                "9.0.2"
            ),
            "https://example.com/v9/9.0/notes-9.0.2.html"
        );
    }
}
