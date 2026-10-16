use std::collections::HashMap;

use ratatui::text::Line;

use super::keys::{Action, Keymap};
use super::{markdown, view};
use crate::github::{Changelog, HtmlWithoutVersion, Origin, Source, Stage};

const HEADER_LINES: u16 = 2;
const FOOTER_LINES: u16 = 2;
const MAX_DISPLAYED_RELEASES: usize = 10;
const MAX_BODY_WIDTH: usize = 96;
// Frame margin, border and padding around a body.
const FRAME_CHROME: usize = 6;
const UPGRADE_ERROR_LINES: usize = 40;

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
    pub fn kind(&self) -> &'static str {
        if self.is_cask { "cask" } else { "formula" }
    }

    pub fn github_url(&self) -> Option<String> {
        self.source.github_url()
    }

    pub fn changelog_url(&self) -> Option<String> {
        if !self.source.manual_url.is_empty() {
            return Some(self.source.manual_url.clone());
        }
        let github = self.github_url()?;
        Some(match &self.changelog_filename {
            Some(filename) => format!("{github}/blob/HEAD/{filename}"),
            None => format!("{github}/releases"),
        })
    }

    pub fn homepage_url(&self) -> Option<String> {
        Some(self.homepage.clone())
            .filter(|homepage| !homepage.is_empty())
            .or_else(|| self.github_url())
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
    pub dark: bool,
    pub show_help: bool,
    /// Shown in place of the footer hints until the next key press.
    pub notice: Option<String>,
    pub outcome: Option<Outcome>,
    pub width: u16,
    pub height: u16,
    pub offset: usize,
    pub spinner: usize,
    pub lines: Vec<Line<'static>>,
    /// Line offset of each item's header within `lines`.
    pub item_lines: Vec<usize>,
    /// Keyed on the Markdown source and wrap width so navigation doesn't
    /// re-render large bodies.
    body_cache: HashMap<usize, (String, usize, Vec<Line<'static>>)>,
}

impl App {
    pub fn new(items: Vec<Item>, keymap: Keymap, dark: bool) -> Self {
        Self {
            items,
            cursor: 0,
            keymap,
            dark,
            show_help: false,
            notice: None,
            outcome: None,
            width: 0,
            height: 1,
            offset: 0,
            spinner: 0,
            lines: vec![],
            item_lines: vec![],
            body_cache: HashMap::new(),
        }
    }

    pub fn busy(&self) -> bool {
        self.items
            .iter()
            .any(|item| item.loading_stage.is_some() || item.upgrade == Upgrade::Running)
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
                    item.body = Some(body);
                    item.changelog_filename = filename;
                    item.loading_stage = None;
                    self.refresh();
                }
            }
            Message::UpgradeDone { index, error } => {
                if let Some(item) = self.items.get_mut(index) {
                    item.upgrade = error.map_or(Upgrade::Done, Upgrade::Failed);
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
        if std::mem::take(&mut self.show_help) {
            return vec![];
        }
        let Some(action) = self.keymap.lookup(key) else {
            return vec![];
        };
        match action {
            Action::Help => self.show_help = true,
            Action::Quit => self.outcome = Some(Outcome::Quit),
            Action::UpgradeAll => {
                if !self
                    .items
                    .iter()
                    .any(|item| item.upgrade == Upgrade::Running)
                {
                    self.outcome = Some(Outcome::UpgradeAll);
                }
            }
            _ => return self.item_key(action),
        }
        vec![]
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
                return item.homepage_url().map(Effect::Open).into_iter().collect();
            }
            Action::OpenGithub => return item.github_url().map(Effect::Open).into_iter().collect(),
            Action::OpenChangelog => {
                return item.changelog_url().map(Effect::Open).into_iter().collect();
            }
            Action::UpgradeCurrent => {
                if matches!(item.upgrade, Upgrade::Running | Upgrade::Done) {
                    return vec![];
                }
                item.upgrade = Upgrade::Running;
                self.refresh();
                self.scroll_to_cursor();
                return vec![Effect::Upgrade(cursor)];
            }
            Action::Help | Action::Quit | Action::UpgradeAll => {}
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
        self.refresh();
        self.scroll_to_cursor();
    }

    fn max_offset(&self) -> usize {
        self.lines.len().saturating_sub(self.height as usize)
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
        for (index, item) in self.items.iter().enumerate() {
            // Collapsed bodies stay cached so re-expanding doesn't re-render.
            let Some(source) = body_markdown(item) else {
                continue;
            };
            let cached =
                self.body_cache
                    .get(&index)
                    .is_some_and(|(cached_source, cached_wrap, _)| {
                        *cached_source == source && *cached_wrap == wrap
                    });
            if !cached {
                let lines = markdown::render(&source, wrap, self.dark);
                self.body_cache.insert(index, (source, wrap, lines));
            }
        }
        let (lines, item_lines) = view::list_lines(self, |index| {
            self.body_cache
                .get(&index)
                .map(|(_, _, lines)| lines.as_slice())
        });
        self.lines = lines;
        self.item_lines = item_lines;
        self.offset = self.offset.min(self.max_offset());
    }
}

fn body_markdown(item: &Item) -> Option<String> {
    let error = match &item.upgrade {
        Upgrade::Failed(error) => Some(error),
        _ => None,
    };
    if !item.expanded || (error.is_none() && item.body.is_none()) {
        return None;
    }
    // A tilde fence, since brew's output may itself contain backtick fences.
    let failure = error
        .map(|error| {
            let lines: Vec<&str> = error.lines().collect();
            let tail = lines[lines.len().saturating_sub(UPGRADE_ERROR_LINES)..].join("\n");
            format!("**Upgrade failed:**\n\n~~~~\n{tail}\n~~~~\n\n")
        })
        .unwrap_or_default();
    Some(failure + item.body.as_deref().unwrap_or_default())
}

/// PRD FR-10.2.
pub fn changelog_body(item: &Item, changelog: &Changelog) -> String {
    let manual = &item.source.manual_url;
    match changelog.origin {
        Origin::None => {
            "No GitHub repository could be resolved for this package; changelog unavailable.".into()
        }
        Origin::Manual => match &changelog.error {
            Some(error) if error.is::<HtmlWithoutVersion>() => format!(
                "The configured changelog is an HTML page that doesn't appear to document {}.\n\nKnown changelog: {manual}",
                item.current
            ),
            Some(_) => format!(
                "Could not fetch the configured changelog URL.\n\nKnown changelog: {manual}"
            ),
            None => format!("(from {manual})\n\n{}", changelog.section),
        },
        Origin::File => format!(
            "(from {})\n\n{}",
            changelog.filename.as_deref().unwrap_or_default(),
            changelog.section
        ),
        Origin::Releases if changelog.releases.is_empty() => {
            if item.source.prereleases_only && changelog.error.is_none() {
                "No GitHub pre-releases found for this package.".into()
            } else {
                "No changelog file or GitHub releases found for this package.".into()
            }
        }
        Origin::Releases => {
            let mut text = String::from("(from GitHub releases)\n\n");
            for release in changelog.releases.iter().take(MAX_DISPLAYED_RELEASES) {
                let date = if release.published_at.is_empty() {
                    "unknown date"
                } else {
                    &release.published_at
                };
                let body = if release.body.is_empty() {
                    "_no release notes_"
                } else {
                    &release.body
                };
                text += &format!("### {} ({date})\n\n{body}\n\n", release.tag_name);
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Release;

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
        let mut app = App::new(items, Keymap::default(), true);
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
        app.lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
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
            assert!(app.show_help);
            assert!(key(&mut app, k).is_empty());
            assert!(!app.show_help);
            assert_eq!(app.outcome, None);
            assert!(!app.items[0].expanded);
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
        app.items[0].source = Source {
            owner: "o".into(),
            repo: "r".into(),
            ..Default::default()
        };
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
        app.lines = vec![Line::raw(""); 100];
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
        let mut app = App::new(items(2), Keymap::default(), true);
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
        let none = changelog_body(&Item::default(), &Changelog::default());
        assert_eq!(
            none,
            "No GitHub repository could be resolved for this package; changelog unavailable."
        );

        let err = Changelog {
            origin: Origin::Manual,
            error: Some(anyhow::anyhow!("boom")),
            ..Default::default()
        };
        assert_eq!(
            changelog_body(&manual("https://m"), &err),
            "Could not fetch the configured changelog URL.\n\nKnown changelog: https://m"
        );

        let html = Changelog {
            origin: Origin::Manual,
            error: Some(HtmlWithoutVersion.into()),
            ..Default::default()
        };
        let body = changelog_body(&manual("https://m"), &html);
        assert!(body.starts_with(
            "The configured changelog is an HTML page that doesn't appear to document 1.2.3."
        ));

        let ok = Changelog {
            origin: Origin::Manual,
            section: "## 1.2.3".into(),
            ..Default::default()
        };
        assert_eq!(
            changelog_body(&manual("https://m"), &ok),
            "(from https://m)\n\n## 1.2.3"
        );

        let file = Changelog {
            origin: Origin::File,
            filename: Some("NEWS.md".into()),
            section: "x".into(),
            ..Default::default()
        };
        assert_eq!(
            changelog_body(&Item::default(), &file),
            "(from NEWS.md)\n\nx"
        );

        let empty = Changelog {
            origin: Origin::Releases,
            ..Default::default()
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
    }

    #[test]
    fn release_body_caps_at_ten() {
        let releases = (0..12)
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
        let changelog = Changelog {
            origin: Origin::Releases,
            releases,
            ..Default::default()
        };
        let body = changelog_body(&Item::default(), &changelog);
        assert!(body.starts_with(
            "(from GitHub releases)\n\n### v0 (unknown date)\n\n_no release notes_\n\n"
        ));
        assert_eq!(body.matches("### ").count(), 10);
    }
}
