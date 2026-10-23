use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ops::Range;
use std::sync::{LazyLock, Mutex};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter};
use syntect::parsing::{ParseState, ScopeStack, ScopeStackOp, SyntaxSet};
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::{SYNTAX_THEMES, Theme, terminal_color};

/// A link's columns on one rendered line; a wrapped link has one per line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub line: usize,
    pub columns: Range<usize>,
    pub url: String,
}

/// A span and the index of the URL it links to, if any.
type Piece = (Span<'static>, Option<usize>);

pub fn render(markdown: &str, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Vec<Link>) {
    let mut renderer = Renderer::new(width.max(10), *theme);
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        renderer.event(event);
    }
    renderer.flush();
    while renderer.lines.last().is_some_and(|line| line.width() == 0) {
        renderer.lines.pop();
    }
    (renderer.lines, renderer.links)
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
    theme: Theme,
    lines: Vec<Line<'static>>,
    spans: Vec<Piece>,
    styles: Vec<Style>,
    containers: Vec<Container>,
    /// A list item's marker, waiting for its first line.
    bullet: Option<String>,
    need_gap: bool,
    urls: Vec<String>,
    /// The `urls` index of the link being rendered.
    link: Option<usize>,
    links: Vec<Link>,
    code_block: Option<String>,
    code_language: Option<String>,
    table: Option<Table>,
}

impl Renderer {
    fn new(width: usize, theme: Theme) -> Self {
        Self {
            width,
            theme,
            lines: vec![],
            spans: vec![],
            styles: vec![],
            containers: vec![],
            bullet: None,
            need_gap: false,
            urls: vec![],
            link: None,
            links: vec![],
            code_block: None,
            code_language: None,
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
                .push((Span::styled(text.to_string(), self.style()), self.link));
        }
    }

    /// Table cells are single lines, so breaks in them become spaces.
    fn line_break(&mut self) {
        if self.table.is_some() {
            self.push_text(" ");
        } else {
            self.spans.push((Span::raw("\n"), None));
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
                    let style = self.style().patch(self.theme.code);
                    self.spans
                        .push((Span::styled(code.to_string(), style), self.link));
                }
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.line_break(),
            // Release notes often use <br> for line breaks; other HTML is dropped.
            Event::InlineHtml(html)
                if html.trim_start().to_ascii_lowercase().starts_with("<br") =>
            {
                self.line_break();
            }
            Event::Rule => {
                self.start_block();
                let rule = "─".repeat(self.width.saturating_sub(self.prefix_width()));
                let mut spans = self.first_prefix();
                spans.push(Span::styled(rule, self.theme.muted));
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
            Tag::CodeBlock(kind) => {
                self.start_block();
                self.code_block = Some(String::new());
                self.code_language = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().map(str::to_string)
                    }
                    CodeBlockKind::Indented => None,
                };
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
                self.styles.push(self.theme.link);
                // Relative and anchor links lead nowhere from a terminal.
                if dest_url.contains("://") {
                    // A control character would end the OSC 8 sequence early.
                    self.urls
                        .push(dest_url.chars().filter(|c| !c.is_control()).collect());
                    self.link = Some(self.urls.len() - 1);
                }
            }
            Tag::Image { .. } => {
                self.styles.push(self.theme.muted);
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
                self.link = None;
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
            self.theme.title
        } else {
            self.theme.heading
        };
        let mut spans = std::mem::take(&mut self.spans);
        for (span, _) in &mut spans {
            span.style = span.style.patch(style);
        }
        // "##" markers are kept so version headings read like the source file.
        let marker = match level {
            HeadingLevel::H1 => " ".to_string(),
            level => format!("{} ", "#".repeat(level as usize)),
        };
        spans.insert(0, (Span::styled(marker, style), None));
        if level == HeadingLevel::H1 {
            spans.push((Span::styled(" ", style), None));
        }
        self.spans = spans;
        self.end_block();
    }

    fn code_block_end(&mut self) {
        let code = self.code_block.take().unwrap_or_default();
        let code = code.trim_end_matches('\n').replace('\t', "    ");
        let language = self.code_language.take();
        let source_lines = language
            .and_then(|language| highlight(&code, &language, self.theme.syntax))
            .unwrap_or_else(|| {
                code.split('\n')
                    .map(|line| vec![Span::styled(line.to_string(), self.theme.code_block)])
                    .collect()
            });
        let available = self.width.saturating_sub(self.prefix_width() + 2).max(1);
        for source_line in source_lines {
            for chunk in split_spans_by_width(source_line, available) {
                let mut spans = self.first_prefix();
                spans.push(Span::raw("  "));
                spans.extend(chunk);
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
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let mut spans = self.first_prefix();
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", self.theme.muted));
                }
                let cell = row.get(column).map_or("", |cell| cell.trim());
                spans.push(Span::styled(pad(&truncate(cell, *width), *width), style));
            }
            self.lines.push(Line::from(spans));
            if row_index + 1 == table.header_rows {
                let separators: Vec<String> =
                    widths.iter().map(|width| "─".repeat(*width)).collect();
                let mut spans = self.prefix(false);
                spans.push(Span::styled(separators.join("─┼─"), self.theme.muted));
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
                Container::Quote => Some(Span::styled("│ ", self.theme.muted)),
                Container::List(_) => None,
                Container::Item(width) => match (&self.bullet, first && Some(index) == last_item) {
                    (Some(bullet), true) => Some(Span::styled(bullet.clone(), self.theme.heading)),
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
        for pieces in wrap(spans, available) {
            let mut full = self.first_prefix();
            let mut column: usize = full.iter().map(Span::width).sum();
            for (span, link) in pieces {
                let columns = column..column + span.width();
                column = columns.end;
                full.push(span);
                let Some(url) = link.and_then(|link| self.urls.get(link)) else {
                    continue;
                };
                let line = self.lines.len();
                match self.links.last_mut() {
                    Some(last)
                        if last.line == line
                            && last.columns.end == columns.start
                            && last.url == *url =>
                    {
                        last.columns.end = columns.end;
                    }
                    _ => self.links.push(Link {
                        line,
                        columns,
                        url: url.clone(),
                    }),
                }
            }
            self.lines.push(Line::from(full));
        }
    }

    /// The prefix for a block's next line, consuming any pending list marker
    /// so it lands on the item's first line whatever block starts it.
    fn first_prefix(&mut self) -> Vec<Span<'static>> {
        let prefix = self.prefix(true);
        self.bullet = None;
        prefix
    }
}

/// Greedy word wrap. "\n" spans force a break, whitespace runs collapse to
/// one space, and words wider than `width` are split.
pub(super) fn wrap(spans: Vec<Piece>, width: usize) -> Vec<Vec<Piece>> {
    let mut lines = vec![];
    let mut line: Vec<Piece> = vec![];
    let mut line_width = 0;
    let mut pending_space: Option<(Style, Option<usize>)> = None;

    for (span, link) in spans {
        if span.content == "\n" {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
            pending_space = None;
            continue;
        }
        for (is_space, piece) in split_words(&span.content) {
            if is_space {
                if line_width > 0 {
                    pending_space = Some((span.style, link));
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
            if let Some((style, space_link)) = pending_space.take() {
                line.push((Span::styled(" ", style), space_link));
                line_width += 1;
            }
            if piece_width <= width {
                line.push((Span::styled(piece.to_string(), span.style), link));
                line_width += piece_width;
                continue;
            }
            for chunk in split_by_width(piece, width) {
                if line_width > 0 {
                    lines.push(std::mem::take(&mut line));
                }
                line_width = chunk.width();
                line.push((Span::styled(chunk, span.style), link));
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

// Loaded on first use so the list still appears instantly at startup.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Enough for every code block of many expanded bodies, so re-rendering them
/// all for a theme preview doesn't evict its own blocks.
const PARSE_CACHE_ENTRIES: usize = 512;

/// Scope operations per line of a code block.
type Operations = Vec<Vec<(usize, ScopeStackOp)>>;

/// Parsing is nearly all of highlighting's cost and doesn't depend on the
/// theme, so a theme change or resize only restyles. Blocks are kept by
/// frecency, so a long session's memory stays bounded while blocks of bodies
/// still being viewed are kept.
struct ParseCache {
    /// By syntax name and code.
    entries: HashMap<(String, String), Cached>,
    clock: u64,
    capacity: usize,
}

struct Cached {
    operations: Operations,
    uses: u32,
    last_used: u64,
}

impl ParseCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            capacity,
        }
    }

    fn operations(
        &mut self,
        key: (String, String),
        parse: impl FnOnce() -> Option<Operations>,
    ) -> Option<&Operations> {
        self.clock += 1;
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.evict();
        }
        let cached = match self.entries.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Cached {
                operations: parse()?,
                uses: 0,
                last_used: 0,
            }),
        };
        cached.uses = cached.uses.saturating_add(1);
        cached.last_used = self.clock;
        Some(&cached.operations)
    }

    /// Drops the entry with the fewest uses per tick since it was last used.
    fn evict(&mut self) {
        let clock = self.clock;
        let frecency =
            |cached: &Cached| f64::from(cached.uses) / (clock - cached.last_used + 1) as f64;
        let least = self
            .entries
            .iter()
            .min_by(|(_, left), (_, right)| frecency(left).total_cmp(&frecency(right)))
            .map(|(key, _)| key.clone());
        if let Some(key) = least {
            self.entries.remove(&key);
        }
    }
}

static PARSED: LazyLock<Mutex<ParseCache>> =
    LazyLock::new(|| Mutex::new(ParseCache::new(PARSE_CACHE_ENTRIES)));

/// None for an unknown language or a highlighting failure, so the caller
/// falls back to plain code styling.
fn highlight(code: &str, language: &str, theme_name: &str) -> Option<Vec<Vec<Span<'static>>>> {
    let syntax = SYNTAXES.find_syntax_by_token(language)?;
    let theme = SYNTAX_THEMES.themes.get(theme_name)?;
    let mut parsed = PARSED.lock().ok()?;
    let operations = parsed.operations((syntax.name.clone(), code.to_string()), || {
        let mut state = ParseState::new(syntax);
        LinesWithEndings::from(code)
            .map(|line| state.parse_line(line, &SYNTAXES).ok())
            .collect()
    })?;
    let highlighter = Highlighter::new(theme);
    let mut state = HighlightState::new(&highlighter, ScopeStack::new());
    let lines = LinesWithEndings::from(code)
        .zip(operations.iter())
        .map(|(line, operations)| {
            HighlightIterator::new(&mut state, operations, line, &highlighter)
                .map(|(style, text)| {
                    // The theme's background is ignored so blocks sit on the
                    // terminal's own background.
                    let span_style = Style::new()
                        .fg(terminal_color(style.foreground))
                        .add_modifier(modifiers(style.font_style));
                    Span::styled(text.trim_end_matches('\n').to_string(), span_style)
                })
                .collect()
        })
        .collect();
    Some(lines)
}

fn modifiers(font_style: FontStyle) -> Modifier {
    [
        (FontStyle::BOLD, Modifier::BOLD),
        (FontStyle::ITALIC, Modifier::ITALIC),
        (FontStyle::UNDERLINE, Modifier::UNDERLINED),
    ]
    .into_iter()
    .filter(|(font, _)| font_style.contains(*font))
    .fold(Modifier::empty(), |all, (_, modifier)| all | modifier)
}

/// Hard-wraps one line of styled spans, keeping each piece's style.
fn split_spans_by_width(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines = vec![vec![]];
    let mut line_width = 0;
    for span in spans {
        let mut piece = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if line_width + character_width > width && line_width > 0 {
                if let Some(line) = lines.last_mut()
                    && !piece.is_empty()
                {
                    line.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                lines.push(vec![]);
                line_width = 0;
            }
            piece.push(character);
            line_width += character_width;
        }
        if let Some(line) = lines.last_mut()
            && !piece.is_empty()
        {
            line.push(Span::styled(piece, span.style));
        }
    }
    lines
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
    use ratatui::style::Color;
    use syntect::easy::HighlightLines;

    fn text(md: &str, width: usize) -> Vec<String> {
        render(md, width, &Theme::default_dark())
            .0
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
        let (lines, _) = render("aaa **bbb** ccc ddd", 8, &Theme::default_dark());
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
    fn items_starting_with_a_block_keep_their_marker() {
        assert_eq!(text("- ```\n  foo\n  ```", 40), ["•   foo"]);
        assert_eq!(
            text("1. ```\n   code\n   ```\n\n   text", 40),
            ["1.   code", "   ", "   text"]
        );
        assert_eq!(text("- | a |\n  |---|\n  | 1 |", 40), ["• a", "  ─", "  1"]);
    }

    #[test]
    fn code_block_and_inline_code() {
        assert_eq!(
            text("Run `x`:\n\n```\nfoo\n  bar\n```", 40),
            ["Run x:", "", "  foo", "    bar"]
        );
    }

    #[test]
    fn known_language_is_highlighted() {
        let (lines, _) = render("```rust\nfn main() {}\n```", 40, &Theme::default_dark());
        assert_eq!(
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>(),
            ["  fn main() {}"]
        );
        let colors: std::collections::HashSet<_> = lines[0]
            .spans
            .iter()
            .skip(1)
            .map(|span| span.style.fg)
            .collect();
        assert!(
            colors.len() > 1,
            "expected several token colours, got {colors:?}"
        );
        assert!(
            colors
                .iter()
                .all(|color| matches!(color, Some(Color::Rgb(..))))
        );
    }

    #[test]
    fn every_builtin_theme_highlights() {
        for (name, theme) in super::super::theme::ThemeConfig::default().themes() {
            assert!(
                highlight("fn main() {}", "rust", theme.syntax).is_some(),
                "{name}"
            );
        }
    }

    #[test]
    fn highlighting_is_the_same_for_every_theme_and_repeat() {
        // The comment spans lines, so parser state must carry across them.
        let code = "/* a\n b */\nfn main() {\n    let s = \"x\";\n}";
        for theme_name in ["base16-ocean.dark", "InspiredGitHub", "Dracula", "ansi"] {
            let mut highlighter = HighlightLines::new(
                SYNTAXES.find_syntax_by_token("rust").unwrap(),
                &SYNTAX_THEMES.themes[theme_name],
            );
            let expected: Vec<Vec<(Style, String)>> = LinesWithEndings::from(code)
                .map(|line| {
                    let regions = highlighter.highlight_line(line, &SYNTAXES).unwrap();
                    regions
                        .into_iter()
                        .map(|(style, text)| {
                            (
                                Style::new()
                                    .fg(terminal_color(style.foreground))
                                    .add_modifier(modifiers(style.font_style)),
                                text.trim_end_matches('\n').to_string(),
                            )
                        })
                        .collect()
                })
                .collect();
            for _ in 0..2 {
                let actual: Option<Vec<Vec<(Style, String)>>> = highlight(code, "rust", theme_name)
                    .map(|lines| {
                        lines
                            .into_iter()
                            .map(|spans| {
                                spans
                                    .into_iter()
                                    .map(|span| (span.style, span.content.into_owned()))
                                    .collect()
                            })
                            .collect()
                    });
                assert_eq!(actual.as_ref(), Some(&expected), "{theme_name}");
            }
        }
    }

    #[test]
    fn parse_cache_evicts_the_least_frecent_block() {
        let mut cache = ParseCache::new(2);
        let parses = std::cell::Cell::new(0);
        let mut get = |code: &str| {
            cache
                .operations(("Rust".into(), code.into()), || {
                    parses.set(parses.get() + 1);
                    Some(vec![])
                })
                .is_some()
        };
        for code in ["often", "often", "often", "once", "new"] {
            assert!(get(code));
        }
        assert_eq!(parses.get(), 3);
        assert!(get("often"));
        assert_eq!(parses.get(), 3, "the often used block was kept");
        assert!(get("once"));
        assert_eq!(parses.get(), 4, "the block used once was evicted");
    }

    #[test]
    fn common_changelog_languages_are_highlighted() {
        for language in [
            "toml",
            "ts",
            "tsx",
            "dockerfile",
            "nix",
            "zig",
            "swift",
            "kotlin",
        ] {
            assert!(
                highlight("x = 1", language, "base16-ocean.dark").is_some(),
                "{language}"
            );
        }
    }

    #[test]
    fn unknown_or_missing_language_stays_plain() {
        for markdown in ["```nosuchlang\nx = 1\n```", "```\nx = 1\n```", "    x = 1"] {
            let (lines, _) = render(markdown, 40, &Theme::default_dark());
            assert_eq!(lines[0].to_string(), "  x = 1", "{markdown:?}");
            assert_eq!(
                lines[0].spans[1].style,
                Theme::default_dark().code_block,
                "{markdown:?}"
            );
        }
    }

    #[test]
    fn highlighted_lines_wrap_and_keep_styles() {
        let (lines, _) = render(
            "```sh\necho aaaaaaaaaaaaaaaa\n```",
            14,
            &Theme::default_light(),
        );
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, ["  echo aaaaaaa", "  aaaaaaaaa"]);
        assert!(
            lines[1]
                .spans
                .iter()
                .skip(1)
                .all(|span| matches!(span.style.fg, Some(Color::Rgb(..))))
        );
    }

    fn link(line: usize, columns: Range<usize>, url: &str) -> Link {
        Link {
            line,
            columns,
            url: url.into(),
        }
    }

    #[test]
    fn links_are_tracked_without_url_text() {
        let (lines, links) = render(
            "[docs](https://d.io) and [#12](https://gh/pull/12)",
            80,
            &Theme::default_dark(),
        );
        assert_eq!(lines[0].to_string(), "docs and #12");
        assert_eq!(
            links,
            [
                link(0, 0..4, "https://d.io"),
                link(0, 9..12, "https://gh/pull/12")
            ]
        );
    }

    #[test]
    fn links_split_across_wrapped_lines_and_prefixes() {
        let (lines, links) = render("- aaa [bb cc](https://x.io) dd", 8, &Theme::default_dark());
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, ["• aaa bb", "  cc dd"]);
        assert_eq!(
            links,
            [link(0, 6..8, "https://x.io"), link(1, 2..4, "https://x.io")]
        );
    }

    #[test]
    fn relative_links_and_control_characters_are_not_linked() {
        assert_eq!(
            render("[x](docs/x.md) [y](#y)", 80, &Theme::default_dark()).1,
            []
        );
        // An escape in the URL would otherwise end the OSC 8 sequence early.
        assert_eq!(
            render(
                "[z](<https://z.io/a\u{1b}\u{7}b>)",
                80,
                &Theme::default_dark()
            )
            .1,
            [link(0, 0..1, "https://z.io/ab")]
        );
    }

    #[test]
    fn table_cells_keep_links_and_breaks() {
        assert_eq!(
            text("| [#1](https://x/1) | a<br>b |\n|---|---|\n| c | d |", 40),
            ["#1 │ a b", "───┼────", "c  │ d  "]
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
