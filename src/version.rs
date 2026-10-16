// A trailing "." is allowed after a match ("3.1" inside "3.1.0") because
// brew's version is sometimes less precise than a changelog heading.
pub fn in_line(line: &str, version: &str) -> bool {
    in_line_with(line, version, true)
}

fn in_line_with(line: &str, version: &str, allow_longer: bool) -> bool {
    if version.is_empty() {
        return false;
    }
    let bytes = line.as_bytes();
    // Overlapping search: a rejected match can overlap a valid one.
    (0..bytes.len())
        .filter(|&start| bytes[start..].starts_with(version.as_bytes()))
        .any(|start| {
            let end = start + version.len();
            let before = start.checked_sub(1).and_then(|index| bytes.get(index));
            let after = bytes.get(end);
            let continues_longer =
                after == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
            !before.is_some_and(|byte| is_version_char(*byte))
                && !after.is_some_and(u8::is_ascii_digit)
                && (allow_longer || !continues_longer)
        })
}

pub type LineMatcher<'a> = Box<dyn Fn(&str) -> bool + 'a>;

/// Tried in order. The second matcher drops a trailing ".0" from three-plus
/// part versions because some projects omit it (gcc's "GCC 16.2", git's
/// "Git v2.56"); it must match exactly so "2.5" (for "2.5.0") doesn't match
/// "2.5.1".
pub fn line_matchers(version: &str) -> Vec<LineMatcher<'_>> {
    let mut matchers: Vec<LineMatcher> = vec![Box::new(move |line| in_line(line, version))];
    if let Some(short) = version
        .strip_suffix(".0")
        .filter(|short| short.contains('.'))
    {
        matchers.push(Box::new(move |line| in_line_with(line, short, false)));
    }
    matchers
}

/// Matches whatever prefix a project puts on its tags ("v1.8.1", "jq-1.8.1",
/// "2024-edition-v1.8.1"). The character before the version must not continue
/// a version number, or "2.3" would match "1.2.3". "-pre" is stripped for
/// preview channels like zed@preview.
pub fn matches_tag(tag: &str, version: &str) -> bool {
    if version.is_empty() {
        return false;
    }
    let tag = tag.strip_suffix("-pre").unwrap_or(tag);
    exact_suffix_match(tag, version) || trailing_components_match(tag, version)
}

fn exact_suffix_match(tag: &str, version: &str) -> bool {
    let Some(prefix) = tag.strip_suffix(version) else {
        return false;
    };
    !prefix.bytes().last().is_some_and(is_version_char)
}

/// yt-dlp tags "2026.07.04" while brew reports "2026.7.4", so components are
/// compared ignoring leading zeros.
fn trailing_components_match(tag: &str, version: &str) -> bool {
    let start = tag
        .bytes()
        .rposition(|byte| !is_version_char(byte))
        .map_or(0, |index| index + 1);
    let run = &tag[start..];
    let tag_parts: Vec<&str> = run.split('.').collect();
    let version_parts: Vec<&str> = version.split('.').collect();
    let all_numeric = tag_parts
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    all_numeric
        && tag_parts.len() == version_parts.len()
        && tag_parts
            .iter()
            .zip(&version_parts)
            .all(|(tag_part, version_part)| {
                without_leading_zeros(tag_part) == without_leading_zeros(version_part)
            })
}

fn without_leading_zeros(component: &str) -> &str {
    match component.trim_start_matches('0') {
        "" => "0",
        trimmed => trimmed,
    }
}

fn is_version_char(byte: u8) -> bool {
    byte.is_ascii_digit() || byte == b'.'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_tag_cases() {
        let cases = [
            ("1.8.1", "1.8.1", true),
            ("v1.8.1", "1.8.1", true),
            ("jq-1.8.1", "1.8.1", true),
            ("2024-edition-v1.8.1", "1.8.1", true),
            ("1.2.3", "2.3", false),
            ("12.3", "2.3", false),
            ("v1.8.2", "1.8.1", false),
            ("", "1.8.1", false),
            ("v1.8.1", "", false),
            ("2026.07.04", "2026.7.4", true),
            ("v1.20.0-pre", "1.20.0", true),
            ("x.1.2", "1.2", false),
        ];
        for (tag, version, want) in cases {
            assert_eq!(
                matches_tag(tag, version),
                want,
                "matches_tag({tag:?}, {version:?})"
            );
        }
    }

    #[test]
    fn in_line_cases() {
        let cases = [
            ("## 3.1 (2026-01-01)", "3.1", true),
            ("## 3.1.0", "3.1", true),
            ("## 3.10.2", "3.1", false),
            ("## 13.1", "3.1", false),
            ("## v3.1", "3.1", true),
        ];
        for (line, version, want) in cases {
            assert_eq!(
                in_line(line, version),
                want,
                "in_line({line:?}, {version:?})"
            );
        }
    }

    #[test]
    fn line_matchers_cases() {
        let cases: [(&str, &str, &[bool]); 5] = [
            ("16.2.0", "## GCC 16.2", &[false, true]),
            ("16.2.0", "## GCC 16.2.1", &[false, false]),
            ("16.2.0", "## 16.2.0", &[true, false]),
            ("1.0", "## 1 big release", &[false]),
            ("1.2.3", "## 1.2.3", &[true]),
        ];
        for (version, line, want) in cases {
            let got: Vec<bool> = line_matchers(version)
                .iter()
                .map(|matcher| matcher(line))
                .collect();
            assert_eq!(got, want, "line_matchers({version:?}) on {line:?}");
        }
    }
}
