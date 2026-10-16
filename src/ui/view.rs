use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::app::{App, Item};
use super::keys::{self, Action};

pub const ACCENT: Color = Color::Rgb(0x00, 0xD7, 0x87);
pub const BORDER: Color = Color::Rgb(0x58, 0x58, 0x58);
pub const MUTED: Color = Color::Rgb(0x58, 0x58, 0x58);
pub const ERROR: Color = Color::Rgb(0xFF, 0x00, 0x00);
pub const VERSION: Color = Color::Rgb(0xFF, 0xAF, 0x00);

const SPINNER: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

const HELP_ROWS: [(&[Action], &str); 10] = [
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
    (&[Action::Help], "help"),
    (&[Action::Quit], "quit"),
];

pub fn dim() -> Style {
    Style::new().fg(MUTED)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn spinner(app: &App) -> Span<'static> {
    Span::styled(SPINNER[app.spinner % SPINNER.len()], dim())
}

/// Also returns each item's header line offset, for scrolling it into view.
pub fn list_lines<'a>(
    app: &App,
    body_for: impl Fn(usize) -> Option<&'a [Line<'static>]>,
) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = vec![];
    let mut item_lines = Vec::with_capacity(app.items.len());
    for (index, item) in app.items.iter().enumerate() {
        item_lines.push(lines.len());
        lines.push(row(app, index, item));
        if !item.expanded {
            continue;
        }
        let mut content: Vec<Line<'static>> =
            body_for(index).map(<[_]>::to_vec).unwrap_or_default();
        if let Some(stage) = &item.loading_stage {
            let text = match stage.as_str() {
                "releases" => " fetching releases...".to_string(),
                stage => format!(" checking {stage}..."),
            };
            content.push(Line::from(vec![spinner(app), Span::raw(text)]));
        }
        if content.is_empty() {
            content.push(Line::styled("(empty)", dim()));
        }
        lines.extend(frame(content));
    }
    (lines, item_lines)
}

fn row(app: &App, index: usize, item: &Item) -> Line<'static> {
    let selected = index == app.cursor && !item.expanded && !item.upgraded;
    let (mut cursor, mut version, mut hint, mut name) = (
        Style::new().fg(ACCENT),
        Style::new().fg(VERSION),
        dim(),
        Style::new(),
    );
    if item.upgraded {
        (cursor, version, name) = (dim(), dim(), dim());
    } else if selected {
        (cursor, version, hint, name) = (
            cursor.patch(bold()),
            version.patch(bold()),
            hint.patch(bold()),
            bold(),
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
    if item.upgraded {
        spans.extend([Span::raw("  "), Span::styled("[upgraded]", dim())]);
    } else if item.upgrading {
        spans.extend([Span::raw("  "), spinner(app), Span::raw(" upgrading...")]);
    } else if item.upgrade_error.is_some() {
        spans.extend([
            Span::raw("  "),
            Span::styled("[upgrade failed]", Style::new().fg(ERROR)),
        ]);
    }
    Line::from(spans)
}

/// A rounded border with horizontal padding 1 and a left margin of 2.
fn frame(content: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let width = content.iter().map(Line::width).max().unwrap_or(0);
    let border = Style::new().fg(BORDER);
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
            item.is_some_and(|item| !item.upgraded),
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
    if !app.ready {
        frame.render_widget(Paragraph::new("Loading..."), area);
        return;
    }
    let [header, _, body, _, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(app.height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled("Outdated packages:", bold())),
        header,
    );
    let end = (app.offset + app.height as usize).min(app.lines.len());
    let visible = app.lines.get(app.offset..end).unwrap_or_default().to_vec();
    frame.render_widget(Paragraph::new(visible), body);
    let footer_line = match &app.notice {
        Some(notice) => Line::styled(notice.clone(), Style::new().fg(ERROR)),
        None => Line::styled(hints(app), dim()),
    };
    frame.render_widget(Paragraph::new(footer_line), footer);
    if app.show_help {
        draw_help(frame, app);
    }
}

/// Keys are read from the active keymap so overrides show up here.
fn help_lines(app: &App) -> Vec<Line<'static>> {
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
        Line::styled("brew-changes", bold()),
        Line::styled(
            "Interactive changelog viewer for outdated Homebrew packages",
            dim(),
        ),
        Line::raw(""),
        Line::raw("by Garth de Wet"),
        Line::raw("made with Claude Code"),
        Line::raw("https://github.com/garthdw/brew-changes"),
        Line::raw(""),
    ];
    for (key, (_, label)) in keys.iter().zip(HELP_ROWS) {
        let padding = " ".repeat(key_width.saturating_sub(key.width()));
        lines.push(Line::from(vec![
            Span::styled(format!("{key}{padding}"), Style::new().fg(VERSION)),
            Span::raw(format!("  {label}")),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("press any key to close", dim()));
    lines
}

/// Clamped to the terminal width minus 6.
fn draw_help(frame: &mut Frame, app: &App) {
    let lines = help_lines(app);
    let area = frame.area();
    let content_width = lines.iter().map(Line::width).max().unwrap_or(0);
    let content_width = u16::try_from(content_width)
        .unwrap_or(u16::MAX)
        .min(area.width.saturating_sub(6).max(1));
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let width = content_width.saturating_add(6);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title("About")
        .padding(Padding::new(2, 2, 1, 1));
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::Source;
    use crate::ui::app::Message;
    use crate::ui::keys::Keymap;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app(items: Vec<Item>, width: u16) -> App {
        let mut app = App::new(items, Keymap::default(), true);
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
        app.lines.iter().map(|line| line.to_string()).collect()
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
    fn status_tags() {
        let upgraded = Item {
            upgraded: true,
            expanded: true,
            body: Some("changelog body".into()),
            ..item("a")
        };
        let upgrading = Item {
            upgrading: true,
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
            app.lines[0].spans[1]
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
            upgraded: true,
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
        assert!(rendered.contains("J/K/↑/↓"));
    }
}
