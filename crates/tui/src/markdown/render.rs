//! Turns one scanned markdown block into finished terminal rows.
//!
//! Every row returned is already wrapped to the column budget, so the caller
//! commits through the no-wrap scrollback path where the buffer height is
//! exactly `rows.len()` — bypassing `chat::wrapped_height`, whose estimate
//! under-counts ratatui's word wrap and silently drops the tail of an answer.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::scan::{ListContext, Marker};
use super::wrap::{self, Gutter};

const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;

/// Bullet glyph per nesting depth; deeper levels reuse the last one.
const BULLETS: &[&str] = &["•", "◦", "▪"];
const TASK_DONE: &str = "☑";
const TASK_OPEN: &str = "☐";
/// Left gutter drawn beside a fenced code block's rows.
const CODE_GUTTER: &str = "│ ";
/// Gutter of the row naming a fenced block's language. A corner rather than
/// [`CODE_GUTTER`] so the label is distinguishable from code even where styling
/// is lost (a screenshot, a `capture-pane` dump, `TERM=dumb`).
const CODE_LABEL_GUTTER: &str = "┌ ";
/// Left gutter drawn beside a blockquote's rows.
const QUOTE_GUTTER: &str = "▌ ";
const RULE_GLYPH: &str = "─";
/// Spaces a tab expands to in a code block: a real tab would desynchronise the
/// column accounting every row is wrapped against.
const TAB_STOP: &str = "    ";
/// Deeper nesting than this renders flat rather than recursing further.
const MAX_NEST_DEPTH: usize = 6;
/// Columns of chrome a table spends per column: `│ ` before each cell plus the
/// closing `│`.
const TABLE_CHROME_PER_COLUMN: usize = 3;
/// Narrowest a table cell may be squeezed before the table degrades to a list.
const MIN_TABLE_CELL: usize = 3;

fn options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_GFM);
    opts
}

fn dim() -> Style {
    Style::default().fg(DIM)
}

/// Inline styles accumulated by the surrounding tags.
#[derive(Clone, Copy, Default)]
struct Inline {
    bold: bool,
    italic: bool,
    strike: bool,
    link: bool,
}

impl Inline {
    fn style(self) -> Style {
        let mut style = Style::default();
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link {
            style = style.fg(ACCENT).add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    fn code_style(self) -> Style {
        let mut style = self.style().fg(ACCENT);
        if self.link {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }
}

fn push_span(out: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push_str(text),
        _ => out.push(Span::styled(text.to_string(), style)),
    }
}

/// Collect the inline content of the events up to `end`, flattening soft breaks
/// to spaces so a hard-wrapped source paragraph reflows to the real width.
fn inline_spans<'a, I>(events: &mut I, end: TagEnd) -> Vec<Span<'static>>
where
    I: Iterator<Item = Event<'a>>,
{
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut style = Inline::default();
    let mut link_target: Option<String> = None;
    let mut link_text = String::new();
    for event in events.by_ref() {
        match event {
            Event::End(tag) if tag == end => break,
            Event::Start(Tag::Strong) => style.bold = true,
            Event::End(TagEnd::Strong) => style.bold = false,
            Event::Start(Tag::Emphasis) => style.italic = true,
            Event::End(TagEnd::Emphasis) => style.italic = false,
            Event::Start(Tag::Strikethrough) => style.strike = true,
            Event::End(TagEnd::Strikethrough) => style.strike = false,
            Event::Start(Tag::Link { dest_url, .. }) => {
                style.link = true;
                link_target = Some(dest_url.to_string());
                link_text.clear();
            }
            Event::End(TagEnd::Link) => {
                style.link = false;
                if let Some(target) = link_target
                    .take()
                    .filter(|t| !t.is_empty() && *t != link_text)
                {
                    push_span(&mut out, &format!(" ({target})"), dim());
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                push_span(&mut out, &format!("[image: {dest_url}] "), dim());
            }
            Event::Text(text) => {
                link_text.push_str(&text);
                push_span(&mut out, &sanitized(&text), style.style());
            }
            Event::Code(text) => {
                link_text.push_str(&text);
                push_span(&mut out, &sanitized(&text), style.code_style());
            }
            Event::InlineMath(text) => push_span(&mut out, &sanitized(&text), style.code_style()),
            Event::DisplayMath(text) => push_span(&mut out, &sanitized(&text), style.code_style()),
            Event::SoftBreak | Event::HardBreak => push_span(&mut out, " ", Style::default()),
            Event::InlineHtml(html) | Event::Html(html) => {
                push_span(&mut out, &sanitized(&html), dim());
            }
            Event::TaskListMarker(done) => {
                push_span(&mut out, if done { TASK_DONE } else { TASK_OPEN }, dim());
                push_span(&mut out, " ", Style::default());
            }
            Event::FootnoteReference(label) => {
                push_span(&mut out, &format!("[^{label}]"), dim());
            }
            Event::Rule | Event::Start(_) | Event::End(_) => {}
        }
    }
    out
}

fn sanitized(text: &CowStr<'_>) -> String {
    wrap::sanitize(text.as_ref()).replace('\n', " ")
}

fn heading_style(level: HeadingLevel) -> Style {
    let style = Style::default().add_modifier(Modifier::BOLD);
    match level {
        HeadingLevel::H1 | HeadingLevel::H2 => style.fg(ACCENT),
        _ => style,
    }
}

fn heading_prefix(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn bullet(depth: usize) -> &'static str {
    BULLETS[depth.min(BULLETS.len() - 1)]
}

/// Marker spans plus the matching continuation indent for a list item.
fn list_gutter(ctx: &ListContext) -> Gutter {
    let indent = " ".repeat(ctx.depth * 2);
    let marker = match &ctx.marker {
        Marker::Bullet => format!("{} ", bullet(ctx.depth)),
        Marker::Ordered(n) => format!("{n}. "),
        Marker::Task(true) => format!("{TASK_DONE} "),
        Marker::Task(false) => format!("{TASK_OPEN} "),
    };
    let hang = " ".repeat(marker.width());
    Gutter::hanging(
        vec![Span::raw(indent.clone()), Span::styled(marker, dim())],
        vec![Span::raw(format!("{indent}{hang}"))],
    )
}

/// Render one block the scanner proved complete.
///
/// A block whose body can hold further blocks — a list item, a blockquote — is
/// rendered by re-scanning its **source** one level down and prefixing the
/// resulting rows with this level's gutter. Recursing on source rather than
/// re-serialising parsed events keeps the round trip lossless: a link inside a
/// quote keeps its target, a fence keeps its language.
pub(crate) fn block(
    source: &str,
    list: Option<&ListContext>,
    width: usize,
    depth: usize,
) -> Vec<Line<'static>> {
    if let Some(ctx) = list {
        return nested(source, width, &list_gutter(ctx), depth);
    }
    if let Some(quoted) = strip_quote(source) {
        return quote(&quoted, width, depth);
    }
    flat(source, width)
}

/// Render `source` one nesting level down and prefix every row with `gutter`,
/// laying the content out in the columns the gutter leaves.
fn nested(source: &str, width: usize, gutter: &Gutter, depth: usize) -> Vec<Line<'static>> {
    let lead_first = gutter.first_spans();
    let lead_rest = gutter.rest_spans();
    let lead_width = wrap::spans_width(lead_first).max(wrap::spans_width(lead_rest));
    let inner_width = width.saturating_sub(lead_width);
    if inner_width == 0 || depth >= MAX_NEST_DEPTH {
        return flat(source, width);
    }
    let rows = super::render_nested(source, inner_width, depth + 1);
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = if index == 0 {
                lead_first.to_vec()
            } else {
                lead_rest.to_vec()
            };
            spans.extend(row.spans);
            Line::from(spans)
        })
        .collect()
}

/// Strip one level of `>` markers, or `None` when `source` is not a blockquote.
/// CommonMark allows up to three leading spaces and an optional space after the
/// marker; a bare `>` line becomes an empty line.
fn strip_quote(source: &str) -> Option<String> {
    const MAX_QUOTE_INDENT: usize = 3;
    let mut out = Vec::new();
    let mut saw_marker = false;
    for line in source.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        match trimmed.strip_prefix('>') {
            Some(rest) if indent <= MAX_QUOTE_INDENT => {
                saw_marker = true;
                out.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // A lazy continuation line belongs to the quote's paragraph.
            _ => out.push(line.to_string()),
        }
    }
    saw_marker.then(|| out.join("\n"))
}

/// Render a leaf block whose kind is decided by parsing it. Container blocks
/// (blockquote, list item) never reach here — [`block`] peels them off first.
fn flat(source: &str, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let mut events = Parser::new_ext(source, options()).peekable();
    let mut rows = Vec::new();
    while let Some(event) = events.next() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let spans = inline_spans(&mut events, TagEnd::Heading(level));
                let hashes = "#".repeat(heading_prefix(level));
                let gutter = Gutter::hanging(
                    vec![Span::styled(format!("{hashes} "), dim())],
                    vec![Span::raw(" ".repeat(hashes.width() + 1))],
                );
                let styled = restyle(spans, heading_style(level));
                rows.extend(wrap::wrap(&styled, width, &gutter));
            }
            Event::Start(Tag::Paragraph) => {
                let spans = inline_spans(&mut events, TagEnd::Paragraph);
                rows.extend(wrap::wrap(&spans, width, &Gutter::none()));
            }
            Event::Start(Tag::CodeBlock(_)) => {
                let text = raw_text(&mut events, TagEnd::CodeBlock);
                for line in text.lines() {
                    rows.extend(code_line(line, width));
                }
            }
            Event::Start(Tag::Table(alignments)) => {
                rows.extend(table(&mut events, &alignments, width));
            }
            Event::Start(Tag::List(start)) => {
                rows.extend(flat_list(&mut events, start, width));
            }
            Event::Rule => rows.push(rule(width)),
            Event::Start(Tag::HtmlBlock) => {
                let text = raw_text(&mut events, TagEnd::HtmlBlock);
                for line in text.lines() {
                    rows.extend(wrap::wrap(
                        &[Span::styled(wrap::sanitize(line), dim())],
                        width,
                        &Gutter::none(),
                    ));
                }
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                rows.extend(wrap::wrap(
                    &[Span::styled(format!("[^{label}]"), dim())],
                    width,
                    &Gutter::none(),
                ));
            }
            _ => {}
        }
    }
    rows
}

/// Replace the base style of every span while keeping its own modifiers, so a
/// heading's bold applies to text that is also emphasised or code.
fn restyle(spans: Vec<Span<'static>>, base: Style) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|span| {
            let style = base.patch(span.style);
            Span::styled(span.content, style)
        })
        .collect()
}

fn raw_text<'a, I>(events: &mut I, end: TagEnd) -> String
where
    I: Iterator<Item = Event<'a>>,
{
    let mut out = String::new();
    for event in events.by_ref() {
        match event {
            Event::End(tag) if tag == end => break,
            Event::Text(text) | Event::Html(text) | Event::Code(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            _ => {}
        }
    }
    out
}

/// One row of a fenced code block: a dim gutter, then the line verbatim.
pub(crate) fn code_line(text: &str, width: usize) -> Vec<Line<'static>> {
    let gutter = Gutter::uniform(vec![Span::styled(CODE_GUTTER, dim())]);
    // Expand before sanitising: `sanitize` drops tabs as control characters, so
    // the other order silently deletes a tab-indented line's indentation.
    let content = wrap::sanitize(&text.replace('\t', TAB_STOP));
    wrap::wrap(&[Span::raw(content)], width, &gutter)
}

/// The header row a fence opens with: the gutter plus its language, if any.
pub(crate) fn code_start(lang: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let label = lang.map(str::trim).filter(|l| !l.is_empty());
    let Some(label) = label else {
        return Vec::new();
    };
    let gutter = Gutter::uniform(vec![Span::styled(CODE_LABEL_GUTTER, dim())]);
    wrap::wrap(
        &[Span::styled(
            wrap::sanitize(label),
            dim().add_modifier(Modifier::ITALIC),
        )],
        width,
        &gutter,
    )
}

fn rule(width: usize) -> Line<'static> {
    Line::from(Span::styled(RULE_GLYPH.repeat(width), dim()))
}

/// GFM alert label, e.g. `> [!WARNING]`.
fn alert_label(kind: BlockQuoteKind) -> (&'static str, Color) {
    match kind {
        BlockQuoteKind::Note => ("NOTE", ACCENT),
        BlockQuoteKind::Tip => ("TIP", Color::Green),
        BlockQuoteKind::Important => ("IMPORTANT", Color::Magenta),
        BlockQuoteKind::Warning => ("WARNING", Color::Yellow),
        BlockQuoteKind::Caution => ("CAUTION", Color::Red),
    }
}

/// Render a blockquote's already-stripped body behind a gutter, prefixed by the
/// GFM alert label when the body opens with one.
fn quote(body: &str, width: usize, depth: usize) -> Vec<Line<'static>> {
    let gutter = || Gutter::uniform(vec![Span::styled(QUOTE_GUTTER, dim())]);
    let (alert, body) = split_alert(body);
    let mut rows = Vec::new();
    if let Some(kind) = alert {
        let (label, color) = alert_label(kind);
        let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
        rows.extend(wrap::wrap(
            &[Span::styled(label.to_string(), style)],
            width,
            &gutter(),
        ));
    }
    if !body.trim().is_empty() {
        rows.extend(nested(&body, width, &gutter(), depth));
    }
    rows
}

/// Split a leading GFM alert marker (`[!WARNING]`) off a quote body.
fn split_alert(body: &str) -> (Option<BlockQuoteKind>, String) {
    let mut lines = body.lines();
    let Some(first) = lines.next() else {
        return (None, String::new());
    };
    let marker = first.trim();
    let Some(name) = marker
        .strip_prefix("[!")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return (None, body.to_string());
    };
    let kind = match name.to_ascii_uppercase().as_str() {
        "NOTE" => BlockQuoteKind::Note,
        "TIP" => BlockQuoteKind::Tip,
        "IMPORTANT" => BlockQuoteKind::Important,
        "WARNING" => BlockQuoteKind::Warning,
        "CAUTION" => BlockQuoteKind::Caution,
        _ => return (None, body.to_string()),
    };
    (Some(kind), lines.collect::<Vec<_>>().join("\n"))
}

/// A list reaching [`flat`] means the scanner did not split it into items — a
/// source shape it does not recognise as a list. Render each item's inline text
/// behind a marker so nothing is dropped; nested structure is flattened.
fn flat_list<'a, I>(events: &mut I, start: Option<u64>, width: usize) -> Vec<Line<'static>>
where
    I: Iterator<Item = Event<'a>>,
{
    let mut rows = Vec::new();
    let mut ordinal = start.unwrap_or(1);
    loop {
        match events.next() {
            Some(Event::Start(Tag::Item)) => {
                let spans = inline_spans(events, TagEnd::Item);
                let marker = match start {
                    Some(_) => Marker::Ordered(ordinal),
                    None => Marker::Bullet,
                };
                let ctx = ListContext { depth: 0, marker };
                rows.extend(wrap::wrap(&spans, width, &list_gutter(&ctx)));
                ordinal += 1;
            }
            Some(Event::End(TagEnd::List(_))) | None => break,
            Some(_) => {}
        }
    }
    rows
}

struct Cell {
    spans: Vec<Span<'static>>,
    width: usize,
}

fn table<'a, I>(events: &mut I, alignments: &[Alignment], width: usize) -> Vec<Line<'static>>
where
    I: Iterator<Item = Event<'a>>,
{
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut header = 0usize;
    loop {
        match events.next() {
            Some(Event::Start(Tag::TableHead)) => {
                rows.push(table_row(events, TagEnd::TableHead));
                header = rows.len();
            }
            Some(Event::Start(Tag::TableRow)) => {
                rows.push(table_row(events, TagEnd::TableRow));
            }
            Some(Event::End(TagEnd::Table)) | None => break,
            Some(_) => {}
        }
    }
    rows.retain(|row| !row.is_empty());
    if rows.is_empty() {
        return Vec::new();
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let widths = column_widths(&rows, columns, width);
    let Some(widths) = widths else {
        return table_as_list(&rows, header, width);
    };
    let mut out = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        out.extend(table_body_rows(row, &widths, alignments, index < header));
        if index + 1 == header {
            out.push(table_separator(&widths));
        }
    }
    out
}

fn table_row<'a, I>(events: &mut I, end: TagEnd) -> Vec<Cell>
where
    I: Iterator<Item = Event<'a>>,
{
    let mut cells = Vec::new();
    loop {
        match events.next() {
            Some(Event::Start(Tag::TableCell)) => {
                let spans = inline_spans(events, TagEnd::TableCell);
                let width = wrap::spans_width(&spans);
                cells.push(Cell { spans, width });
            }
            Some(Event::End(tag)) if tag == end => break,
            None => break,
            Some(_) => {}
        }
    }
    cells
}

/// Column budgets that fit `width`, or `None` when even the minimum does not.
fn column_widths(rows: &[Vec<Cell>], columns: usize, width: usize) -> Option<Vec<usize>> {
    let chrome = columns * TABLE_CHROME_PER_COLUMN + 1;
    let content = width.checked_sub(chrome)?;
    if content < columns * MIN_TABLE_CELL {
        return None;
    }
    let mut natural = vec![0usize; columns];
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            natural[index] = natural[index].max(cell.width);
        }
    }
    let total: usize = natural.iter().sum();
    if total <= content {
        return Some(natural);
    }
    // Shrink the widest columns first so narrow ones stay readable.
    let mut budgets = natural.clone();
    let mut excess = total - content;
    while excess > 0 {
        let widest = budgets
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > MIN_TABLE_CELL)
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)?;
        budgets[widest] -= 1;
        excess -= 1;
    }
    Some(budgets)
}

fn align(spans: &[Span<'static>], budget: usize, alignment: Alignment) -> Vec<Span<'static>> {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let clipped = wrap::clip(&text, budget);
    let pad = budget.saturating_sub(clipped.width());
    let (left, right) = match alignment {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::None | Alignment::Left => (0, pad),
    };
    let style = spans.first().map(|s| s.style).unwrap_or_default();
    let mut out = Vec::new();
    if left > 0 {
        out.push(Span::raw(" ".repeat(left)));
    }
    out.push(Span::styled(clipped, style));
    if right > 0 {
        out.push(Span::raw(" ".repeat(right)));
    }
    out
}

fn table_body_rows(
    row: &[Cell],
    widths: &[usize],
    alignments: &[Alignment],
    header: bool,
) -> Vec<Line<'static>> {
    let mut spans = vec![Span::styled("│", dim())];
    for (index, budget) in widths.iter().enumerate() {
        let empty = Vec::new();
        let cell = row.get(index).map(|c| &c.spans).unwrap_or(&empty);
        let alignment = alignments.get(index).copied().unwrap_or(Alignment::None);
        let cell = if header {
            restyle(cell.clone(), Style::default().add_modifier(Modifier::BOLD))
        } else {
            cell.clone()
        };
        spans.push(Span::raw(" "));
        spans.extend(align(&cell, *budget, alignment));
        spans.push(Span::raw(" "));
        spans.push(Span::styled("│", dim()));
    }
    vec![Line::from(spans)]
}

fn table_separator(widths: &[usize]) -> Line<'static> {
    let mut text = String::from("├");
    for (index, budget) in widths.iter().enumerate() {
        text.push_str(&RULE_GLYPH.repeat(budget + 2));
        text.push(if index + 1 == widths.len() {
            '┤'
        } else {
            '┼'
        });
    }
    Line::from(Span::styled(text, dim()))
}

/// A table too narrow for any column layout renders as `header: value` rows,
/// which loses the grid but never loses a cell.
fn table_as_list(rows: &[Vec<Cell>], header: usize, width: usize) -> Vec<Line<'static>> {
    let headers: Vec<String> = rows
        .first()
        .filter(|_| header > 0)
        .map(|row| {
            row.iter()
                .map(|c| c.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for row in rows.iter().skip(header) {
        for (index, cell) in row.iter().enumerate() {
            let mut spans = Vec::new();
            if let Some(label) = headers.get(index) {
                spans.push(Span::styled(format!("{label}: "), dim()));
            }
            spans.extend(cell.spans.clone());
            out.extend(wrap::wrap(
                &spans,
                width,
                &Gutter::hanging(vec![], vec![Span::raw("  ")]),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn render(source: &str, width: usize) -> Vec<String> {
        text_of(&block(source, None, width, 0))
    }

    fn styles_of(rows: &[Line<'_>]) -> Vec<(String, Style)> {
        rows.iter()
            .flat_map(|r| r.spans.iter())
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    #[test]
    fn a_paragraph_drops_its_markup_and_keeps_the_text() {
        assert_eq!(
            render("plain **bold** and *italic* and `code`", 80),
            vec!["plain bold and italic and code"]
        );
    }

    #[test]
    fn emphasis_becomes_a_modifier() {
        let rows = block("a **bold** b", None, 80, 0);
        let bold = styles_of(&rows)
            .into_iter()
            .find(|(t, _)| t.contains("bold"))
            .map(|(_, s)| s);
        assert!(
            bold.is_some_and(|s| s.add_modifier.contains(Modifier::BOLD)),
            "{:?}",
            styles_of(&rows)
        );
    }

    #[test]
    fn a_heading_keeps_its_level_prefix_and_goes_bold() {
        let rows = block("## Heading here", None, 80, 0);
        assert_eq!(text_of(&rows), vec!["## Heading here"]);
        let styled = styles_of(&rows)
            .into_iter()
            .find(|(t, _)| t.contains("Heading"))
            .map(|(_, s)| s);
        assert!(styled.is_some_and(|s| s.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn a_setext_heading_renders_as_a_heading() {
        assert_eq!(render("Title\n===", 80), vec!["# Title"]);
        assert_eq!(render("Title\n---", 80), vec!["## Title"]);
    }

    #[test]
    fn a_rule_fills_the_width() {
        let rows = render("---", 12);
        assert_eq!(rows, vec!["────────────"]);
    }

    #[test]
    fn a_bullet_item_gets_a_glyph_and_hanging_indent() {
        let ctx = ListContext {
            depth: 0,
            marker: Marker::Bullet,
        };
        let rows = text_of(&block("alpha beta gamma delta epsilon", Some(&ctx), 16, 0));
        assert!(rows[0].starts_with("• "), "{rows:?}");
        for row in rows.iter().skip(1) {
            assert!(row.starts_with("  "), "{rows:?}");
        }
    }

    #[test]
    fn an_ordered_item_uses_the_scanned_ordinal() {
        let ctx = ListContext {
            depth: 0,
            marker: Marker::Ordered(7),
        };
        assert_eq!(
            text_of(&block("seventh", Some(&ctx), 40, 0))[0],
            "7. seventh"
        );
    }

    #[test]
    fn a_task_item_shows_a_checkbox() {
        for (done, glyph) in [(true, TASK_DONE), (false, TASK_OPEN)] {
            let ctx = ListContext {
                depth: 0,
                marker: Marker::Task(done),
            };
            assert!(text_of(&block("task", Some(&ctx), 40, 0))[0].starts_with(glyph));
        }
    }

    #[test]
    fn a_nested_item_is_indented_and_uses_a_different_glyph() {
        let ctx = ListContext {
            depth: 1,
            marker: Marker::Bullet,
        };
        let row = &text_of(&block("inner", Some(&ctx), 40, 0))[0];
        assert!(row.starts_with("  ◦ "), "{row:?}");
    }

    #[test]
    fn a_quote_gets_a_gutter_on_every_row() {
        let rows = text_of(&block("> some quoted text that wraps around", None, 20, 0));
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(row.starts_with(QUOTE_GUTTER), "{rows:?}");
        }
    }

    #[test]
    fn a_gfm_alert_shows_its_label() {
        let rows = text_of(&block("> [!WARNING]\n> be careful", None, 40, 0));
        assert!(rows[0].contains("WARNING"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("be careful")), "{rows:?}");
    }

    #[test]
    fn a_quote_keeps_its_body_intact() {
        // Nested bodies re-scan their source rather than re-serialising parsed
        // events, so a link keeps its target and a fence keeps its language.
        let rows = text_of(&block("> see [docs](https://example.com/x)", None, 60, 0));
        assert!(
            rows.iter().any(|r| r.contains("https://example.com/x")),
            "the link target was lost: {rows:?}"
        );

        let rows = text_of(&block("> ```rust\n> fn main() {}\n> ```", None, 60, 0));
        assert!(
            rows.iter().any(|r| r.contains("rust")),
            "the fence language was lost: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("fn main() {}")),
            "the fence body was lost: {rows:?}"
        );
        for row in &rows {
            assert!(row.starts_with(QUOTE_GUTTER), "{rows:?}");
        }
    }

    #[test]
    fn a_quote_can_hold_a_list_and_a_heading() {
        let rows = text_of(&block("> ## Title\n>\n> - one\n> - two", None, 40, 0));
        let joined = rows.join("\n");
        assert!(joined.contains("## Title"), "{rows:?}");
        assert!(
            joined.contains("• one") && joined.contains("• two"),
            "{rows:?}"
        );
        for row in &rows {
            assert!(row.starts_with(QUOTE_GUTTER), "{rows:?}");
        }
    }

    #[test]
    fn a_nested_quote_stacks_its_gutters() {
        let rows = text_of(&block("> outer\n>\n> > inner", None, 40, 0));
        assert!(
            rows.iter()
                .any(|r| r.starts_with(&format!("{QUOTE_GUTTER}{QUOTE_GUTTER}"))),
            "a quote inside a quote needs two gutters: {rows:?}"
        );
    }

    #[test]
    fn quote_nesting_terminates_on_a_pathological_depth() {
        let source: String = ">".repeat(64) + " deep";
        let rows = text_of(&block(&source, None, 40, 0));
        assert!(
            rows.iter().any(|r| r.contains("deep")),
            "content must survive the depth cap: {rows:?}"
        );
    }

    #[test]
    fn a_list_item_can_hold_a_fenced_block() {
        let ctx = ListContext {
            depth: 0,
            marker: Marker::Bullet,
        };
        let rows = text_of(&block("run this:\n```sh\nls -la\n```", Some(&ctx), 40, 0));
        let joined = rows.join("\n");
        assert!(joined.contains("ls -la"), "{rows:?}");
        assert!(
            joined.contains("sh"),
            "the fence language was lost: {rows:?}"
        );
        assert!(rows[0].starts_with("• "), "{rows:?}");
    }

    #[test]
    fn a_code_line_gets_a_gutter_and_keeps_its_spacing() {
        let rows = text_of(&code_line("    indented = True", 40));
        assert_eq!(rows, vec!["│     indented = True"]);
    }

    #[test]
    fn a_tab_indented_code_line_keeps_its_indentation() {
        assert_eq!(
            text_of(&code_line("\tpass", 40)),
            vec!["│     pass"],
            "tabs must expand before sanitising, which drops them"
        );
    }

    #[test]
    fn a_code_start_labels_the_language_only_when_present() {
        assert_eq!(text_of(&code_start(Some("rust"), 40)), vec!["┌ rust"]);
        assert!(code_start(None, 40).is_empty());
        assert!(code_start(Some(""), 40).is_empty());
    }

    #[test]
    fn a_table_aligns_on_display_width() {
        let source = "| lang | note |\n|---|---|\n| rust | fast |\n| 中文 | 宽字符 |";
        let rows = text_of(&block(source, None, 40, 0));
        let widths: Vec<usize> = rows.iter().map(|r| r.width()).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "table rows disagree on width: {widths:?} {rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("宽字符")), "{rows:?}");
    }

    #[test]
    fn a_table_never_exceeds_the_width() {
        let source = "| a very long header indeed | another long header |\n|---|---|\n| some long cell text here | more long cell text |";
        for width in 6..80 {
            let rows = block(source, None, width, 0);
            for row in &rows {
                assert!(
                    row.width() <= width,
                    "width {width}: row {} cols: {:?}",
                    row.width(),
                    text_of(std::slice::from_ref(row))
                );
            }
        }
    }

    #[test]
    fn a_table_too_narrow_to_lay_out_degrades_to_labelled_rows() {
        let source = "| lang | note |\n|---|---|\n| rust | fast |";
        let rows = text_of(&block(source, None, 12, 0));
        let joined = rows.join(" ");
        assert!(
            joined.contains("rust") && joined.contains("fast"),
            "{rows:?}"
        );
        assert!(
            joined.contains("lang:") && joined.contains("note:"),
            "the header labels are the only thing keeping the header text: {rows:?}"
        );
    }

    #[test]
    fn a_ragged_table_follows_gfm_cell_counting() {
        // GFM: a row with fewer cells than the header is padded, and cells beyond
        // the header's count are ignored — pulldown-cmark applies both, so the
        // renderer only ever sees at most the header's column count.
        let rows = text_of(&block("| a | b |\n|---|---|\n| 1 |", None, 20, 0));
        assert_eq!(
            rows,
            vec!["│ a │ b │", "├───┼───┤", "│ 1 │   │"],
            "a short row is padded, not dropped"
        );
        let rows = text_of(&block("| a |\n|---|\n| 1 | 2 |", None, 20, 0));
        assert_eq!(
            rows,
            vec!["│ a │", "├───┤", "│ 1 │"],
            "excess cells are ignored"
        );
    }

    #[test]
    fn a_link_shows_its_target_when_it_differs_from_the_text() {
        let rows = render("see [docs](https://example.com/x)", 80);
        assert_eq!(rows, vec!["see docs (https://example.com/x)"]);
    }

    #[test]
    fn an_autolink_is_not_duplicated() {
        let rows = render("<https://example.com/>", 80);
        assert_eq!(rows, vec!["https://example.com/"]);
    }

    #[test]
    fn no_row_of_any_block_exceeds_the_width() {
        let sources = [
            "# 一个很长的中文标题用来测试换行是否正确处理",
            "普通段落，中文没有空格，必须能在字之间断行，并且不能让标点出现在行首。",
            "> 引用里的中文也要正确换行，这是一段比较长的引用文字。",
            "- 列表项里的中文同样需要悬挂缩进和正确的换行处理",
            "| 语言 | 说明 |\n|---|---|\n| rust | 很快 |",
            "plain ascii paragraph with several words to wrap",
            "---",
        ];
        for source in sources {
            for width in 4..60 {
                for row in &block(source, None, width, 0) {
                    assert!(
                        row.width() <= width,
                        "source {source:?} width {width}: row is {} cols: {:?}",
                        row.width(),
                        text_of(std::slice::from_ref(row))
                    );
                }
            }
        }
    }

    #[test]
    fn cjk_emphasis_adjacent_to_punctuation_is_left_literal() {
        // CommonMark flanking: a closer preceded by punctuation and followed by
        // a letter does not close. `**note:**text` fails the same way in ASCII.
        // The web client fixes this with remark-cjk-friendly; there is no Rust
        // equivalent, so the markers stay visible rather than silently vanish.
        assert_eq!(render("**要点：**说明", 80), vec!["**要点：**说明"]);
        assert_eq!(render("**要点：** 说明", 80), vec!["要点： 说明"]);
        assert_eq!(render("**要点**：说明", 80), vec!["要点：说明"]);
    }

    #[test]
    fn control_characters_are_stripped_from_text() {
        let rows = render("before\u{1b}[31mafter", 80);
        assert_eq!(rows, vec!["before[31mafter"]);
    }

    #[test]
    fn an_empty_source_renders_nothing() {
        assert!(block("", None, 40, 0).is_empty());
        assert!(block("   ", None, 40, 0).is_empty());
    }
}
