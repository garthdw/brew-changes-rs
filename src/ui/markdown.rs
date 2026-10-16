use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::view::{bold, dim};

struct Palette {
    heading: Style,
    title: Style,
    code: Style,
    code_block: Style,
    link: Style,
    dim: Style,
}

impl Palette {
    fn new(dark: bool) -> Self {
        let (heading, code_background, block_foreground, link) = if dark {
            (39, 236, 250, 33)
        } else {
            (27, 254, 238, 27)
        };
        Self {
            heading: Style::new()
                .fg(Color::Indexed(heading))
                .add_modifier(Modifier::BOLD),
            title: Style::new()
                .fg(Color::Indexed(228))
                .bg(Color::Indexed(63))
                .add_modifier(Modifier::BOLD),
            code: Style::new()
                .fg(Color::Indexed(203))
                .bg(Color::Indexed(code_background)),
            code_block: Style::new().fg(Color::Indexed(block_foreground)),
            link: Style::new()
                .fg(Color::Indexed(link))
                .add_modifier(Modifier::UNDERLINED),
            dim: dim(),
        }
    }
}

pub fn render(markdown: &str, width: usize, dark: bool) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(width.max(10), Palette::new(dark));
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.flush();
    while renderer.lines.last().is_some_and(|line| line.width() == 0) {
        renderer.lines.pop();
    }
    renderer.lines
}

enum Container {
    Quote,
    List(Option<u64>),
    /// Continuation indent width of a list item.
    Item(usize),
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    header_rows: usize,
}

struct Renderer {
    width: usize,
    palette: Palette,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// A list item's marker, waiting for its first line.
    bullet: Option<String>,
    need_gap: bool,
    links: Vec<(String, usize)>,
    code_block: Option<String>,
    table: Option<Table>,
}

impl Renderer {
    fn new(width: usize, palette: Palette) -> Self {
        Self {
            width,
            palette,
            lines: vec![],
            spans: vec![],
            styles: vec![],
            containers: vec![],
            bullet: None,
            need_gap: false,
            links: vec![],
            code_block: None,
            table: None,
        }
    }

    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::new(), |combined, style| combined.patch(*style))
    }

    fn push_text(&mut self, text: &str) {
        if let Some(code) = &mut self.code_block {
            code.push_str(text);
        } else if let Some(cell) = self
            .table
            .as_mut()
            .and_then(|table| table.rows.last_mut())
            .and_then(|row| row.last_mut())
        {
            cell.push_str(text);
        } else {
            self.spans
                .push(Span::styled(text.to_string(), self.style()));
        }
    }

    fn start_block(&mut self) {
        self.flush();
        if self.need_gap {
            let prefix = self.prefix(false);
            self.lines.push(Line::from(prefix));
            self.need_gap = false;
        }
    }

    fn end_block(&mut self) {
        self.flush();
        self.need_gap = true;
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text),
            Event::Code(code) => {
                if self.table.is_some() {
                    self.push_text(&code);
                } else {
                    let style = self.style().patch(self.palette.code);
                    self.spans.push(Span::styled(code.to_string(), style));
                }
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.spans.push(Span::raw("\n")),
            // Release notes often use <br> for line breaks; other HTML is dropped.
            Event::InlineHtml(html)
                if html.trim_start().to_ascii_lowercase().starts_with("<br") =>
            {
                self.spans.push(Span::raw("\n"));
            }
            Event::Rule => {
                self.start_block();
                let rule = "─".repeat(self.width.saturating_sub(self.prefix_width()));
                let mut spans = self.prefix(false);
                spans.push(Span::styled(rule, self.palette.dim));
                self.lines.push(Line::from(spans));
                self.need_gap = true;
            }
            Event::TaskListMarker(done) => self.push_text(if done { "[x] " } else { "[ ] " }),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph | Tag::Heading { .. } => self.start_block(),
            Tag::BlockQuote(_) => {
                self.start_block();
                self.containers.push(Container::Quote);
            }
            Tag::CodeBlock(_) => {
                self.start_block();
                self.code_block = Some(String::new());
            }
            Tag::List(start) => {
                // A nested list continues its parent item without a gap.
                if self.in_item() {
                    self.flush();
                } else {
                    self.start_block();
                }
                self.containers.push(Container::List(start));
            }
            Tag::Item => {
                self.start_block();
                let marker = match self.containers.last_mut() {
                    Some(Container::List(Some(number))) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_string(),
                };
                self.containers.push(Container::Item(marker.width()));
                self.bullet = Some(marker);
            }
            Tag::Table(_) => {
                self.start_block();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(vec![]);
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|table| table.rows.last_mut()) {
                    row.push(String::new());
                }
            }
            Tag::Emphasis => self
                .styles
                .push(Style::new().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(Style::new().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::new().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(self.palette.link);
                self.links.push((dest_url.to_string(), self.spans.len()));
            }
            Tag::Image { .. } => {
                self.styles.push(self.palette.dim);
                self.push_text("[image: ");
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_block(),
            TagEnd::Heading(level) => self.heading(level),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.containers.pop();
                self.need_gap = true;
            }
            TagEnd::CodeBlock => self.code_block_end(),
            TagEnd::List(_) => {
                self.flush();
                self.containers.pop();
                self.need_gap = !self.in_item();
            }
            TagEnd::Item => {
                self.flush();
                self.containers.pop();
                self.bullet = None;
            }
            TagEnd::Table => self.table_end(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((url, start)) = self.links.pop() {
                    let text: String = self
                        .spans
                        .get(start..)
                        .unwrap_or_default()
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect();
                    // Issue, PR and user references would otherwise repeat
                    // long GitHub URLs on nearly every release-note line.
                    let is_reference = text.starts_with('#') || text.starts_with('@');
                    if !url.is_empty() && !url.starts_with('#') && text != url && !is_reference {
                        self.spans
                            .push(Span::styled(format!(" ({url})"), self.palette.dim));
                    }
                }
            }
            TagEnd::Image => {
                self.push_text("]");
                self.styles.pop();
            }
            _ => {}
        }
    }

    fn in_item(&self) -> bool {
        self.containers
            .iter()
            .any(|container| matches!(container, Container::Item(_)))
    }

    fn heading(&mut self, level: HeadingLevel) {
        let style = if level == HeadingLevel::H1 {
            self.palette.title
        } else {
            self.palette.heading
        };
        let mut spans = std::mem::take(&mut self.spans);
        for span in &mut spans {
            span.style = span.style.patch(style);
        }
        // "##" markers are kept so version headings read like the source file.
        let marker = match level {
            HeadingLevel::H1 => " ".to_string(),
            level => format!("{} ", "#".repeat(level as usize)),
        };
        spans.insert(0, Span::styled(marker, style));
        if level == HeadingLevel::H1 {
            spans.push(Span::styled(" ", style));
        }
        self.spans = spans;
        self.end_block();
    }

    fn code_block_end(&mut self) {
        let code = self.code_block.take().unwrap_or_default();
        let available = self.width.saturating_sub(self.prefix_width() + 2).max(1);
        for line in code.trim_end_matches('\n').split('\n') {
            let line = line.replace('\t', "    ");
            for chunk in split_by_width(&line, available) {
                let mut spans = self.prefix(false);
                spans.push(Span::raw("  "));
                spans.push(Span::styled(chunk, self.palette.code_block));
                self.lines.push(Line::from(spans));
            }
        }
        self.need_gap = true;
    }

    fn table_end(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let available = self
            .width
            .saturating_sub(self.prefix_width() + 3 * (columns - 1));
        let column_cap = (available / columns).max(4);
        let widths: Vec<usize> = (0..columns)
            .map(|column| {
                table
                    .rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| cell.trim().width())
                    .max()
                    .unwrap_or(0)
                    .min(column_cap)
            })
            .collect();
        for (row_index, row) in table.rows.iter().enumerate() {
            let style = if row_index < table.header_rows {
                bold()
            } else {
                Style::new()
            };
            let mut spans = self.prefix(false);
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", self.palette.dim));
                }
                let cell = row.get(column).map_or("", |cell| cell.trim());
                spans.push(Span::styled(pad(&truncate(cell, *width), *width), style));
            }
            self.lines.push(Line::from(spans));
            if row_index + 1 == table.header_rows {
                let separators: Vec<String> =
                    widths.iter().map(|width| "─".repeat(*width)).collect();
                let mut spans = self.prefix(false);
                spans.push(Span::styled(separators.join("─┼─"), self.palette.dim));
                self.lines.push(Line::from(spans));
            }
        }
        self.need_gap = true;
    }

    fn prefix_width(&self) -> usize {
        self.containers
            .iter()
            .map(|container| match container {
                Container::Quote => 2,
                Container::List(_) => 0,
                Container::Item(width) => *width,
            })
            .sum()
    }

    /// `first` puts a pending list marker on the line.
    fn prefix(&self, first: bool) -> Vec<Span<'static>> {
        let last_item = self
            .containers
            .iter()
            .rposition(|container| matches!(container, Container::Item(_)));
        self.containers
            .iter()
            .enumerate()
            .filter_map(|(index, container)| match container {
                Container::Quote => Some(Span::styled("│ ", self.palette.dim)),
                Container::List(_) => None,
                Container::Item(width) => match (&self.bullet, first && Some(index) == last_item) {
                    (Some(bullet), true) => {
                        Some(Span::styled(bullet.clone(), self.palette.heading))
                    }
                    _ => Some(Span::raw(" ".repeat(*width))),
                },
            })
            .collect()
    }

    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let available = self.width.saturating_sub(self.prefix_width()).max(1);
        for (index, line) in wrap(spans, available).into_iter().enumerate() {
            let mut full = self.prefix(index == 0);
            full.extend(line);
            self.lines.push(Line::from(full));
        }
        self.bullet = None;
    }
}

/// Greedy word wrap. "\n" spans force a break, whitespace runs collapse to
/// one space, and words wider than `width` are split.
fn wrap(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines = vec![];
    let mut line: Vec<Span<'static>> = vec![];
    let mut line_width = 0;
    let mut pending_space: Option<Style> = None;

    for span in spans {
        if span.content == "\n" {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
            pending_space = None;
            continue;
        }
        for (is_space, piece) in split_words(&span.content) {
            if is_space {
                if line_width > 0 {
                    pending_space = Some(span.style);
                }
                continue;
            }
            let piece_width = piece.width();
            let gap = usize::from(pending_space.is_some());
            if line_width > 0 && line_width + gap + piece_width > width {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
                pending_space = None;
            }
            if let Some(style) = pending_space.take() {
                line.push(Span::styled(" ", style));
                line_width += 1;
            }
            if piece_width <= width {
                line.push(Span::styled(piece.to_string(), span.style));
                line_width += piece_width;
                continue;
            }
            for chunk in split_by_width(piece, width) {
                if line_width > 0 {
                    lines.push(std::mem::take(&mut line));
                }
                line_width = chunk.width();
                line.push(Span::styled(chunk, span.style));
            }
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Alternating runs of whitespace and non-whitespace, tagged with whether
/// each is whitespace.
fn split_words(text: &str) -> Vec<(bool, &str)> {
    let mut pieces = vec![];
    let mut start = 0;
    let mut previous: Option<bool> = None;
    for (index, character) in text.char_indices() {
        let is_space = character.is_whitespace();
        if let Some(previous_is_space) = previous
            && previous_is_space != is_space
        {
            pieces.push((previous_is_space, &text[start..index]));
            start = index;
        }
        previous = Some(is_space);
    }
    if let Some(previous_is_space) = previous {
        pieces.push((previous_is_space, &text[start..]));
    }
    pieces
}

fn split_by_width(text: &str, width: usize) -> Vec<String> {
    let mut chunks = vec![];
    let mut chunk = String::new();
    let mut chunk_width = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if chunk_width + character_width > width && chunk_width > 0 {
            chunks.push(std::mem::take(&mut chunk));
            chunk_width = 0;
        }
        chunk.push(character);
        chunk_width += character_width;
    }
    chunks.push(chunk);
    chunks
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut truncated = split_by_width(text, width.saturating_sub(1))
        .into_iter()
        .next()
        .unwrap_or_default();
    truncated.push('…');
    truncated
}

fn pad(text: &str, width: usize) -> String {
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(md: &str, width: usize) -> Vec<String> {
        render(md, width, true)
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn headings_paragraphs_and_gaps() {
        assert_eq!(
            text("## 1.2.0\n\nFixed a bug.\n\nAnother.", 40),
            ["## 1.2.0", "", "Fixed a bug.", "", "Another."]
        );
    }

    #[test]
    fn wraps_words_and_preserves_styles() {
        let lines = render("aaa **bbb** ccc ddd", 8, true);
        let rendered: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(rendered, ["aaa bbb", "ccc ddd"]);
        let bold = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "bbb")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn long_words_are_split() {
        assert_eq!(text("abcdefghijkl", 10), ["abcdefghij", "kl"]);
    }

    #[test]
    fn tight_and_nested_lists() {
        assert_eq!(
            text("- one\n- two that wraps here\n  - nested\n1. first", 16),
            [
                "• one",
                "• two that wraps",
                "  here",
                "  • nested",
                "",
                "1. first"
            ]
        );
    }

    #[test]
    fn code_block_and_inline_code() {
        assert_eq!(
            text("Run `x`:\n\n```\nfoo\n  bar\n```", 40),
            ["Run x:", "", "  foo", "    bar"]
        );
    }

    #[test]
    fn links_show_url_unless_reference() {
        assert_eq!(
            text("[docs](https://d.io) and [#12](https://gh/pull/12)", 80),
            ["docs (https://d.io) and #12"]
        );
    }

    #[test]
    fn quotes_rules_tables_and_html() {
        assert_eq!(text("> quoted", 40), ["│ quoted"]);
        assert_eq!(text("---", 12), ["─".repeat(12)]);
        assert_eq!(
            text("| a | bb |\n|---|---|\n| 1 | 2 |", 40),
            ["a │ bb", "──┼───", "1 │ 2 "]
        );
        assert_eq!(
            text("<!-- hidden -->\n\nshown<br>next", 40),
            ["shown", "next"]
        );
    }
}
