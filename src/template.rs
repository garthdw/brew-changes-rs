//! Placeholders shared by changelog_config.json's `url`, `version` and
//! `heading`: `{version}`; `major`, `minor` and `patch` joined by one of
//! `.`, `-` or `_` (`{major.minor}`, `{major-minor-patch}`); and, in headings
//! only, `{*}` for any text. Any other `{...}` is literal, so headings like
//! calibre's `{{{ 9.16.0` need no escaping.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Literal(&'a str),
    Any,
    Whole,
    Parts { count: usize, separator: char },
}

fn tokens(template: &str) -> Vec<Token<'_>> {
    let mut tokens = vec![];
    let mut literal_start = 0;
    let mut index = 0;
    while index < template.len() {
        let placeholder = template[index..]
            .strip_prefix('{')
            .and_then(|rest| rest.split_once('}'))
            .and_then(|(name, _)| Some((placeholder(name)?, name.len() + 2)));
        match placeholder {
            Some((token, length)) => {
                if literal_start < index {
                    tokens.push(Token::Literal(&template[literal_start..index]));
                }
                tokens.push(token);
                index += length;
                literal_start = index;
            }
            None => index += template[index..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if literal_start < template.len() {
        tokens.push(Token::Literal(&template[literal_start..]));
    }
    tokens
}

fn placeholder(name: &str) -> Option<Token<'static>> {
    match name {
        "version" => return Some(Token::Whole),
        "*" => return Some(Token::Any),
        _ => {}
    }
    let separator = name
        .chars()
        .find(|character| !character.is_ascii_alphabetic());
    let parts: Vec<&str> = match separator {
        Some(separator) if ".-_".contains(separator) => name.split(separator).collect(),
        Some(_) => return None,
        None => vec![name],
    };
    let in_order = parts
        .iter()
        .zip(["major", "minor", "patch"])
        .all(|(part, expected)| *part == expected);
    (in_order && parts.len() <= 3).then_some(Token::Parts {
        count: parts.len(),
        separator: separator.unwrap_or('.'),
    })
}

fn render(token: Token<'_>, version: &str) -> String {
    match token {
        Token::Literal(text) => text.to_string(),
        Token::Any => "{*}".to_string(),
        Token::Whole => version.to_string(),
        Token::Parts { count, separator } => version
            .split('.')
            .take(count)
            .collect::<Vec<_>>()
            .join(&separator.to_string()),
    }
}

pub fn expand(template: &str, version: &str) -> String {
    tokens(template)
        .into_iter()
        .map(|token| render(token, version))
        .collect()
}

/// Matches the whole line, trimmed and without a leading ATX `#` marker, since
/// HTML pages are converted to Markdown. `None` matches any version, to find
/// where a section ends when there's no installed version to stop at.
pub fn heading_matches(template: &str, line: &str, version: Option<&str>) -> bool {
    let line = line.trim();
    let line = match line.trim_start_matches('#') {
        stripped if stripped.len() < line.len() && stripped.starts_with(' ') => {
            stripped.trim_start()
        }
        _ => line,
    };
    matches(&tokens(template), line, version)
}

fn matches(tokens: &[Token<'_>], text: &str, version: Option<&str>) -> bool {
    let Some((&token, rest)) = tokens.split_first() else {
        return text.is_empty();
    };
    match (token, version) {
        (Token::Literal(literal), _) => text
            .strip_prefix(literal)
            .is_some_and(|text| matches(rest, text, version)),
        (Token::Any, _) => text
            .char_indices()
            .map(|(index, _)| index)
            .chain([text.len()])
            .any(|index| matches(rest, &text[index..], version)),
        (_, Some(version)) => text
            .strip_prefix(render(token, version).as_str())
            .is_some_and(|after| !continues_version(after) && matches(rest, after, Some(version))),
        (_, None) => {
            if !text.starts_with(|character: char| character.is_ascii_digit()) {
                return false;
            }
            let run = text
                .find(|character: char| {
                    !character.is_ascii_alphanumeric() && !"._-".contains(character)
                })
                .unwrap_or(text.len());
            (1..=run)
                .rev()
                .any(|end| !continues_version(&text[end..]) && matches(rest, &text[end..], None))
        }
    }
}

/// "3.1" must not match the start of "3.1.2" or "3.1c".
fn continues_version(after: &str) -> bool {
    let mut characters = after.chars();
    match characters.next() {
        Some(character) if character.is_ascii_alphanumeric() => true,
        Some('.') => characters
            .next()
            .is_some_and(|character| character.is_ascii_digit()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_cases() {
        let cases = [
            ("https://x/{version}/", "1.3.1", "https://x/1.3.1/"),
            ("{major}", "1.3.1", "1"),
            ("{major.minor}", "1.3.1", "1.3"),
            ("{major-minor-patch}", "1.3.1", "1-3-1"),
            (
                "release_notes_{major_minor}.html",
                "9.18",
                "release_notes_9_18.html",
            ),
            ("{major.minor.patch}", "1.97.57.0", "1.97.57"),
            ("{major.minor.patch}", "9.18", "9.18"),
            ("{{{ {version}", "2.0", "{{{ 2.0"),
            ("{unknown} {minor}", "2.0", "{unknown} {minor}"),
            ("{major.patch}", "2.0", "{major.patch}"),
            ("no placeholders", "2.0", "no placeholders"),
        ];
        for (template, version, want) in cases {
            assert_eq!(expand(template, version), want, "{template:?} {version:?}");
        }
    }

    #[test]
    fn heading_matches_version() {
        let cases = [
            (
                "CHANGES FROM {*} TO {version}",
                "CHANGES FROM 3.7c TO 3.8",
                "3.8",
                true,
            ),
            (
                "CHANGES FROM {*} TO {version}",
                "CHANGES FROM 3.8 TO 3.8a",
                "3.8",
                false,
            ),
            (
                "Changes between {*} and {version}:",
                "Changes between 3.0.24 and 3.0.25:",
                "3.0.24",
                false,
            ),
            (
                "Changes between {*} and {version}:",
                "Changes between 3.0.23 and 3.0.24:",
                "3.0.24",
                true,
            ),
            ("{version} ({*}", "5.8.4 (2026-09-09)", "5.8.4", true),
            ("{version} ({*}", "  5.8.4 (2026-09-09)  ", "5.8.4", true),
            (
                "{version} ({*}",
                "- fixed since 5.8.4 (thanks)",
                "5.8.4",
                false,
            ),
            (
                "Version {version} {*}",
                "Version 3.1.2 was built",
                "3.1",
                false,
            ),
            ("Version {version}{*}", "Version 3.1.2", "3.1", false),
            ("Version {version}{*}", "Version 3.1 (May)", "3.1", true),
            (
                "* Noteworthy changes in release {version} {*}",
                "* Noteworthy changes in release 9.12 (2026-09-14) [stable]",
                "9.12",
                true,
            ),
            ("{{{ {version} {*}", "{{{ 9.16.0 2026-10-09", "9.16.0", true),
            ("version {version}:", "## version 9.0:", "9.0", true),
            ("v{version}", "### v2.8", "2.8", true),
        ];
        for (template, line, version, want) in cases {
            assert_eq!(
                heading_matches(template, line, Some(version)),
                want,
                "{template:?} on {line:?} for {version:?}"
            );
        }
    }

    #[test]
    fn heading_matches_any_version() {
        let cases = [
            (
                "CHANGES FROM {*} TO {version}",
                "CHANGES FROM 3.7b TO 3.7c",
                true,
            ),
            ("version {version}:", "version <next>:", false),
            (
                "* Noteworthy changes in release {version} {*}",
                "* Noteworthy changes in release ?.? (????-??-??) [?]",
                false,
            ),
            ("{version} ({*}", "5.8.3 (2026-03-31)", true),
            ("{version} ({*}", "Fixed (things)", false),
            ("go{major.minor}", "go1.23", true),
        ];
        for (template, line, want) in cases {
            assert_eq!(
                heading_matches(template, line, None),
                want,
                "{template:?} on {line:?}"
            );
        }
    }
}
