use std::collections::HashMap;

use ratatui::text::Line;

use super::keys::{Action, Keymap};
use super::markdown::Link;
use super::theme::Theme;
use super::{markdown, view};
use crate::forges::{Changelog, Commits, Releases, Source, Stage};

const HEADER_LINES: u16 = 2;
pub(super) const FOOTER_LINES: u16 = 2;
const MAX_BODY_WIDTH: usize = 96;
// Frame margin, border and padding around a body.
const FRAME_CHROME: usize = 6;
const UPGRADE_ERROR_LINES: usize = 40;
const UPGRADE_RUNNING: &str = "An upgrade is still running; wait for it to finish.";

#[derive(Debug, Default, Clone)]
pub struct Item {
    pub name: String,
    pub is_cask: bool,
    pub installed: String,
    pub current: String,
    pub homepage: String,
    pub source: Source,
    pub changelog_filename: Option<String>,
    pub expanded: bool,
    pub loading_stage: Option<Stage>,
    pub body: Option<String>,
    pub upgrade: Upgrade,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Upgrade {
    #[default]
    Idle,
    Running,
    Done,
    Failed(String),
}

impl Item {
    pub fn changelog_url(&self) -> Option<String> {
        if !self.source.manual_url.is_empty() {
            return Some(self.source.manual_url.clone());
        }
        match &self.changelog_filename {
            Some(filename) => self.source.file_url(filename),
            None => self.source.releases_url(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Quit,
    UpgradeAll,
}

#[derive(Debug)]
pub enum Message {
    Key(String),
    Resize {
        width: u16,
        height: u16,
    },
    Stage {
        index: usize,
        stage: Stage,
    },
    Fetched {
        index: usize,
        body: String,
        filename: Option<String>,
    },
    UpgradeDone {
        index: usize,
        error: Option<String>,
    },
    /// A background action failed with nothing else to show it on.
    Notice(String),
    Tick,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Fetch(usize),
    Upgrade(usize),
    Open(String),
}

pub struct App {
    pub items: Vec<Item>,
    pub cursor: usize,
    pub keymap: Keymap,
    pub theme: Theme,
    /// Every theme the picker offers, by name.
    pub themes: Vec<(String, Theme)>,
    /// The kept theme's index in `themes`, tracked rather than found by value
    /// as a custom theme can equal the built-in it starts from.
    pub active: usize,
    pub popup: Option<Popup>,
    /// Shown in place of the footer hints until the next key press.
    pub notice: Option<String>,
    pub outcome: Option<Outcome>,
    pub width: u16,
    pub height: u16,
    pub offset: usize,
    pub spinner: usize,
    pub line_count: usize,
    /// Line offset of each item's header.
    pub item_lines: Vec<usize>,
    /// Collapsed bodies stay cached so re-expanding doesn't re-render.
    body_cache: HashMap<usize, Body>,
}

#[derive(Debug, PartialEq)]
pub enum Popup {
    Help,
    /// The scroll offset.
    Licences(usize),
    /// The highlighted theme's index, applied as a preview; cancelling
    /// restores `active`. Nothing is saved.
    Theme(usize),
    /// Asked before quitting while an upgrade runs.
    ConfirmQuit,
}

/// Lines are only rebuilt when their inputs change, since bodies can run to
/// thousands of lines and `refresh` runs on every tick while busy. Entries are
/// removed wherever an item's body or upgrade failure changes.
#[derive(Default)]
struct Body {
    wrap: usize,
    framed: Vec<Line<'static>>,
    links: Vec<Link>,
}

impl App {
    pub fn new(
        items: Vec<Item>,
        keymap: Keymap,
        themes: Vec<(String, Theme)>,
        active: usize,
    ) -> Self {
        Self {
            items,
            cursor: 0,
            keymap,
            theme: themes
                .get(active)
                .map_or_else(Theme::default_dark, |&(_, theme)| theme),
            themes,
            active,
            popup: None,
            notice: None,
            outcome: None,
            width: 0,
            height: 1,
            offset: 0,
            spinner: 0,
            line_count: 0,
            item_lines: vec![],
            body_cache: HashMap::new(),
        }
    }

    pub fn busy(&self) -> bool {
        self.upgrading() || self.items.iter().any(|item| item.loading_stage.is_some())
    }

    pub fn update(&mut self, message: Message) -> Vec<Effect> {
        match message {
            Message::Resize { width, height } => {
                self.width = width;
                self.height = height.saturating_sub(HEADER_LINES + FOOTER_LINES).max(1);
                self.refresh();
                self.scroll_to_cursor();
            }
            Message::Key(key) => {
                self.notice = None;
                return self.key(&key);
            }
            Message::Notice(notice) => self.notice = Some(notice),
            Message::Stage { index, stage } => {
                if let Some(item) = self.items.get_mut(index) {
                    item.loading_stage = Some(stage);
                    self.refresh();
                }
            }
            Message::Fetched {
                index,
                body,
                filename,
            } => {
                if let Some(item) = self.items.get_mut(index) {
                    item.body = Some(if item.installed == item.current {
                        format!(
                            "_A Homebrew rebuild of {} with no upstream version change; these are the notes you already have._\n\n{body}",
                            item.current
                        )
                    } else {
                        body
                    });
                    item.changelog_filename = filename;
                    item.loading_stage = None;
                    self.body_cache.remove(&index);
                    self.refresh();
                }
            }
            Message::UpgradeDone { index, error } => {
                if let Some(item) = self.items.get_mut(index) {
                    item.upgrade = error.map_or(Upgrade::Done, Upgrade::Failed);
                    self.body_cache.remove(&index);
                    self.refresh();
                }
            }
            Message::Tick => {
                if self.busy() {
                    self.spinner = self.spinner.wrapping_add(1);
                    self.refresh();
                }
            }
        }
        vec![]
    }

    fn key(&mut self, key: &str) -> Vec<Effect> {
        match self.popup.take() {
            Some(Popup::Help) if key == "l" => self.popup = Some(Popup::Licences(0)),
            Some(Popup::Help) => {}
            Some(Popup::Licences(offset)) => {
                self.popup = self.licences_key(key, offset).map(Popup::Licences);
            }
            Some(Popup::Theme(selected)) => self.theme_picker_key(key, selected),
            // A repeated quit key confirms, so ctrl+c twice always exits.
            Some(Popup::ConfirmQuit)
                if key == "y" || self.keymap.lookup(key) == Some(Action::Quit) =>
            {
                self.outcome = Some(Outcome::Quit);
            }
            Some(Popup::ConfirmQuit) => {}
            None => return self.main_key(key),
        }
        vec![]
    }

    fn main_key(&mut self, key: &str) -> Vec<Effect> {
        let Some(action) = self.keymap.lookup(key) else {
            return vec![];
        };
        match action {
            Action::Help => self.popup = Some(Popup::Help),
            Action::PickTheme => self.popup = Some(Popup::Theme(self.active)),
            // Exiting could interrupt the running `brew upgrade` mid-install.
            Action::Quit if self.upgrading() => self.popup = Some(Popup::ConfirmQuit),
            // Concurrent `brew upgrade`s contend for Homebrew's locks.
            Action::UpgradeAll | Action::UpgradeCurrent if self.upgrading() => {
                self.notice = Some(UPGRADE_RUNNING.into());
            }
            Action::Quit => self.outcome = Some(Outcome::Quit),
            Action::UpgradeAll => self.outcome = Some(Outcome::UpgradeAll),
            _ => return self.item_key(action),
        }
        vec![]
    }

    /// Quit cancels rather than exiting, as the picker is a modal.
    fn theme_picker_key(&mut self, key: &str, mut selected: usize) {
        let action = self.keymap.lookup(key);
        match action {
            Some(Action::MoveUp | Action::MoveUpOrScrollUp) => {
                selected = selected.saturating_sub(1);
            }
            Some(Action::MoveDown | Action::MoveDownOrScrollDown) => {
                selected = (selected + 1).min(self.themes.len().saturating_sub(1));
            }
            Some(Action::ToggleExpand) => return self.active = selected,
            Some(Action::Quit | Action::PickTheme) => return self.preview(self.active),
            _ if key == "esc" => return self.preview(self.active),
            _ => {}
        }
        self.preview(selected);
        self.popup = Some(Popup::Theme(selected));
    }

    /// The new scroll offset, or None once closed. Scrolling stops when the
    /// last line is in view, so scrolling back up responds straight away.
    fn licences_key(&self, key: &str, offset: usize) -> Option<usize> {
        // Popups stop above the footer.
        let page = usize::from((self.height + HEADER_LINES).saturating_sub(view::POPUP_CHROME));
        let offset = match (self.keymap.lookup(key), key) {
            (Some(Action::Quit | Action::Help), _) | (_, "esc") => return None,
            (Some(Action::MoveUp | Action::MoveUpOrScrollUp), _) => offset.saturating_sub(1),
            (Some(Action::MoveDown | Action::MoveDownOrScrollDown), _) => offset + 1,
            (_, "pgup") => offset.saturating_sub(page),
            (_, "pgdown" | "space") => offset + page,
            _ => offset,
        };
        Some(offset.min(view::LICENCES.len().saturating_sub(page)))
    }

    /// Cached bodies hold styled lines, so they're rebuilt in the new theme.
    fn preview(&mut self, index: usize) {
        let Some(&(_, theme)) = self.themes.get(index) else {
            return;
        };
        if theme != self.theme {
            self.theme = theme;
            self.body_cache.clear();
            self.refresh();
        }
    }

    fn upgrading(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.upgrade == Upgrade::Running)
    }

    fn item_key(&mut self, action: Action) -> Vec<Effect> {
        let cursor = self.cursor;
        let Some(item) = self.items.get_mut(cursor) else {
            return vec![];
        };
        match action {
            Action::MoveUpOrScrollUp if item.expanded => {
                self.offset = self.offset.saturating_sub(1)
            }
            Action::MoveDownOrScrollDown if item.expanded => {
                self.offset = (self.offset + 1).min(self.max_offset())
            }
            Action::MoveUp | Action::MoveUpOrScrollUp => self.move_cursor(-1),
            Action::MoveDown | Action::MoveDownOrScrollDown => self.move_cursor(1),
            Action::ToggleExpand => {
                item.expanded = !item.expanded;
                let fetch = item.expanded && item.body.is_none() && item.loading_stage.is_none();
                if fetch {
                    item.loading_stage = Some(Stage::Changelog);
                }
                self.refresh();
                self.scroll_to_cursor();
                if fetch {
                    return vec![Effect::Fetch(cursor)];
                }
            }
            Action::OpenHomepage => {
                return Some(item.homepage.clone())
                    .filter(|homepage| !homepage.is_empty())
                    .or_else(|| item.source.repo_url())
                    .map(Effect::Open)
                    .into_iter()
                    .collect();
            }
            Action::OpenGithub => {
                return item
                    .source
                    .repo_url()
                    .map(Effect::Open)
                    .into_iter()
                    .collect();
            }
            Action::OpenChangelog => {
                return item.changelog_url().map(Effect::Open).into_iter().collect();
            }
            Action::UpgradeCurrent => {
                if item.upgrade == Upgrade::Done {
                    return vec![];
                }
                item.upgrade = Upgrade::Running;
                self.body_cache.remove(&cursor);
                self.refresh();
                self.scroll_to_cursor();
                return vec![Effect::Upgrade(cursor)];
            }
            Action::Help | Action::Quit | Action::UpgradeAll | Action::PickTheme => {}
        }
        vec![]
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        self.cursor = self
            .cursor
            .saturating_add_signed(delta)
            .min(self.items.len() - 1);
        self.scroll_to_cursor();
    }

    fn max_offset(&self) -> usize {
        self.line_count.saturating_sub(self.height as usize)
    }

    pub fn scroll_to_cursor(&mut self) {
        let Some(&target) = self.item_lines.get(self.cursor) else {
            return;
        };
        let height = self.height as usize;
        if target < self.offset {
            self.offset = target;
        } else if target >= self.offset + height {
            self.offset = target + 1 - height;
        }
        self.offset = self.offset.min(self.max_offset());
    }

    pub fn refresh(&mut self) {
        let wrap = (self.width as usize)
            .saturating_sub(FRAME_CHROME)
            .min(MAX_BODY_WIDTH);
        self.item_lines.clear();
        let mut line = 0;
        for (index, item) in self.items.iter().enumerate() {
            self.item_lines.push(line);
            line += 1;
            if !item.expanded {
                continue;
            }
            let loading = item.loading_stage.as_ref();
            let body = self.body_cache.entry(index).or_default();
            // `framed` is never empty once built, as it includes the border.
            // While loading the body is at most an upgrade failure, so
            // re-rendering it for each spinner frame is cheap. Loading ends
            // only in `Fetched`, which drops the entry.
            if body.framed.is_empty() || body.wrap != wrap || loading.is_some() {
                let source = body_markdown(item);
                let (lines, links) = markdown::render(&source, wrap, &self.theme);
                (body.framed, body.links) =
                    view::frame_body(lines, links, loading, self.spinner, &self.theme);
                body.wrap = wrap;
            }
            line += body.framed.len();
        }
        self.line_count = line;
        self.offset = self.offset.min(self.max_offset());
    }

    pub fn body_lines(&self, index: usize) -> &[Line<'static>] {
        self.body(index).map_or(&[], |body| &body.framed)
    }

    pub fn body_links(&self, index: usize) -> &[Link] {
        self.body(index).map_or(&[], |body| &body.links)
    }

    fn body(&self, index: usize) -> Option<&Body> {
        match (self.items.get(index), self.body_cache.get(&index)) {
            (Some(item), Some(body)) if item.expanded => Some(body),
            _ => None,
        }
    }
}

fn body_markdown(item: &Item) -> String {
    // A tilde fence, since brew's output may itself contain backtick fences.
    let failure = match &item.upgrade {
        Upgrade::Failed(error) => {
            let lines: Vec<&str> = error.lines().collect();
            let tail = lines[lines.len().saturating_sub(UPGRADE_ERROR_LINES)..].join("\n");
            format!("**Upgrade failed:**\n\n~~~~\n{tail}\n~~~~\n\n")
        }
        _ => String::new(),
    };
    failure + item.body.as_deref().unwrap_or_default()
}

pub fn changelog_body(item: &Item, changelog: &Changelog) -> String {
    let manual = &item.source.manual_url;
    let forge = item.source.forge_name();
    let releases = item.source.releases_label();
    match changelog {
        Changelog::NoSource => {
            "No repository could be resolved for this package; changelog unavailable.".into()
        }
        Changelog::ManualHtmlWithoutVersion => format!(
            "The configured changelog is an HTML page that doesn't appear to document {}.\n\nKnown changelog: {manual}",
            item.current
        ),
        Changelog::ManualFailed(error) => format!(
            "Could not fetch the configured changelog URL.\n\n{error:#}\n\nKnown changelog: {manual}"
        ),
        Changelog::Manual { section, .. } => format!("(from {manual})\n\n{section}"),
        Changelog::File {
            filename, section, ..
        } => format!("(from {filename})\n\n{section}"),
        Changelog::Releases {
            result: Ok(Releases { list, found }),
            ..
        } if !list.is_empty() => {
            let mut text = if *found {
                String::new()
            } else {
                format!(
                    "No {forge} release matches {}; showing the newest instead.\n\n",
                    item.current
                )
            };
            text += &format!("(from {releases})\n\n");
            for release in list {
                let date = if release.published_at.is_empty() {
                    "unknown date"
                } else {
                    &release.published_at
                };
                let body = if release.lacks_notes() {
                    "_no release notes_"
                } else {
                    &release.body
                };
                text += &format!("### {} ({date})\n\n{body}\n\n", release.tag_name);
            }
            text
        }
        Changelog::Releases {
            result: Err(error), ..
        } => format!("Could not fetch {releases}.\n\n{error:#}"),
        Changelog::Releases {
            fallback_file,
            file_error: Some(error),
            ..
        } => format!(
            "Could not fetch {}, and no {forge} release matches {}.\n\n{error:#}",
            fallback_file
                .as_deref()
                .unwrap_or("the repository's file listing"),
            item.current
        ),
        Changelog::Releases {
            fallback_file: Some(filename),
            ..
        } => format!(
            "{filename} doesn't document {}, and no {forge} release matches it.",
            item.current
        ),
        Changelog::Releases { result: Ok(_), .. } if item.source.prereleases_only => {
            format!("No {forge} pre-releases found for this package.")
        }
        Changelog::Releases { .. } => {
            format!("No changelog file or {releases} found for this package.")
        }
        Changelog::Commits(Ok(Commits { list, .. })) if list.is_empty() => {
            "No commits found between these versions.".into()
        }
        Changelog::Commits(Ok(Commits { list, truncated })) => {
            let repo_url = item.source.repo_url().unwrap_or_default();
            let mut text = if *truncated {
                format!(
                    "Showing the newest {} commits; older ones were cut.\n\n",
                    list.len()
                )
            } else {
                String::new()
            };
            text += "(from GitHub commits)\n\n";
            for commit in list {
                text += &format!(
                    "- {} ([{}]({repo_url}/commit/{}), {})\n",
                    commit.subject,
                    commit.sha.get(..7).unwrap_or(&commit.sha),
                    commit.sha,
                    commit.date.get(..10).unwrap_or(&commit.date)
                );
            }
            text
        }
        Changelog::Commits(Err(error)) => format!("Could not fetch GitHub commits.\n\n{error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forges::{Commit, Commits, GitHub, GitLab, Release};
    use crate::ui::theme::ThemeConfig;

    fn items(n: usize) -> Vec<Item> {
        (0..n)
            .map(|i| Item {
                name: format!("pkg{i}"),
                installed: "1.0".into(),
                current: "2.0".into(),
                ..Default::default()
            })
            .collect()
    }

    fn ready(items: Vec<Item>) -> App {
        let mut app = App::new(items, Keymap::default(), ThemeConfig::default().themes(), 0);
        app.update(Message::Resize {
            width: 80,
            height: 14,
        });
        app
    }

    fn key(app: &mut App, k: &str) -> Vec<Effect> {
        app.update(Message::Key(k.into()))
    }

    fn line_text(app: &App) -> String {
        view::lines_in(app, 0..app.line_count)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn picked(app: &App) -> Option<usize> {
        match app.popup {
            Some(Popup::Theme(selected)) => Some(selected),
            _ => None,
        }
    }

    #[test]
    fn theme_picker_previews_keeps_and_cancels() {
        let mut app = ready(items(1));
        let original = app.theme;
        key(&mut app, "t");
        assert_eq!(picked(&app), Some(0));
        key(&mut app, "j");
        assert_eq!(picked(&app), Some(1));
        assert_eq!(app.theme, app.themes[1].1);
        key(&mut app, "esc");
        assert_eq!(picked(&app), None);
        assert_eq!(app.theme, original);

        key(&mut app, "t");
        key(&mut app, "k");
        assert_eq!(picked(&app), Some(0));
        for _ in 0..app.themes.len() + 1 {
            key(&mut app, "down");
        }
        assert_eq!(picked(&app), Some(app.themes.len() - 1));
        key(&mut app, "up");
        key(&mut app, "enter");
        assert_eq!(picked(&app), None);
        assert_eq!(app.theme, app.themes[app.themes.len() - 2].1);
        assert!(!app.items[0].expanded);
    }

    #[test]
    fn theme_picker_reopens_on_the_kept_theme_not_an_identical_one() {
        let mut app = ready(items(1));
        app.themes.push(("zed".into(), Theme::default_light()));
        key(&mut app, "t");
        for _ in 1..app.themes.len() {
            key(&mut app, "j");
        }
        key(&mut app, "enter");
        key(&mut app, "t");
        assert_eq!(picked(&app), Some(app.themes.len() - 1));
    }

    #[test]
    fn quit_in_theme_picker_cancels_instead_of_quitting() {
        let mut app = ready(items(1));
        let original = app.theme;
        key(&mut app, "t");
        key(&mut app, "j");
        key(&mut app, "q");
        assert_eq!(picked(&app), None);
        assert_eq!(app.outcome, None);
        assert_eq!(app.theme, original);
    }

    #[test]
    fn theme_change_rerenders_cached_bodies() {
        let mut item = items(1).remove(0);
        item.body = Some("notes".into());
        item.expanded = true;
        let mut app = ready(vec![item]);
        let border = |app: &App| app.body_lines(0)[0].spans[1].style;
        assert_eq!(border(&app), app.theme.border);
        let light = app
            .themes
            .iter()
            .position(|(name, _)| name == "InspiredGitHub");
        key(&mut app, "t");
        for _ in 0..light.unwrap_or_default() {
            key(&mut app, "j");
        }
        assert_eq!(border(&app), Theme::default_light().border);
    }

    #[test]
    fn cursor_clamps_at_bounds() {
        let mut app = ready(items(3));
        app.move_cursor(-5);
        assert_eq!(app.cursor, 0);
        app.move_cursor(10);
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn empty_list_ignores_navigation_and_actions() {
        let mut app = ready(vec![]);
        for k in ["down", "j", "enter", "u", "o", "g", "c"] {
            assert!(key(&mut app, k).is_empty(), "{k}");
        }
        assert_eq!(app.cursor, 0);
        assert_eq!(app.outcome, None);
    }

    #[test]
    fn arrow_keys_move_cursor() {
        let mut app = ready(items(3));
        key(&mut app, "down");
        assert_eq!(app.cursor, 1);
        key(&mut app, "up");
        assert_eq!(app.cursor, 0);
        key(&mut app, "J");
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn jk_move_cursor_when_collapsed() {
        let mut app = ready(items(3));
        key(&mut app, "j");
        assert_eq!(app.cursor, 1);
        key(&mut app, "k");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn jk_scroll_when_expanded() {
        let mut app = ready(items(1));
        app.items[0].expanded = true;
        app.items[0].body = Some("line\n\n".repeat(50));
        app.refresh();
        key(&mut app, "j");
        assert_eq!(app.cursor, 0);
        assert_eq!(app.offset, 1);
        key(&mut app, "k");
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn enter_toggles_and_starts_exactly_one_fetch() {
        let mut app = ready(items(1));
        assert_eq!(key(&mut app, "enter"), [Effect::Fetch(0)]);
        assert!(app.items[0].expanded);
        assert_eq!(app.items[0].loading_stage, Some(Stage::Changelog));
        assert!(key(&mut app, "enter").is_empty());
        assert!(!app.items[0].expanded);
        // Re-expanding while the fetch is in flight doesn't start another.
        assert!(key(&mut app, "enter").is_empty());
    }

    #[test]
    fn rebuild_with_same_upstream_version_prefixes_a_note() {
        let mut app = ready(vec![Item {
            name: "node".into(),
            installed: "26.13.0".into(),
            current: "26.13.0".into(),
            ..Default::default()
        }]);
        assert_eq!(key(&mut app, "enter"), [Effect::Fetch(0)]);
        app.update(Message::Fetched {
            index: 0,
            body: "notes".into(),
            filename: None,
        });
        let body = app.items[0].body.as_deref().unwrap_or_default();
        assert!(body.contains("rebuild"), "{body:?}");
        assert!(body.ends_with("\n\nnotes"), "{body:?}");
    }

    #[test]
    fn fetched_body_is_cached_for_the_session() {
        let mut app = ready(items(1));
        key(&mut app, "enter");
        app.update(Message::Fetched {
            index: 0,
            body: "notes".into(),
            filename: Some("CHANGELOG.md".into()),
        });
        key(&mut app, "enter");
        assert!(key(&mut app, "enter").is_empty());
        assert_eq!(
            app.items[0].changelog_filename.as_deref(),
            Some("CHANGELOG.md")
        );
        assert_eq!(app.items[0].loading_stage, None);
    }

    #[test]
    fn stage_updates_loading_text() {
        let mut app = ready(items(1));
        key(&mut app, "enter");
        app.update(Message::Stage {
            index: 0,
            stage: Stage::File("CHANGELOG.md".into()),
        });
        assert!(line_text(&app).contains("checking CHANGELOG.md..."));
        app.update(Message::Stage {
            index: 0,
            stage: Stage::Releases,
        });
        assert!(line_text(&app).contains("fetching releases..."));
    }

    #[test]
    fn loading_spinner_ticks_then_clears() {
        let mut app = ready(items(1));
        key(&mut app, "enter");
        let before = line_text(&app);
        app.update(Message::Tick);
        assert_ne!(line_text(&app), before);
        app.update(Message::Fetched {
            index: 0,
            body: String::new(),
            filename: None,
        });
        let text = line_text(&app);
        assert!(!text.contains("checking") && text.contains("(empty)"));
    }

    #[test]
    fn out_of_range_results_are_ignored() {
        let mut app = ready(items(1));
        app.update(Message::Fetched {
            index: 5,
            body: "x".into(),
            filename: None,
        });
        app.update(Message::Stage {
            index: 5,
            stage: Stage::Releases,
        });
        app.update(Message::UpgradeDone {
            index: 5,
            error: None,
        });
        assert_eq!(app.items.len(), 1);
        assert!(app.items[0].body.is_none());
    }

    #[test]
    fn a_quits_with_upgrade_all_unless_upgrading() {
        let mut app = ready(items(2));
        app.items[1].upgrade = Upgrade::Running;
        key(&mut app, "a");
        assert_eq!(app.outcome, None);
        assert_eq!(app.notice.as_deref(), Some(UPGRADE_RUNNING));
        app.items[1].upgrade = Upgrade::Idle;
        key(&mut app, "a");
        assert_eq!(app.outcome, Some(Outcome::UpgradeAll));
    }

    #[test]
    fn u_upgrades_in_place() {
        let mut app = ready(items(2));
        app.items[0].upgrade = Upgrade::Failed("old".into());
        assert_eq!(key(&mut app, "u"), [Effect::Upgrade(0)]);
        assert_eq!(app.outcome, None);
        assert_eq!(app.items[0].upgrade, Upgrade::Running);
        assert!(key(&mut app, "u").is_empty(), "already upgrading");
        app.update(Message::UpgradeDone {
            index: 0,
            error: None,
        });
        assert_eq!(app.items[0].upgrade, Upgrade::Done);
        assert!(key(&mut app, "u").is_empty(), "already upgraded");
    }

    #[test]
    fn u_waits_for_another_running_upgrade() {
        let mut app = ready(items(2));
        assert_eq!(key(&mut app, "u"), [Effect::Upgrade(0)]);
        key(&mut app, "j");
        assert!(key(&mut app, "u").is_empty());
        assert_eq!(app.items[1].upgrade, Upgrade::Idle);
        assert_eq!(app.notice.as_deref(), Some(UPGRADE_RUNNING));
    }

    #[test]
    fn upgrade_failure_is_shown_when_expanded() {
        let mut app = ready(items(1));
        key(&mut app, "u");
        app.update(Message::UpgradeDone {
            index: 0,
            error: Some("brew upgrade pkg0: exit 1\nError: boom".into()),
        });
        assert!(matches!(app.items[0].upgrade, Upgrade::Failed(_)));
        assert!(line_text(&app).contains("[upgrade failed]"));
        assert!(!line_text(&app).contains("Error: boom"));
        app.items[0].body = Some("notes".into());
        key(&mut app, "enter");
        let text = line_text(&app);
        assert!(
            text.contains("Upgrade failed:")
                && text.contains("Error: boom")
                && text.contains("notes")
        );
    }

    #[test]
    fn help_opens_and_any_key_closes_it() {
        for k in ["q", "?", "enter", "a"] {
            let mut app = ready(items(2));
            assert!(key(&mut app, "?").is_empty());
            assert_eq!(app.popup, Some(Popup::Help));
            assert!(key(&mut app, k).is_empty());
            assert_eq!(app.popup, None);
            assert_eq!(app.outcome, None);
            assert!(!app.items[0].expanded);
        }
    }

    #[test]
    fn licences_open_from_help_scroll_and_close() {
        for close in ["esc", "q", "?"] {
            let mut app = ready(items(2));
            key(&mut app, "?");
            key(&mut app, "l");
            assert_eq!(app.popup, Some(Popup::Licences(0)));
            key(&mut app, "j");
            key(&mut app, "down");
            key(&mut app, "k");
            assert_eq!(app.popup, Some(Popup::Licences(1)));
            key(&mut app, "pgdown");
            assert_eq!(app.popup, Some(Popup::Licences(9)));
            key(&mut app, "pgup");
            assert_eq!(app.popup, Some(Popup::Licences(1)));
            for _ in 0..view::LICENCES.len() {
                key(&mut app, "space");
            }
            assert_eq!(app.popup, Some(Popup::Licences(view::LICENCES.len() - 8)));
            assert!(key(&mut app, close).is_empty());
            assert_eq!(app.popup, None);
            assert_eq!(app.outcome, None);
        }
    }

    #[test]
    fn notice_lasts_until_next_key() {
        let mut app = ready(items(1));
        app.update(Message::Notice("xdg-open: not found".into()));
        assert_eq!(app.notice.as_deref(), Some("xdg-open: not found"));
        key(&mut app, "down");
        assert_eq!(app.notice, None);
    }

    #[test]
    fn quit_while_upgrading_asks_first() {
        for (first, confirm) in [("q", "y"), ("q", "q"), ("ctrl+c", "ctrl+c")] {
            let mut app = ready(items(2));
            app.items[1].upgrade = Upgrade::Running;
            key(&mut app, first);
            assert_eq!(app.popup, Some(Popup::ConfirmQuit), "{first}");
            assert_eq!(app.outcome, None, "{first}");
            key(&mut app, confirm);
            assert_eq!(app.outcome, Some(Outcome::Quit), "{first} {confirm}");
        }
        for cancel in ["n", "esc", "enter"] {
            let mut app = ready(items(2));
            app.items[1].upgrade = Upgrade::Running;
            key(&mut app, "q");
            key(&mut app, cancel);
            assert_eq!(app.popup, None, "{cancel}");
            assert_eq!(app.outcome, None, "{cancel}");
            assert!(!app.items[0].expanded, "{cancel}");
        }
    }

    #[test]
    fn retried_upgrade_clears_old_failure_from_body() {
        let mut app = ready(items(1));
        app.items[0].body = Some("notes".into());
        key(&mut app, "enter");
        key(&mut app, "u");
        app.update(Message::UpgradeDone {
            index: 0,
            error: Some("Error: boom".into()),
        });
        assert!(line_text(&app).contains("Error: boom"));
        key(&mut app, "u");
        assert!(!line_text(&app).contains("Error: boom"));
    }

    #[test]
    fn quit_keys() {
        for k in ["q", "ctrl+c"] {
            let mut app = ready(items(1));
            key(&mut app, k);
            assert_eq!(app.outcome, Some(Outcome::Quit));
        }
    }

    #[test]
    fn open_effects() {
        let mut app = ready(items(1));
        assert!(key(&mut app, "o").is_empty());
        assert!(key(&mut app, "g").is_empty());
        assert!(key(&mut app, "c").is_empty());
        app.items[0].source = Source::with_forge(GitHub::new("o", "r"));
        assert_eq!(
            key(&mut app, "o"),
            [Effect::Open("https://github.com/o/r".into())]
        );
        assert_eq!(
            key(&mut app, "c"),
            [Effect::Open("https://github.com/o/r/releases".into())]
        );
        app.items[0].changelog_filename = Some("NEWS".into());
        assert_eq!(
            key(&mut app, "c"),
            [Effect::Open("https://github.com/o/r/blob/HEAD/NEWS".into())]
        );
        app.items[0].homepage = "https://home".into();
        assert_eq!(key(&mut app, "o"), [Effect::Open("https://home".into())]);
        app.items[0].source.manual_url = "https://manual".into();
        assert_eq!(key(&mut app, "c"), [Effect::Open("https://manual".into())]);
    }

    #[test]
    fn scroll_to_cursor_both_directions() {
        let mut app = ready(items(20));
        app.height = 5;
        app.line_count = 100;
        app.item_lines = (0..20).map(|i| i * 3).collect();
        app.cursor = 19;
        app.scroll_to_cursor();
        assert_eq!(app.offset, 57 - 5 + 1);
        app.cursor = 0;
        app.scroll_to_cursor();
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn resize_sets_viewport_clamped_to_one() {
        let mut app = App::new(
            items(2),
            Keymap::default(),
            ThemeConfig::default().themes(),
            0,
        );
        app.update(Message::Resize {
            width: 100,
            height: 20,
        });
        assert_eq!((app.width, app.height), (100, 16));
        app.update(Message::Resize {
            width: 100,
            height: 1,
        });
        assert_eq!(app.height, 1);
    }

    #[test]
    fn spinner_ticks_only_while_busy() {
        let mut app = ready(items(1));
        app.update(Message::Tick);
        assert_eq!(app.spinner, 0);
        app.items[0].upgrade = Upgrade::Running;
        app.update(Message::Tick);
        assert_eq!(app.spinner, 1);
    }

    fn manual(url: &str) -> Item {
        Item {
            current: "1.2.3".into(),
            source: Source {
                manual_url: url.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn body_texts() {
        let none = changelog_body(&Item::default(), &Changelog::NoSource);
        assert_eq!(
            none,
            "No repository could be resolved for this package; changelog unavailable."
        );

        let err = Changelog::ManualFailed(anyhow::anyhow!("boom"));
        assert_eq!(
            changelog_body(&manual("https://m"), &err),
            "Could not fetch the configured changelog URL.\n\nboom\n\nKnown changelog: https://m"
        );

        let html = Changelog::ManualHtmlWithoutVersion;
        let body = changelog_body(&manual("https://m"), &html);
        assert!(body.starts_with(
            "The configured changelog is an HTML page that doesn't appear to document 1.2.3."
        ));

        let ok = Changelog::Manual {
            section: "## 1.2.3".into(),
            found: true,
            end: None,
        };
        assert_eq!(
            changelog_body(&manual("https://m"), &ok),
            "(from https://m)\n\n## 1.2.3"
        );

        let file = Changelog::File {
            filename: "NEWS.md".into(),
            section: "x".into(),
            end: None,
        };
        assert_eq!(
            changelog_body(&Item::default(), &file),
            "(from NEWS.md)\n\nx"
        );

        let empty = Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Ok(Releases::default()),
        };
        assert_eq!(
            changelog_body(&Item::default(), &empty),
            "No changelog file or GitHub releases found for this package."
        );
        let pre = Item {
            source: Source {
                prereleases_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            changelog_body(&pre, &empty),
            "No GitHub pre-releases found for this package."
        );
        let undocumented = Changelog::Releases {
            fallback_file: Some("CHANGELOG.md".into()),
            file_error: None,
            result: Ok(Releases::default()),
        };
        assert_eq!(
            changelog_body(&items(1)[0], &undocumented),
            "CHANGELOG.md doesn't document 2.0, and no GitHub release matches it."
        );
        let failed = Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Err(anyhow::anyhow!("boom")),
        };
        assert_eq!(
            changelog_body(&pre, &failed),
            "Could not fetch GitHub releases.\n\nboom"
        );
        for (fallback_file, what) in [
            (Some("CHANGELOG.md"), "CHANGELOG.md"),
            (None, "the repository's file listing"),
        ] {
            let unreadable = Changelog::Releases {
                fallback_file: fallback_file.map(String::from),
                file_error: Some(anyhow::anyhow!("boom")),
                result: Ok(Releases::default()),
            };
            assert_eq!(
                changelog_body(&items(1)[0], &unreadable),
                format!("Could not fetch {what}, and no GitHub release matches 2.0.\n\nboom")
            );
        }
    }

    #[test]
    fn release_body_shows_the_whole_found_range() {
        let list = (0..12)
            .map(|i| Release {
                tag_name: format!("v{i}"),
                ..Default::default()
            })
            .chain([Release {
                tag_name: "vX".into(),
                published_at: "2026".into(),
                body: "notes".into(),
                ..Default::default()
            }])
            .collect();
        let changelog = Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Ok(Releases { list, found: true }),
        };
        let body = changelog_body(&Item::default(), &changelog);
        assert!(body.starts_with(
            "(from GitHub releases)\n\n### v0 (unknown date)\n\n_no release notes_\n\n"
        ));
        assert_eq!(body.matches("### ").count(), 13);
        assert!(body.contains("### vX (2026)\n\nnotes"));
    }

    #[test]
    fn release_body_says_when_target_is_missing() {
        let item = Item {
            current: "2.0.31".into(),
            ..Default::default()
        };
        let changelog = Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Ok(Releases {
                list: vec![Release {
                    tag_name: "v2.0.32".into(),
                    body: "notes".into(),
                    ..Default::default()
                }],
                found: false,
            }),
        };
        assert!(changelog_body(&item, &changelog).starts_with(
            "No GitHub release matches 2.0.31; showing the newest instead.\n\n(from GitHub releases)\n\n### v2.0.32"
        ));
    }

    #[test]
    fn other_forge_links_and_labels() {
        let mut item = Item {
            source: Source::with_forge(GitLab::new("https://gitlab.gnome.org", "GNOME", "glib")),
            ..Default::default()
        };
        assert_eq!(
            item.changelog_url().as_deref(),
            Some("https://gitlab.gnome.org/GNOME/glib/-/releases")
        );
        item.changelog_filename = Some("NEWS".into());
        assert_eq!(
            item.changelog_url().as_deref(),
            Some("https://gitlab.gnome.org/GNOME/glib/-/blob/HEAD/NEWS")
        );
        let changelog = Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Ok(Releases {
                list: vec![Release {
                    tag_name: "2.86.0".into(),
                    body: "notes".into(),
                    ..Default::default()
                }],
                found: true,
            }),
        };
        assert!(
            changelog_body(&item, &changelog).starts_with("(from GitLab releases)\n\n"),
            "{}",
            changelog_body(&item, &changelog)
        );
    }

    #[test]
    fn commit_log_body() {
        let item = Item {
            source: Source {
                commit_log: true,
                ..Source::with_forge(GitHub::new("o", "r"))
            },
            ..Default::default()
        };
        let mut commits = Commits {
            list: vec![Commit {
                sha: "ea9eb7edb5eac4a9d175d74523db34fac0fa5180".into(),
                date: "2026-10-30T18:07:02Z".into(),
                subject: "Don't FOLD ldexp.".into(),
            }],
            truncated: false,
        };
        let line = "- Don't FOLD ldexp. ([ea9eb7e](https://github.com/o/r/commit/ea9eb7edb5eac4a9d175d74523db34fac0fa5180), 2026-10-30)\n";
        assert_eq!(
            changelog_body(&item, &Changelog::Commits(Ok(commits.clone()))),
            format!("(from GitHub commits)\n\n{line}")
        );
        commits.truncated = true;
        assert_eq!(
            changelog_body(&item, &Changelog::Commits(Ok(commits))),
            format!(
                "Showing the newest 1 commits; older ones were cut.\n\n(from GitHub commits)\n\n{line}"
            )
        );
        assert_eq!(
            changelog_body(&item, &Changelog::Commits(Ok(Commits::default()))),
            "No commits found between these versions."
        );
        assert_eq!(
            changelog_body(&item, &Changelog::Commits(Err(anyhow::anyhow!("boom")))),
            "Could not fetch GitHub commits.\n\nboom"
        );
    }

    #[test]
    fn release_body_of_only_the_version_counts_as_no_notes() {
        let changelog = Changelog::Releases {
            fallback_file: None,
            file_error: None,
            result: Ok(Releases {
                list: vec![Release {
                    tag_name: "7.1.2-35".into(),
                    body: "7.1.2-35".into(),
                    ..Default::default()
                }],
                found: true,
            }),
        };
        assert_eq!(
            changelog_body(&Item::default(), &changelog),
            "(from GitHub releases)\n\n### 7.1.2-35 (unknown date)\n\n_no release notes_\n\n"
        );
    }
}
