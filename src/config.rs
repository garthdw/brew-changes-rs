use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Entry {
    pub url: String,
    pub repo: String,
    pub prerelease: bool,
    pub heading: String,
    pub version: String,
}

static ENTRIES: LazyLock<HashMap<String, Entry>> =
    LazyLock::new(|| parse(include_str!("changelog_config.json")));

// A malformed entry is skipped rather than losing the whole table.
fn parse(data: &str) -> HashMap<String, Entry> {
    let Ok(raw) = serde_json::from_str::<HashMap<String, serde_json::Value>>(data) else {
        return HashMap::new();
    };
    raw.into_iter()
        .filter_map(|(name, value)| Some((name, serde_json::from_value(value).ok()?)))
        .collect()
}

pub fn get(name: &str) -> Entry {
    ENTRIES.get(name).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_entries() {
        assert!(!get("rust").url.is_empty());
        assert!(!get("tmux").heading.is_empty());
        assert!(!get("brave-browser").version.is_empty());
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
    fn unknown_field_is_rejected() {
        assert!(parse(r#"{"old": {"url": "u", "lax_headings": true}}"#).is_empty());
    }

    #[test]
    fn every_embedded_entry_parses() {
        let raw: HashMap<String, serde_json::Value> =
            serde_json::from_str(include_str!("changelog_config.json")).unwrap();
        let mut missing: Vec<&String> = raw
            .keys()
            .filter(|name| *name != "_comment" && !ENTRIES.contains_key(*name))
            .collect();
        missing.sort();
        assert!(missing.is_empty(), "entries that don't parse: {missing:?}");
    }
}
