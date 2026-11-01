use std::collections::HashMap;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::changelog::Order;

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Entry {
    pub url: String,
    pub repo: String,
    pub prerelease: bool,
    pub commits: bool,
    pub heading: String,
    pub version: String,
    pub order: Order,
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

/// A versioned formula ("node@22") without its own entry uses the base
/// name's, whose templates can pick out its version. Channels
/// ("firefox@nightly") don't, as their releases are documented elsewhere.
pub fn get(name: &str) -> Entry {
    ENTRIES
        .get(name)
        .or_else(|| {
            let (base, suffix) = name.split_once('@')?;
            suffix
                .starts_with(|character: char| character.is_ascii_digit())
                .then(|| ENTRIES.get(base))?
        })
        .cloned()
        .unwrap_or_default()
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
        assert_eq!(zed.version, "{version}-pre");
        assert!(get("luajit").commits);
        assert_eq!(get("node@20"), get("node"));
        assert!(!get("python@3.12").url.is_empty());
        assert!(!get("dart-lang/dart/dart").url.is_empty());
        assert_eq!(get("_comment"), Entry::default());
        assert_eq!(get("definitely-not-a-real-package"), Entry::default());
        assert_eq!(get("definitely-not-a-real-package@2"), Entry::default());
        assert_eq!(get("firefox@nightly"), Entry::default());
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
    fn orders_other_than_newest_first_have_a_heading() {
        let missing: Vec<&String> = ENTRIES
            .iter()
            .filter(|(_, entry)| entry.order != Order::NewestFirst && entry.heading.is_empty())
            .map(|(name, _)| name)
            .collect();
        assert!(missing.is_empty(), "`order` without `heading`: {missing:?}");
        assert_eq!(get("go").order, Order::Mixed);
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
