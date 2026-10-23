use std::num::NonZeroU16;
use std::ops::Range;
use std::sync::LazyLock;

use ratatui::Frame;
use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

use unicode_width::UnicodeWidthStr;

use super::app::{App, FOOTER_LINES, Item, Popup, Upgrade};
use super::keys::{self, Action};
use super::markdown::{self, Link};
use super::theme::Theme;
use crate::github::Stage;

/// A popup's borders and padding, in lines.
pub const POPUP_CHROME: u16 = 4;

const LICENCE_WIDTH: usize = 80;

pub enum Licence {
    Heading,
    Path,
    Text,
}

/// The licences of two-face's bundled syntaxes and themes. Lines are wrapped
/// at a fixed width, not the terminal's, so the scroll range doesn't depend
/// on it. Only the few long lines are wrapped, as wrapping drops indentation.
pub static LICENCES: LazyLock<Vec<(Licence, String)>> = LazyLock::new(|| {
    let listing = two_face::acknowledgement::listing();
    let mut lines = vec![
        (
            Licence::Text,
            "Syntax highlighting uses the syntaxes and themes curated by".into(),
        ),
        (
            Licence::Text,
            "bat (github.com/sharkdp/bat), packaged by two-face.".into(),
        ),
    ];
    for (heading, licences) in [
        ("Syntaxes", listing.for_syntaxes()),
        ("Themes", listing.for_themes()),
    ] {
        lines.push((Licence::Text, String::new()));
        lines.push((Licence::Heading, heading.into()));
        for licence in licences {
            lines.push((Licence::Text, String::new()));
            lines.push((Licence::Path, licence.rel_path.display().to_string()));
            for line in licence.text.lines() {
                // Tabs would be drawn one cell wide.
                let line = line.replace('\t', "    ");
                if line.width() <= LICENCE_WIDTH {
                    lines.push((Licence::Text, line));
                    continue;
                }
                let wrapped = markdown::wrap(vec![(Span::raw(line), None)], LICENCE_WIDTH);
                lines.extend(wrapped.into_iter().map(|pieces| {
                    let text = pieces.iter().map(|(span, _)| span.content.as_ref());
                    (Licence::Text, text.collect())
                }));
            }
        }
    }
    lines
});

const SPINNER: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

const HELP_ROWS: [(&[Action], &str); 11] = [
    (&[Action::MoveUp, Action::MoveDown], "move the cursor"),
    (
        &[Action::MoveUpOrScrollUp, Action::MoveDownOrScrollDown],
        "scroll changelog (when expanded)",
    ),
    (&[Action::ToggleExpand], "expand/collapse"),
    (&[Action::OpenHomepage], "open homepage"),
    (&[Action::OpenGithub], "open github"),
    (&[Action::OpenChangelog], "open changelog"),
    (&[Action::UpgradeAll], "upgrade all"),
    (&[Action::UpgradeCurrent], "upgrade current"),
    (&[Action::PickTheme], "pick theme"),
    (&[Action::Help], "help"),
    (&[Action::Quit], "quit"),
];

fn spinner(tick: usize, theme: &Theme) -> Span<'static> {
    Span::styled(SPINNER[tick % SPINNER.len()], theme.muted)
}

/// Lines `range` of the item list, building only the rows that fall in it.
pub fn lines_in(app: &App, range: Range<usize>) -> Vec<Line<'static>> {
    let mut lines = vec![];
    for ((index, item), &start) in app.items.iter().enumerate().zip(&app.item_lines) {
        if start >= range.end {
            break;
        }
        if range.contains(&start) {
            lines.push(row(app, index, item));
        }
        let body_start = start + 1;
        lines.extend(
            app.body_lines(index)
                .iter()
                .skip(range.start.saturating_sub(body_start))
                .take(range.end.saturating_sub(body_start.max(range.start)))
                .cloned(),
        );
    }
    lines
}

/// Links in `range`, with lines relative to its start.
pub fn links_in(app: &App, range: Range<usize>) -> Vec<Link> {
    let mut links = vec![];
    for (index, &start) in app.item_lines.iter().enumerate() {
        let body_start = start + 1;
        links.extend(app.body_links(index).iter().filter_map(|link| {
            let line = body_start + link.line;
            range.contains(&line).then(|| Link {
                line: line - range.start,
                ..link.clone()
            })
        }));
    }
    links
}

/// Links move past the top border and the margin, border and padding.
pub fn frame_body(
    mut content: Vec<Line<'static>>,
    links: Vec<Link>,
    loading: Option<&Stage>,
    tick: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, Vec<Link>) {
    let links = links
        .into_iter()
        .map(|link| Link {
            line: link.line + 1,
            columns: link.columns.start + 4..link.columns.end + 4,
            ..link
        })
        .collect();
    if let Some(stage) = loading {
        let text = match stage {
            Stage::Changelog => " checking changelog...".to_string(),
            Stage::File(filename) => format!(" checking {filename}..."),
            Stage::Releases => " fetching releases...".to_string(),
        };
        content.push(Line::from(vec![spinner(tick, theme), Span::raw(text)]));
    }
    if content.is_empty() {
        content.push(Line::styled("(empty)", theme.muted));
    }
    (frame(content, theme), links)
}

fn row(app: &App, index: usize, item: &Item) -> Line<'static> {
    let theme = &app.theme;
    let (mut cursor, mut version, mut hint, mut name) =
        (theme.accent, theme.version, theme.muted, Style::new());
    if item.upgrade == Upgrade::Done {
        (cursor, version, name) = (theme.muted, theme.muted, theme.muted);
    } else if index == app.cursor && !item.expanded {
        (cursor, version, hint, name) = (
            cursor.patch(theme.selected),
            version.patch(theme.selected),
            hint.patch(theme.selected),
            theme.selected,
        );
    }
    let source = &item.source;
    let source_hint = if source.has_repo() {
        format!("[{}/{}]", source.owner, source.repo)
    } else if !source.manual_url.is_empty() {
        "[known changelog]".into()
    } else {
        "[no changelog source]".into()
    };
    let mut spans = vec![
        Span::styled(if index == app.cursor { "> " } else { "  " }, cursor),
        Span::styled(format!("{} ({})", item.name, item.kind()), name),
        Span::raw("  "),
        Span::styled(item.installed.clone(), version),
        Span::raw(" -> "),
        Span::styled(item.current.clone(), version),
        Span::raw("  "),
        Span::styled(source_hint, hint),
    ];
    match item.upgrade {
        Upgrade::Idle => {}
        Upgrade::Running => {
            spans.extend([
                Span::raw("  "),
                spinner(app.spinner, theme),
                Span::raw(" upgrading..."),
            ]);
        }
        Upgrade::Done => spans.extend([Span::raw("  "), Span::styled("[upgraded]", theme.muted)]),
        Upgrade::Failed(_) => spans.extend([
            Span::raw("  "),
            Span::styled("[upgrade failed]", theme.error),
        ]),
    }
    Line::from(spans)
}

/// A rounded border with horizontal padding 1 and a left margin of 2.
fn frame(content: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    let width = content.iter().map(Line::width).max().unwrap_or(0);
    let border = theme.border;
    let edge = |left: &str, right: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{left}{}{right}", "─".repeat(width + 2)), border),
        ])
    };
    let mut framed = vec![edge("╭", "╮")];
    for line in content {
        let padding = width.saturating_sub(line.width());
        let mut spans = vec![Span::raw("  "), Span::styled("│ ", border)];
        spans.extend(line.spans);
        spans.extend([Span::raw(" ".repeat(padding)), Span::styled(" │", border)]);
        framed.push(Line::from(spans));
    }
    framed.push(edge("╰", "╯"));
    framed
}

/// Hints are labelled from the active keymap and dropped lowest-priority
/// first until they fit.
pub fn hints(app: &App) -> String {
    let item = app.items.get(app.cursor);
    let github = item.and_then(Item::github_url);
    let candidates = [
        (Action::ToggleExpand, "expand/collapse", 100, true),
        (
            Action::OpenHomepage,
            "open homepage",
            10,
            item.is_some_and(|item| {
                !item.homepage.is_empty() && Some(&item.homepage) != github.as_ref()
            }),
        ),
        (Action::OpenGithub, "open github", 20, github.is_some()),
        (
            Action::OpenChangelog,
            "open changelog",
            30,
            item.and_then(Item::changelog_url).is_some(),
        ),
        (Action::UpgradeAll, "upgrade all", 40, true),
        (
            Action::UpgradeCurrent,
            "upgrade current",
            50,
            item.is_some_and(|item| item.upgrade != Upgrade::Done),
        ),
        (Action::Help, "help", 15, true),
        (Action::Quit, "quit", 90, true),
    ];
    let mut shown: Vec<(String, u32)> = candidates
        .into_iter()
        .filter(|(.., show)| *show)
        .filter_map(|(action, label, priority, _)| {
            let key = app.keymap.hint_key(action)?;
            Some((format!("[{}] {label}", keys::glyph(key)), priority))
        })
        .collect();
    let joined = |hints: &[(String, u32)]| {
        hints
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join("  ")
    };
    while shown.len() > 1 && joined(&shown).width() > app.width as usize {
        let Some(lowest) = (0..shown.len()).min_by_key(|&index| shown[index].1) else {
            break;
        };
        shown.remove(lowest);
    }
    joined(&shown)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let [header, _, body, _, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(app.height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled("Outdated packages:", app.theme.header)),
        header,
    );
    let visible = lines_in(app, app.offset..app.offset + app.height as usize);
    frame.render_widget(Paragraph::new(visible), body);
    // A link cell's forced width would hide the popup cells it spans.
    if app.popup.is_none() {
        for link in links_in(app, app.offset..app.offset + app.height as usize) {
            hyperlink(frame.buffer_mut(), body, &link);
        }
    }
    let footer_line = match &app.notice {
        Some(notice) => Line::styled(notice.clone(), app.theme.error),
        None => Line::styled(hints(app), app.theme.muted),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);
    match &app.popup {
        None => {}
        Some(Popup::Help) => draw_popup(
            frame,
            app,
            "About",
            help_lines(app),
            Scroll::From(0),
            "[l] licences  press any key to close".into(),
        ),
        Some(Popup::Licences(offset)) => {
            let scroll =
                |action, default| keys::glyph(app.keymap.hint_key(action).unwrap_or(default));
            draw_popup(
                frame,
                app,
                "Licences",
                licence_lines(&app.theme),
                Scroll::From(*offset),
                format!(
                    "[{}/{}] scroll  [pgup/pgdown] page  [esc] close",
                    scroll(Action::MoveDownOrScrollDown, "j"),
                    scroll(Action::MoveUpOrScrollUp, "k"),
                ),
            );
        }
        Some(Popup::Theme(selected)) => {
            let keep = app.keymap.hint_key(Action::ToggleExpand).unwrap_or("enter");
            draw_popup(
                frame,
                app,
                "Theme",
                theme_picker_lines(app, *selected),
                Scroll::Reveal(*selected),
                format!("[{}] keep  [esc] cancel  (not saved)", keys::glyph(keep)),
            );
        }
        Some(Popup::ConfirmQuit) => draw_popup(
            frame,
            app,
            "Quit?",
            vec![
                Line::raw("An upgrade is still running;"),
                Line::raw("quitting now may interrupt it mid-install."),
            ],
            Scroll::From(0),
            "[y] quit  any other key: keep waiting".into(),
        ),
    }
}

/// Rewrites a link's cells as one OSC 8 hyperlink in its first cell. The
/// forced width keeps ratatui from counting the escape sequence as text.
fn hyperlink(buffer: &mut Buffer, area: Rect, link: &Link) {
    let offset = |value: usize| u16::try_from(value).unwrap_or(u16::MAX);
    let y = area.y.saturating_add(offset(link.line));
    let start = area.x.saturating_add(offset(link.columns.start));
    let end = area
        .x
        .saturating_add(offset(link.columns.end))
        .min(area.right());
    if y >= area.bottom() {
        return;
    }
    let mut text = String::new();
    let mut x = start;
    while x < end {
        let Some(cell) = buffer.cell((x, y)) else {
            return;
        };
        text.push_str(cell.symbol());
        x = x.saturating_add(offset(cell.symbol().width().max(1)));
    }
    let (Some(width), Some(cell)) = (NonZeroU16::new(x - start), buffer.cell_mut((start, y)))
    else {
        return;
    };
    cell.set_symbol(&format!("\x1b]8;;{}\x1b\\{text}\x1b]8;;\x1b\\", link.url))
        .set_diff_option(CellDiffOption::ForcedWidth(width));
}

/// Keys are read from the active keymap so overrides show up here.
fn help_lines(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let keys: Vec<String> = HELP_ROWS
        .iter()
        .map(|(actions, _)| {
            let mut keys: Vec<&str> = app
                .keymap
                .keys_for(actions)
                .into_iter()
                .map(keys::glyph)
                .collect();
            keys.sort();
            keys.join("/")
        })
        .collect();
    let key_width = keys.iter().map(|key| key.width()).max().unwrap_or(0);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("brew-changes", theme.header),
            Span::styled(format!(" {}", crate::VERSION), theme.muted),
        ]),
        Line::styled(
            "Interactive changelog viewer for outdated Homebrew packages",
            theme.muted,
        ),
        Line::raw(""),
        Line::raw("by Garth de Wet"),
        Line::raw("made with Claude Code"),
        Line::raw("https://github.com/garthdw/brew-changes-rs"),
        Line::raw("Syntaxes and themes from bat, via two-face"),
        Line::raw(""),
    ];
    for (key, (_, label)) in keys.iter().zip(HELP_ROWS) {
        let padding = " ".repeat(key_width.saturating_sub(key.width()));
        lines.push(Line::from(vec![
            Span::styled(format!("{key}{padding}"), theme.version),
            Span::raw(format!("  {label}")),
        ]));
    }
    lines
}

fn licence_lines(theme: &Theme) -> Vec<Line<'static>> {
    LICENCES
        .iter()
        .map(|(kind, text)| {
            let style = match kind {
                Licence::Heading => theme.header,
                Licence::Path => theme.heading,
                Licence::Text => Style::new(),
            };
            Line::styled(text.as_str(), style)
        })
        .collect()
}

fn theme_picker_lines(app: &App, selected: usize) -> Vec<Line<'static>> {
    app.themes
        .iter()
        .enumerate()
        .map(|(index, (name, _))| {
            if index == selected {
                Line::from(vec![
                    Span::styled("> ", app.theme.accent),
                    Span::styled(name.clone(), app.theme.selected),
                ])
            } else {
                Line::raw(format!("  {name}"))
            }
        })
        .collect()
}

/// Where a popup taller than the terminal scrolls to.
enum Scroll {
    /// As little as keeps this line in view.
    Reveal(usize),
    /// To this line, stopping once the last line is in view.
    From(usize),
}

/// Clamped to the terminal width minus 6, and kept above the footer so its
/// hints stay readable. The hint sits on the bottom border so it stays
/// visible however far the popup scrolls.
fn draw_popup(
    frame: &mut Frame,
    app: &App,
    title: &'static str,
    lines: Vec<Line<'static>>,
    scroll: Scroll,
    hint: String,
) {
    let area = frame.area();
    let area = Rect {
        height: area.height.saturating_sub(FOOTER_LINES),
        ..area
    };
    let content_width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .max(hint.width());
    let content_width = u16::try_from(content_width)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(6).max(1));
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(POPUP_CHROME);
    let popup = area.centered(
        Constraint::Length(content_width.saturating_add(6)),
        Constraint::Length(height),
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.border)
        .title(title)
        .title_bottom(Line::styled(hint, app.theme.muted))
        .padding(Padding::new(2, 2, 1, 1));
    let visible = usize::from(popup.height.saturating_sub(POPUP_CHROME));
    let scroll = match scroll {
        Scroll::Reveal(line) => (line + 1).saturating_sub(visible),
        Scroll::From(line) => line.min(lines.len().saturating_sub(visible)),
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::ui::app::Message;
    use crate::ui::keys::Keymap;
    use crate::ui::theme::ThemeConfig;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Modifier};

    fn app(items: Vec<Item>, width: u16) -> App {
        let mut app = App::new(items, Keymap::default(), ThemeConfig::default().themes(), 0);
        app.update(Message::Resize { width, height: 20 });
        app
    }

    fn item(name: &str) -> Item {
        Item {
            name: name.into(),
            installed: "1.0".into(),
            current: "2.0".into(),
            ..Default::default()
        }
    }

    fn text(app: &App) -> Vec<String> {
        lines_in(app, 0..app.line_count)
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    fn repo_source() -> Source {
        Source {
            owner: "o".into(),
            repo: "r".into(),
            ..Default::default()
        }
    }

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    #[test]
    fn rows_mark_cursor_and_frame_expanded_body() {
        let beta = Item {
            is_cask: true,
            expanded: true,
            body: Some("release notes".into()),
            source: repo_source(),
            ..item("beta")
        };
        let mut app = app(vec![item("alpha"), beta], 80);
        app.cursor = 1;
        app.refresh();
        assert_eq!(
            text(&app),
            [
                "  alpha (formula)  1.0 -> 2.0  [no changelog source]",
                "> beta (cask)  1.0 -> 2.0  [o/r]",
                "  ╭───────────────╮",
                "  │ release notes │",
                "  ╰───────────────╯",
            ]
        );
        assert_eq!(app.item_lines, [0, 1]);
    }

    #[test]
    fn lines_in_matches_slice_of_all_lines() {
        let expanded = |name| Item {
            expanded: true,
            body: Some("one\n\ntwo\n\nthree".into()),
            ..item(name)
        };
        let app = app(vec![expanded("a"), item("b"), expanded("c")], 80);
        let all = text(&app);
        assert_eq!(all.len(), app.line_count);
        for start in 0..all.len() {
            for end in start..=all.len() + 2 {
                let window: Vec<String> = lines_in(&app, start..end)
                    .iter()
                    .map(|line| line.to_string())
                    .collect();
                assert_eq!(window, all[start..end.min(all.len())], "{start}..{end}");
            }
        }
    }

    #[test]
    fn status_tags() {
        let upgraded = Item {
            upgrade: Upgrade::Done,
            expanded: true,
            body: Some("changelog body".into()),
            ..item("a")
        };
        let upgrading = Item {
            upgrade: Upgrade::Running,
            ..item("b")
        };
        let manual = Item {
            source: Source {
                manual_url: "https://m".into(),
                ..Default::default()
            },
            ..item("c")
        };
        let rendered = text(&app(vec![upgraded, upgrading, manual], 80)).join("\n");
        assert!(rendered.contains("[upgraded]") && rendered.contains("changelog body"));
        assert!(rendered.contains("⣾ upgrading..."));
        assert!(rendered.contains("[known changelog]"));
    }

    #[test]
    fn selected_row_is_bold_only_when_collapsed_and_not_upgraded() {
        let is_bold = |app: &App| {
            lines_in(app, 0..1)[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        };
        let mut app = app(vec![item("a")], 80);
        assert!(is_bold(&app));
        app.items[0].expanded = true;
        app.refresh();
        assert!(!is_bold(&app));
    }

    #[test]
    fn empty_body_shows_placeholder() {
        let app = app(
            vec![Item {
                expanded: true,
                body: Some(String::new()),
                ..item("a")
            }],
            80,
        );
        assert!(text(&app).join("\n").contains("(empty)"));
    }

    #[test]
    fn hints_follow_item_and_width() {
        let github_item = Item {
            source: repo_source(),
            homepage: "https://home".into(),
            ..item("a")
        };
        assert_eq!(
            hints(&app(vec![github_item.clone()], 200)),
            "[enter] expand/collapse  [o] open homepage  [g] open github  [c] open changelog  [a] upgrade all  [u] upgrade current  [?] help  [q] quit"
        );
        assert_eq!(
            hints(&app(vec![github_item], 60)),
            "[enter] expand/collapse  [u] upgrade current  [q] quit"
        );
        let upgraded = Item {
            upgrade: Upgrade::Done,
            ..item("b")
        };
        assert_eq!(
            hints(&app(vec![upgraded], 200)),
            "[enter] expand/collapse  [a] upgrade all  [?] help  [q] quit"
        );
    }

    #[test]
    fn hints_follow_keymap_overrides() {
        let mut app = app(vec![item("a")], 200);
        let (keymap, _) =
            Keymap::default().merge_json(r#"{"main": {"q": null, "x": "quit", "u": null}}"#);
        app.keymap = keymap;
        assert_eq!(
            hints(&app),
            "[enter] expand/collapse  [a] upgrade all  [?] help  [ctrl+c] quit"
        );
    }

    #[test]
    fn links_become_osc8_hyperlinks() {
        let mut app = app(
            vec![Item {
                expanded: true,
                body: Some("see [docs](https://d.io)".into()),
                ..item("a")
            }],
            80,
        );
        assert_eq!(
            links_in(&app, 0..app.line_count),
            [Link {
                line: 2,
                columns: 8..12,
                url: "https://d.io".into()
            }]
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        // Header and blank line, then the row and the top border.
        let cell = &terminal.backend().buffer()[(8, 4)];
        assert_eq!(
            cell.symbol(),
            "\x1b]8;;https://d.io\x1b\\docs\x1b]8;;\x1b\\"
        );
        app.update(Message::Key("?".into()));
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(!format!("{:?}", terminal.backend().buffer()).contains("\x1b]8"));
    }

    #[test]
    fn notice_replaces_hints() {
        let mut app = app(vec![item("a")], 80);
        app.update(Message::Notice("xdg-open: not found".into()));
        let rendered = screen(&app);
        assert!(rendered.contains("xdg-open: not found"));
        assert!(!rendered.contains("[q] quit"));
    }

    #[test]
    fn draws_layout_and_help() {
        let mut app = app(vec![item("alpha")], 80);
        let rendered = screen(&app);
        assert!(rendered.contains("Outdated packages:"));
        assert!(rendered.contains("> alpha (formula)"));
        app.update(Message::Key("?".into()));
        let rendered = screen(&app);
        assert!(rendered.contains("About") && rendered.contains("press any key to close"));
        assert!(rendered.contains(&format!("brew-changes {}", env!("CARGO_PKG_VERSION"))));
        assert!(rendered.contains("J/K/↑/↓"));
        assert!(rendered.contains("pick theme"));
    }

    #[test]
    fn help_credits_two_face_and_opens_licences() {
        let mut app = app(vec![item("alpha")], 80);
        app.update(Message::Key("?".into()));
        let rendered = screen(&app);
        assert!(rendered.contains("two-face"), "{rendered}");
        assert!(rendered.contains("[l] licences"), "{rendered}");
        app.update(Message::Key("l".into()));
        let rendered = screen(&app);
        assert!(rendered.contains("Licences"), "{rendered}");
        assert!(rendered.contains("Syntaxes"), "{rendered}");
        assert!(rendered.contains("[esc] close"), "{rendered}");
    }

    #[test]
    fn licence_lines_fit_the_fixed_width() {
        assert!(LICENCES.len() > 1000);
        assert!(
            LICENCES
                .iter()
                .all(|(_, line)| line.width() <= LICENCE_WIDTH && !line.contains('\t'))
        );
    }

    #[test]
    fn header_and_help_name_use_theme_header() {
        let mut app = app(vec![item("alpha")], 80);
        app.theme.header = Style::new().fg(Color::Magenta);
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Magenta);
        let name = help_lines(&app)[0].spans[0].style;
        assert_eq!(name, app.theme.header);
    }

    #[test]
    fn draws_theme_picker() {
        let mut app = app(vec![item("alpha")], 80);
        app.update(Message::Key("t".into()));
        app.update(Message::Key("j".into()));
        let rendered = screen(&app);
        assert!(rendered.contains("Theme"), "{rendered}");
        assert!(rendered.contains("  base16-ocean.dark"), "{rendered}");
        assert!(rendered.contains("> base16-eighties.dark"), "{rendered}");
        assert!(
            rendered.contains("[enter] keep  [esc] cancel"),
            "{rendered}"
        );
    }

    #[test]
    fn theme_picker_scrolls_to_selection_on_short_terminals() {
        let mut app = app(vec![item("alpha")], 80);
        app.update(Message::Key("t".into()));
        for _ in 1..app.themes.len() {
            app.update(Message::Key("j".into()));
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        let last = app.themes.last().map(|(name, _)| name.as_str());
        assert!(
            last.is_some_and(|last| rendered.contains(&format!("> {last}"))),
            "{rendered}"
        );
        let buffer = terminal.backend().buffer();
        let row = |y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(7).contains("keep"), "{rendered}");
        assert_eq!(row(8).trim(), "", "{rendered}");
        assert!(row(9).starts_with("[enter] expand/collapse"), "{rendered}");
    }
}
