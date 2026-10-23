use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::{Result, bail};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use syntect::highlighting::{Highlighter, ThemeSet};
use syntect::parsing::ScopeStack;
use two_face::theme::EmbeddedLazyThemeSet;

use super::keys::{config_path, load_config};

const DEFAULT_DARK: &str = "base16-ocean.dark";
const DEFAULT_LIGHT: &str = "InspiredGitHub";

/// bat's themes, via two-face. Loaded on first use; the picker lists them
/// at startup.
pub(super) static SYNTAX_THEMES: LazyLock<ThemeSet> =
    LazyLock::new(|| two_face::theme::extra().into());

/// Colours taken from a syntect theme so the list and changelog chrome match
/// its code highlighting, e.g. versions use the theme's number colour.
struct Palette {
    dark: bool,
    text: Color,
    accent: Color,
    version: Color,
    heading: Color,
    link: Color,
    code: Color,
    muted: Color,
    border: Color,
    error: Color,
    title: Color,
    title_background: Color,
}

/// The base16 themes share one layout: green strings, orange numbers, blue
/// functions, cyan support, red variables, purple keywords.
const fn base16(dark: bool, text: u32, comment: u32, background: u32, colors: [u32; 6]) -> Palette {
    let [green, orange, blue, cyan, red, purple] = colors;
    Palette {
        dark,
        text: Color::from_u32(text),
        accent: Color::from_u32(green),
        version: Color::from_u32(orange),
        heading: Color::from_u32(blue),
        link: Color::from_u32(cyan),
        code: Color::from_u32(red),
        muted: Color::from_u32(comment),
        border: Color::from_u32(comment),
        error: Color::from_u32(red),
        title: Color::from_u32(background),
        title_background: Color::from_u32(purple),
    }
}

const OCEAN: [u32; 6] = [0xa3be8c, 0xd08770, 0x8fa1b3, 0x96b5b4, 0xbf616a, 0xb48ead];

const OCEAN_DARK: Palette = base16(true, 0xc0c5ce, 0x65737e, 0x2b303b, OCEAN);

const INSPIRED_GITHUB: Palette = Palette {
    dark: false,
    text: Color::from_u32(0x323232),
    accent: Color::from_u32(0x63a35c),
    version: Color::from_u32(0x0086b3),
    heading: Color::from_u32(0x795da3),
    link: Color::from_u32(0x183691),
    code: Color::from_u32(0xa71d5d),
    muted: Color::from_u32(0x969896),
    border: Color::from_u32(0x969896),
    error: Color::from_u32(0xb52a1d),
    title: Color::from_u32(0xffffff),
    title_background: Color::from_u32(0xa71d5d),
};

const SOLARIZED: Palette = Palette {
    dark: true,
    text: Color::from_u32(0x839496),
    accent: Color::from_u32(0x859900),
    version: Color::from_u32(0xb58900),
    heading: Color::from_u32(0x268bd2),
    link: Color::from_u32(0x2aa198),
    code: Color::from_u32(0xdc322f),
    muted: Color::from_u32(0x586e75),
    border: Color::from_u32(0x586e75),
    error: Color::from_u32(0xdc322f),
    title: Color::from_u32(0xfdf6e3),
    title_background: Color::from_u32(0x6c71c4),
};

/// Hand-tuned palettes, listed first. bat's other themes get `derived` ones.
const BUILTINS: [(&str, Palette); 7] = [
    (DEFAULT_DARK, OCEAN_DARK),
    (
        "base16-eighties.dark",
        base16(
            true,
            0xd3d0c8,
            0x747369,
            0x2d2d2d,
            [0x99cc99, 0xf99157, 0x6699cc, 0x66cccc, 0xf2777a, 0xcc99cc],
        ),
    ),
    (
        "base16-mocha.dark",
        base16(
            true,
            0xd0c8c6,
            0x7e705a,
            0x3b3228,
            [0xbeb55b, 0xd28b71, 0x8ab3b5, 0x7bbda4, 0xcb6077, 0xa89bb9],
        ),
    ),
    // Its own comment colour (#a7adba) is too faint for footer text on a
    // light background, so muted text borrows the dark variant's.
    (
        "base16-ocean.light",
        Palette {
            muted: Color::from_u32(0x65737e),
            border: Color::from_u32(0xa7adba),
            ..base16(false, 0x4f5b66, 0xa7adba, 0xeff1f5, OCEAN)
        },
    ),
    (DEFAULT_LIGHT, INSPIRED_GITHUB),
    ("Solarized (dark)", SOLARIZED),
    (
        "Solarized (light)",
        Palette {
            dark: false,
            text: Color::from_u32(0x657b83),
            muted: Color::from_u32(0x93a1a1),
            border: Color::from_u32(0x93a1a1),
            ..SOLARIZED
        },
    ),
];

/// bat's terminal themes (`ansi`, `base16`, `base16-256`) put a palette index
/// in `r` with alpha 0, and mean the terminal's default colour by alpha 1.
pub(super) fn terminal_color(color: syntect::highlighting::Color) -> Color {
    match color.a {
        0 => Color::Indexed(color.r),
        1 => Color::Reset,
        _ => Color::Rgb(color.r, color.g, color.b),
    }
}

/// Follows the `base16` layout using the theme's own scope colours. Each
/// slot takes the first scope coloured differently from plain text, as
/// themes leave many scopes plain.
fn derived(theme: &syntect::highlighting::Theme) -> Palette {
    let highlighter = Highlighter::new(theme);
    let text = theme
        .settings
        .foreground
        .map_or(Color::Reset, terminal_color);
    let scope = |scopes: &[&str]| {
        scopes
            .iter()
            .filter_map(|scope| ScopeStack::from_str(scope).ok())
            .map(|stack| terminal_color(highlighter.style_for_stack(stack.as_slice()).foreground))
            .find(|color| *color != text)
            .unwrap_or(text)
    };
    let background = theme.settings.background;
    let comment = scope(&["comment"]);
    Palette {
        // Terminal themes don't know the background, and most terminals are
        // dark.
        dark: background.is_none_or(|color| {
            color.a < 2
                || u32::from(color.r) * 299 + u32::from(color.g) * 587 + u32::from(color.b) * 114
                    < 128_000
        }),
        text,
        accent: scope(&["string", "markup.inserted"]),
        version: scope(&["constant.numeric"]),
        heading: scope(&["entity.name.function", "support.function"]),
        link: scope(&["support.function", "entity.name.tag"]),
        code: scope(&["variable.parameter", "variable"]),
        muted: comment,
        border: comment,
        error: scope(&["markup.deleted", "invalid"]),
        title: background.map_or(Color::Reset, terminal_color),
        title_background: scope(&["keyword"]),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub accent: Style,
    pub border: Style,
    pub muted: Style,
    pub error: Style,
    pub version: Style,
    pub heading: Style,
    pub title: Style,
    pub code: Style,
    pub code_block: Style,
    pub link: Style,
    /// Patched onto the collapsed row under the cursor.
    pub selected: Style,
    /// The list heading and the name in the help popup.
    pub header: Style,
    /// The syntect theme for fenced code blocks.
    pub syntax: &'static str,
}

impl Theme {
    fn from_palette(syntax: &'static str, palette: &Palette) -> Self {
        let color = |color| Style::new().fg(color);
        let bold = Style::new().add_modifier(Modifier::BOLD);
        // Backgrounds stay neutral: the terminal's, not the theme's, is behind
        // the text.
        let code_background = if palette.dark { 236 } else { 254 };
        Self {
            accent: color(palette.accent),
            border: color(palette.border),
            muted: color(palette.muted),
            error: color(palette.error),
            version: color(palette.version),
            heading: color(palette.heading).patch(bold),
            title: color(palette.title)
                .bg(palette.title_background)
                .patch(bold),
            code: color(palette.code).bg(Color::Indexed(code_background)),
            code_block: color(palette.text),
            link: color(palette.link).add_modifier(Modifier::UNDERLINED),
            selected: bold,
            header: bold,
            syntax,
        }
    }

    fn builtin(name: &str) -> Option<Self> {
        if let Some((syntax, palette)) = BUILTINS.iter().find(|(builtin, _)| *builtin == name) {
            return Some(Self::from_palette(syntax, palette));
        }
        let (syntax, theme) = SYNTAX_THEMES.themes.get_key_value(name)?;
        Some(Self::from_palette(syntax, &derived(theme)))
    }

    fn is_builtin(name: &str) -> bool {
        Self::builtin_names().any(|builtin| builtin == name)
    }

    /// The hand-tuned themes in their original order, then bat's others.
    fn builtin_names() -> impl Iterator<Item = &'static str> {
        let embedded = EmbeddedLazyThemeSet::theme_names()
            .iter()
            .map(|embedded| embedded.as_name())
            .filter(|name| BUILTINS.iter().all(|(builtin, _)| builtin != name));
        BUILTINS.iter().map(|(name, _)| *name).chain(embedded)
    }

    pub fn default_dark() -> Self {
        Self::from_palette(DEFAULT_DARK, &OCEAN_DARK)
    }

    pub fn default_light() -> Self {
        Self::from_palette(DEFAULT_LIGHT, &INSPIRED_GITHUB)
    }

    fn style_mut(&mut self, key: &str) -> Option<&mut Style> {
        Some(match key {
            "accent" => &mut self.accent,
            "border" => &mut self.border,
            "muted" => &mut self.muted,
            "error" => &mut self.error,
            "version" => &mut self.version,
            "heading" => &mut self.heading,
            "title" => &mut self.title,
            "code" => &mut self.code,
            "code-block" => &mut self.code_block,
            "link" => &mut self.link,
            "selected" => &mut self.selected,
            "header" => &mut self.header,
            _ => return None,
        })
    }
}

/// A bare string is shorthand for `{"fg": ...}`.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StyleValue {
    Foreground(String),
    Full(StyleEntry),
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StyleEntry {
    fg: Option<String>,
    bg: Option<String>,
    bold: Option<bool>,
    dim: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strikethrough: Option<bool>,
    reversed: Option<bool>,
}

/// Only the attributes the entry names change, so `{"bold": false}` keeps
/// the default colour.
fn apply(mut style: Style, value: StyleValue) -> Result<Style, String> {
    let entry = match value {
        StyleValue::Foreground(fg) => StyleEntry {
            fg: Some(fg),
            ..Default::default()
        },
        StyleValue::Full(entry) => entry,
    };
    let color =
        |value: &str| Color::from_str(value).map_err(|_| format!("invalid colour {value:?}"));
    if let Some(fg) = &entry.fg {
        style = style.fg(color(fg)?);
    }
    if let Some(bg) = &entry.bg {
        style = style.bg(color(bg)?);
    }
    for (flag, modifier) in [
        (entry.bold, Modifier::BOLD),
        (entry.dim, Modifier::DIM),
        (entry.italic, Modifier::ITALIC),
        (entry.underline, Modifier::UNDERLINED),
        (entry.strikethrough, Modifier::CROSSED_OUT),
        (entry.reversed, Modifier::REVERSED),
    ] {
        style = match flag {
            Some(true) => style.add_modifier(modifier),
            Some(false) => style.remove_modifier(modifier),
            None => style,
        };
    }
    Ok(style)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(untagged)]
enum Selection {
    Fixed(String),
    ByBackground(ByBackground),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ByBackground {
    dark: Option<String>,
    light: Option<String>,
}

impl Default for Selection {
    fn default() -> Self {
        Self::ByBackground(ByBackground::default())
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    theme: Selection,
    #[serde(default)]
    themes: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeConfig {
    selection: Selection,
    themes: BTreeMap<String, Theme>,
}

impl ThemeConfig {
    /// A bad file falls back to the defaults with warnings to print rather
    /// than failing startup.
    pub fn load() -> (Self, Vec<String>) {
        match config_path("theme.json") {
            Some(path) => Self::load_from(&path),
            None => (Self::default(), vec![]),
        }
    }

    fn load_from(path: &Path) -> (Self, Vec<String>) {
        load_config(path, "theme", |data| Self::parse(data, path))
    }

    #[cfg(test)]
    fn parse_json(data: &str) -> (Self, Vec<String>) {
        Self::parse(data, Path::new("theme.json"))
    }

    /// Every configured theme is checked, selected or not, so typos surface
    /// before the theme is switched to.
    fn parse(data: &str, path: &Path) -> (Self, Vec<String>) {
        let file: File = match serde_json::from_str(data) {
            Ok(file) => file,
            Err(error) => {
                return (
                    Self::default(),
                    vec![format!(
                        "theme: could not parse {path:?} ({error}), using defaults"
                    )],
                );
            }
        };
        let mut warnings = vec![];
        let mut themes = BTreeMap::new();
        for (name, mut entries) in file.themes {
            let base = match entries.remove("syntax") {
                Some(serde_json::Value::String(syntax)) => Theme::builtin(&syntax).ok_or_else(|| {
                    format!("theme: themes.{name}.syntax {syntax:?} is not a built-in theme, ignoring {name:?}")
                }),
                Some(syntax) => Err(format!(
                    "theme: themes.{name}.syntax {syntax} is not a theme name, ignoring {name:?}"
                )),
                None => Theme::builtin(&name).ok_or_else(|| {
                    format!("theme: themes.{name} is not a built-in theme and needs a \"syntax\", ignoring it")
                }),
            };
            let mut theme = match base {
                Ok(theme) => theme,
                Err(warning) => {
                    warnings.push(warning);
                    continue;
                }
            };
            for (key, value) in entries {
                let Some(style) = theme.style_mut(&key) else {
                    warnings.push(format!(
                        "theme: themes.{name}.{key} is not a known style, ignoring"
                    ));
                    continue;
                };
                let applied = StyleValue::deserialize(&value)
                    .map_err(|_| format!("invalid style {value}"))
                    .and_then(|parsed| apply(*style, parsed));
                match applied {
                    Ok(applied) => *style = applied,
                    Err(error) => {
                        warnings.push(format!("theme: themes.{name}.{key} has {error}, ignoring"))
                    }
                }
            }
            themes.insert(name, theme);
        }
        let mut known = |name: Option<String>| {
            let name = name?;
            if themes.contains_key(&name) || Theme::is_builtin(&name) {
                return Some(name);
            }
            warnings.push(format!(
                "theme: {name:?} is not a built-in or configured theme, using the default"
            ));
            None
        };
        let selection = match file.theme {
            Selection::Fixed(name) => {
                known(Some(name)).map_or_else(Selection::default, Selection::Fixed)
            }
            Selection::ByBackground(ByBackground { dark, light }) => {
                Selection::ByBackground(ByBackground {
                    dark: known(dark),
                    light: known(light),
                })
            }
        };
        (Self { selection, themes }, warnings)
    }

    fn theme(&self, name: &str) -> Option<Theme> {
        self.themes
            .get(name)
            .copied()
            .or_else(|| Theme::builtin(name))
    }

    /// Built-ins first, with any overrides applied, then custom themes.
    pub fn themes(&self) -> Vec<(String, Theme)> {
        let custom = self
            .themes
            .keys()
            .filter(|name| !Theme::is_builtin(name))
            .cloned();
        Theme::builtin_names()
            .map(str::to_string)
            .chain(custom)
            .filter_map(|name| Some((name.clone(), self.theme(&name)?)))
            .collect()
    }

    /// Overrides the file's selection, as `--theme` does.
    pub fn select(&mut self, name: &str) -> Result<()> {
        if self.theme(name).is_none() {
            let names: Vec<String> = self.themes().into_iter().map(|(name, _)| name).collect();
            bail!(
                "unknown theme {name:?}, expected one of: {}",
                names.join(", ")
            );
        }
        self.selection = Selection::Fixed(name.into());
        Ok(())
    }

    /// Every theme and the selected one's index. `detect_dark` only runs
    /// when the theme depends on the background, as querying the terminal
    /// can stall on ones that don't answer.
    pub fn resolve(&self, detect_dark: impl FnOnce() -> bool) -> (Vec<(String, Theme)>, usize) {
        let name = match &self.selection {
            Selection::Fixed(name) => name.as_str(),
            Selection::ByBackground(ByBackground { dark, light }) => {
                if detect_dark() {
                    dark.as_deref().unwrap_or(DEFAULT_DARK)
                } else {
                    light.as_deref().unwrap_or(DEFAULT_LIGHT)
                }
            }
        };
        let themes = self.themes();
        // `parse` and `select` only keep names that exist; 0 is the default.
        let active = themes
            .iter()
            .position(|(theme, _)| theme == name)
            .unwrap_or(0);
        (themes, active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(config: &ThemeConfig, detect_dark: impl FnOnce() -> bool) -> Theme {
        let (themes, active) = config.resolve(detect_dark);
        themes[active].1
    }

    fn resolved(json: &str, dark: bool) -> (Theme, Vec<String>) {
        let (config, warnings) = ThemeConfig::parse_json(json);
        (current(&config, || dark), warnings)
    }

    #[test]
    fn defaults_follow_background() {
        assert_eq!(resolved("{}", true), (Theme::default_dark(), vec![]));
        assert_eq!(resolved("{}", false), (Theme::default_light(), vec![]));
        assert_ne!(Theme::default_dark().border, Theme::default_light().border);
    }

    #[test]
    fn builtins_use_their_own_palettes() {
        let themes = ThemeConfig::default().themes();
        let versions: std::collections::HashSet<_> = themes
            .iter()
            .take(BUILTINS.len())
            .map(|(_, theme)| theme.version)
            .collect();
        // The dark and light variants of ocean and Solarized share accents.
        assert_eq!(versions.len(), BUILTINS.len() - 2, "{themes:?}");
        let solarized = Theme::builtin("Solarized (dark)");
        assert_eq!(
            solarized.map(|theme| theme.version.fg),
            Some(Some(Color::from_u32(0xb58900)))
        );
    }

    #[test]
    fn fixed_theme_skips_detection() {
        let (config, warnings) = ThemeConfig::parse_json(r#"{"theme": "Solarized (light)"}"#);
        assert!(warnings.is_empty());
        let theme = current(&config, || panic!("detected the background"));
        assert_eq!(theme.syntax, "Solarized (light)");
        assert_eq!(theme.code.bg, Some(Color::Indexed(254)));
    }

    #[test]
    fn overrides_apply_to_selected_and_unselected_themes() {
        let json = r##"{
            "theme": {"dark": "mine"},
            "themes": {
                "mine": {"syntax": "base16-mocha.dark", "accent": "#ff79c6", "link": "39"},
                "InspiredGitHub": {"heading": "red"}
            }
        }"##;
        let (theme, warnings) = resolved(json, true);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(theme.syntax, "base16-mocha.dark");
        assert_eq!(theme.accent.fg, Some(Color::Rgb(0xFF, 0x79, 0xC6)));
        assert_eq!(
            theme.link,
            Style::new()
                .fg(Color::Indexed(39))
                .add_modifier(Modifier::UNDERLINED)
        );
        assert_eq!(
            Some(theme.border),
            Theme::builtin("base16-mocha.dark").map(|mocha| mocha.border)
        );

        let (theme, _) = resolved(json, false);
        assert_eq!(theme.syntax, DEFAULT_LIGHT);
        assert_eq!(
            theme.heading,
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn style_objects_set_colours_and_modifiers() {
        let json = r##"{"themes": {"base16-ocean.dark": {
            "title": {"fg": "white", "bg": "#000000"},
            "link": {"underline": false, "italic": true},
            "selected": {"bold": false, "reversed": true},
            "header": {"fg": "cyan", "bold": false}
        }}}"##;
        let (theme, warnings) = resolved(json, true);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            theme.title,
            Style::new()
                .fg(Color::White)
                .bg(Color::Rgb(0, 0, 0))
                .add_modifier(Modifier::BOLD)
        );
        assert_eq!(
            theme.link,
            Style::new()
                .fg(Color::from_u32(0x96b5b4))
                .add_modifier(Modifier::ITALIC)
                .remove_modifier(Modifier::UNDERLINED)
        );
        let selected_row = Style::new()
            .add_modifier(Modifier::BOLD)
            .patch(theme.selected);
        assert_eq!(selected_row.add_modifier, Modifier::REVERSED);
        assert_eq!(
            theme.header,
            Style::new().fg(Color::Cyan).remove_modifier(Modifier::BOLD)
        );
    }

    #[test]
    fn derived_palettes_match_hand_tuned_base16() {
        for name in [DEFAULT_DARK, "base16-eighties.dark", "base16-mocha.dark"] {
            let hand_tuned = BUILTINS.iter().find(|(builtin, _)| *builtin == name);
            let derived = SYNTAX_THEMES.themes.get(name).map(derived);
            assert_eq!(
                derived.map(|palette| Theme::from_palette(name, &palette)),
                hand_tuned.map(|(_, palette)| Theme::from_palette(name, palette)),
                "{name}"
            );
        }
    }

    #[test]
    fn terminal_themes_use_terminal_colours() {
        let ansi = Theme::builtin("ansi");
        assert_eq!(
            ansi.map(|theme| (theme.code_block.fg, theme.accent.fg)),
            Some((Some(Color::Reset), Some(Color::Indexed(2))))
        );
    }

    #[test]
    fn bats_themes_follow_the_hand_tuned_ones() {
        let names: Vec<String> = ThemeConfig::default()
            .themes()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names.len(), EmbeddedLazyThemeSet::theme_names().len());
        assert_eq!(
            names[..BUILTINS.len()],
            BUILTINS.map(|(name, _)| name.to_string())
        );
        assert!(names.iter().any(|name| name == "Catppuccin Mocha"));
        assert!(names.iter().any(|name| name == "Dracula"));
    }

    #[test]
    fn bad_entries_warn_and_fall_back() {
        let json = r#"{
            "theme": {"dark": "nope"},
            "themes": {
                "custom": {"accent": "red"},
                "typo": {"syntax": "base16-ocean.drak"},
                "number": {"syntax": 1},
                "base16-ocean.dark": {
                    "accnt": "red",
                    "link": "notacolour",
                    "code": {"fg": "red", "blink": true},
                    "error": 5,
                    "heading": "blue"
                }
            }
        }"#;
        let (theme, warnings) = resolved(json, true);
        assert_eq!(warnings.len(), 8, "{warnings:?}");
        assert_eq!(theme.syntax, DEFAULT_DARK);
        assert_eq!(theme.heading.fg, Some(Color::Blue));
        let default = Theme::default_dark();
        assert_eq!(
            (theme.link, theme.code, theme.error),
            (default.link, default.code, default.error)
        );
    }

    #[test]
    fn unknown_fixed_theme_falls_back_to_detection() {
        let (theme, warnings) = resolved(r#"{"theme": "nope"}"#, false);
        assert_eq!(warnings.len(), 1);
        assert_eq!(theme, Theme::default_light());
    }

    #[test]
    fn bad_file_falls_back() {
        for json in [
            "not json",
            r#"{"theme": {"drak": "x"}}"#,
            r#"{"colours": {}}"#,
        ] {
            let (theme, warnings) = resolved(json, true);
            assert_eq!(theme, Theme::default_dark(), "{json}");
            assert_eq!(warnings.len(), 1, "{json}");
        }
    }

    #[test]
    fn missing_file_is_silent() {
        let (config, warnings) =
            ThemeConfig::load_from(Path::new("/nonexistent/brew-changes/theme.json"));
        assert_eq!(config, ThemeConfig::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn themes_list_builtins_then_custom_with_overrides() {
        let (config, _) = ThemeConfig::parse_json(
            r#"{"themes": {"zed": {"syntax": "InspiredGitHub"}, "InspiredGitHub": {"accent": "red"}}}"#,
        );
        let themes = config.themes();
        let names: Vec<&str> = themes.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names.len(), EmbeddedLazyThemeSet::theme_names().len() + 1);
        assert_eq!(names.last(), Some(&"zed"));
        let inspired = themes.iter().find(|(name, _)| name == "InspiredGitHub");
        assert_eq!(
            inspired.map(|(_, theme)| theme.accent.fg),
            Some(Some(Color::Red))
        );
    }

    #[test]
    fn resolve_indexes_the_selected_name_not_an_identical_theme() {
        let (config, _) = ThemeConfig::parse_json(
            r#"{"theme": "zed", "themes": {"zed": {"syntax": "InspiredGitHub"}}}"#,
        );
        let (themes, active) = config.resolve(|| true);
        assert_eq!(themes[active].0, "zed");
    }

    #[test]
    fn select_overrides_file_and_rejects_unknown_names() {
        let (mut config, _) = ThemeConfig::parse_json(r#"{"theme": "Solarized (dark)"}"#);
        config
            .select("InspiredGitHub")
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(current(&config, || true).syntax, "InspiredGitHub");
        let error = config.select("nope").err().map(|error| error.to_string());
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("base16-ocean.dark")),
            "{error:?}"
        );
        assert_eq!(current(&config, || true).syntax, "InspiredGitHub");
    }
}
