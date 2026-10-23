use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    MoveUp,
    MoveDown,
    MoveUpOrScrollUp,
    MoveDownOrScrollDown,
    ToggleExpand,
    OpenHomepage,
    OpenGithub,
    OpenChangelog,
    UpgradeAll,
    UpgradeCurrent,
    PickTheme,
    Help,
    Quit,
}

const DEFAULTS: [(&str, Action); 16] = [
    ("up", Action::MoveUp),
    ("K", Action::MoveUp),
    ("down", Action::MoveDown),
    ("J", Action::MoveDown),
    ("k", Action::MoveUpOrScrollUp),
    ("j", Action::MoveDownOrScrollDown),
    ("enter", Action::ToggleExpand),
    ("o", Action::OpenHomepage),
    ("g", Action::OpenGithub),
    ("c", Action::OpenChangelog),
    ("a", Action::UpgradeAll),
    ("u", Action::UpgradeCurrent),
    ("t", Action::PickTheme),
    ("?", Action::Help),
    ("q", Action::Quit),
    ("ctrl+c", Action::Quit),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap(BTreeMap<String, Action>);

impl Default for Keymap {
    fn default() -> Self {
        Self(
            DEFAULTS
                .iter()
                .map(|(key, action)| (key.to_string(), *action))
                .collect(),
        )
    }
}

impl Keymap {
    pub fn lookup(&self, key: &str) -> Option<Action> {
        self.0.get(key).copied()
    }

    /// Sorted, since the map is ordered.
    pub fn keys_for(&self, actions: &[Action]) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(_, action)| actions.contains(action))
            .map(|(key, _)| key.as_str())
            .collect()
    }

    /// Prefers the default key while it's still bound, so "[q] quit" isn't
    /// replaced by "[ctrl+c] quit" just because it sorts first.
    pub fn hint_key(&self, action: Action) -> Option<&str> {
        DEFAULTS
            .iter()
            .find(|(key, default_action)| {
                *default_action == action && self.lookup(key) == Some(action)
            })
            .map(|(key, _)| *key)
            .or_else(|| self.keys_for(&[action]).first().copied())
    }

    /// A bad file falls back to the defaults with warnings to print rather
    /// than failing startup.
    pub fn load() -> (Self, Vec<String>) {
        match config_path("keymap.json") {
            Some(path) => Self::load_from(&path),
            None => (Self::default(), vec![]),
        }
    }

    fn load_from(path: &Path) -> (Self, Vec<String>) {
        load_config(path, "keymap", |data| Self::default().merge(data, path))
    }

    #[cfg(test)]
    pub fn merge_json(self, data: &str) -> (Self, Vec<String>) {
        self.merge(data, Path::new("keymap.json"))
    }

    fn merge(mut self, data: &str, path: &Path) -> (Self, Vec<String>) {
        let parsed: HashMap<String, HashMap<String, Option<String>>> =
            match serde_json::from_str(data) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return (
                        self,
                        vec![format!(
                            "keymap: could not parse {path:?} ({error}), using defaults"
                        )],
                    );
                }
            };
        let mut warnings = vec![];
        for (context, bindings) in parsed {
            if context != "main" {
                warnings.push(format!(
                    "keymap: {context:?} is not a known context, ignoring"
                ));
                continue;
            }
            for (key, action) in bindings {
                let Some(name) = action else {
                    self.0.remove(&key);
                    continue;
                };
                match serde_json::from_value(name.as_str().into()) {
                    Ok(action) => {
                        self.0.insert(key, action);
                    }
                    Err(_) => warnings.push(format!(
                        "keymap: main.{key} has unknown action {name:?}, ignoring"
                    )),
                }
            }
        }
        (self, warnings)
    }
}

/// `$XDG_CONFIG_HOME` wins over the platform directory
/// (`~/Library/Application Support` on macOS) so XDG users on macOS get it.
pub(super) fn config_path(file: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)?;
    Some(base.join("brew-changes").join(file))
}

/// A missing file is silent; an unreadable one warns. Either way the
/// defaults are used.
pub(super) fn load_config<T: Default>(
    path: &Path,
    label: &str,
    parse: impl FnOnce(&str) -> (T, Vec<String>),
) -> (T, Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(data) => parse(&data),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (T::default(), vec![]),
        Err(error) => (
            T::default(),
            vec![format!(
                "{label}: could not read {path:?} ({error}), using defaults"
            )],
        ),
    }
}

/// Names follow the terminal convention used in keymap.json: "j", "K",
/// "ctrl+c", "enter".
pub fn key_name(event: &KeyEvent) -> String {
    let base = match event.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pgup".into(),
        KeyCode::PageDown => "pgdown".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => return String::new(),
    };
    let mut name = String::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        name += "ctrl+";
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        name += "alt+";
    }
    // Shift is already reflected in a char's case.
    if event.modifiers.contains(KeyModifiers::SHIFT)
        && !matches!(event.code, KeyCode::Char(_) | KeyCode::BackTab)
    {
        name += "shift+";
    }
    name + &base
}

pub fn glyph(key: &str) -> &str {
    match key {
        "up" => "↑",
        "down" => "↓",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(json: &str) -> (Keymap, Vec<String>) {
        Keymap::default().merge_json(json)
    }

    #[test]
    fn override_and_unbind() {
        let (keymap, warnings) = merged(r#"{"main": {"x": "quit", "q": null}}"#);
        assert!(warnings.is_empty());
        assert_eq!(keymap.lookup("x"), Some(Action::Quit));
        assert_eq!(keymap.lookup("q"), None);
        assert_eq!(keymap.lookup("ctrl+c"), Some(Action::Quit));
        assert_eq!(keymap.lookup("j"), Some(Action::MoveDownOrScrollDown));
    }

    #[test]
    fn bad_file_or_entry_falls_back() {
        let (keymap, warnings) = merged("not json");
        assert_eq!(keymap, Keymap::default());
        assert_eq!(warnings.len(), 1);

        let (keymap, warnings) = merged(r#"{"main": {"q": "explode", "x": "help"}}"#);
        assert_eq!(keymap.lookup("q"), Some(Action::Quit));
        assert_eq!(keymap.lookup("x"), Some(Action::Help));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn missing_file_is_silent() {
        let (keymap, warnings) =
            Keymap::load_from(Path::new("/nonexistent/brew-changes/keymap.json"));
        assert_eq!(keymap, Keymap::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn hint_key_prefers_default_then_first_bound() {
        let keymap = Keymap::default();
        assert_eq!(keymap.hint_key(Action::Quit), Some("q"));
        let (keymap, _) = merged(r#"{"main": {"q": null, "x": "quit"}}"#);
        assert_eq!(keymap.hint_key(Action::Quit), Some("ctrl+c"));
        let (keymap, _) = merged(r#"{"main": {"u": null}}"#);
        assert_eq!(keymap.hint_key(Action::UpgradeCurrent), None);
    }

    #[test]
    fn key_names() {
        let name = |code, modifiers| key_name(&KeyEvent::new(code, modifiers));
        assert_eq!(name(KeyCode::Char('j'), KeyModifiers::NONE), "j");
        assert_eq!(name(KeyCode::Char('K'), KeyModifiers::SHIFT), "K");
        assert_eq!(name(KeyCode::Char('c'), KeyModifiers::CONTROL), "ctrl+c");
        assert_eq!(name(KeyCode::Enter, KeyModifiers::NONE), "enter");
        assert_eq!(name(KeyCode::Up, KeyModifiers::SHIFT), "shift+up");
        assert_eq!(name(KeyCode::Char('?'), KeyModifiers::SHIFT), "?");
    }
}
