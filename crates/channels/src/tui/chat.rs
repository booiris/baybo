//! Chat view renderer: scrollback pane + input line.

use aura_model::ContentBlock;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{AppState, ChatLine};
use crate::tui::event::LogLevel;

/// Maximum rows the input box grows to before it clips. Beyond this, the
/// cursor may scroll off-screen; that's acceptable for a chat-style input
/// where very tall drafts are rare.
const INPUT_MAX_ROWS: u16 = 10;

pub(crate) fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let input_h = input_box_height(state);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(input_h)])
        .split(area);

    render_scrollback(frame, chunks[0], state);
    render_input(frame, chunks[1], state);
    render_completion_popup(frame, chunks[1], state);
}

fn input_box_height(state: &AppState) -> u16 {
    let lines = state.input.matches('\n').count().saturating_add(1) as u16;
    (lines.min(INPUT_MAX_ROWS)).saturating_add(2)
}

fn render_scrollback(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(state.scrollback.len() * 2);
    for chat in &state.scrollback {
        match chat {
            ChatLine::User(text) => {
                let prefix = Span::styled(
                    "you> ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
                let mut first = true;
                for line_text in text.lines() {
                    let spans = if first {
                        vec![prefix.clone(), Span::raw(line_text.to_string())]
                    } else {
                        vec![Span::raw("     "), Span::raw(line_text.to_string())]
                    };
                    lines.push(Line::from(spans));
                    first = false;
                }
                if first {
                    // Empty text still deserves a visible prompt row.
                    lines.push(Line::from(vec![prefix, Span::raw("")]));
                }
            }
            ChatLine::Assistant(blocks) => {
                let prefix = Span::styled(
                    "aura> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                );
                let mut first = true;
                for block in blocks {
                    for (i, text) in render_block(block).lines().enumerate() {
                        let spans = if first && i == 0 {
                            vec![prefix.clone(), Span::raw(text.to_string())]
                        } else {
                            vec![Span::raw("      "), Span::raw(text.to_string())]
                        };
                        lines.push(Line::from(spans));
                    }
                    first = false;
                }
            }
            ChatLine::System(text) => {
                lines.push(Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            ChatLine::Log(record) => {
                let (label, color) = match record.level {
                    LogLevel::Error => ("error", Color::Red),
                    LogLevel::Warn => ("warn ", Color::Yellow),
                };
                let prefix = Span::styled(
                    format!("{label} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                );
                let target = Span::styled(
                    format!("[{}] ", record.target),
                    Style::default().fg(Color::DarkGray),
                );
                let mut iter = record.message.lines();
                if let Some(first) = iter.next() {
                    lines.push(Line::from(vec![
                        prefix.clone(),
                        target.clone(),
                        Span::raw(first.to_string()),
                    ]));
                } else {
                    lines.push(Line::from(vec![prefix, target]));
                }
                for rest in iter {
                    lines.push(Line::from(vec![
                        Span::raw("      "),
                        Span::raw(rest.to_string()),
                    ]));
                }
            }
        }
        lines.push(Line::from(""));
    }

    if let Some(stream) = state.streaming.as_deref() {
        let prefix = Span::styled(
            "aura> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        );
        let mut first = true;
        for text in stream.lines() {
            let spans = if first {
                vec![prefix.clone(), Span::raw(text.to_string())]
            } else {
                vec![Span::raw("      "), Span::raw(text.to_string())]
            };
            lines.push(Line::from(spans));
            first = false;
        }
        if stream.ends_with('\n') || stream.is_empty() {
            lines.push(Line::from(vec![prefix, Span::raw("")]));
        }
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(" chat ")
        .title_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    let total = lines.len() as u16;
    let viewport = inner.height;
    let scroll = if total > viewport {
        total
            .saturating_sub(viewport)
            .saturating_sub(state.scroll_offset)
    } else {
        0
    };

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    // Right-aligned hint on the top border. Shows the newline shortcut so
    // first-time users discover multi-line input without needing to open
    // the docs. Dim style to stay unobtrusive against the main " input "
    // title on the left.
    let hint = Line::from(Span::styled(
        " shift+enter · alt+enter = newline ",
        Style::default().fg(Color::DarkGray),
    ))
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(" input ")
        .title_top(hint);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(state.input.as_str()).block(block);
    frame.render_widget(paragraph, area);

    // Put the cursor where the insertion point is, inside the input box.
    // For multi-line input we split the prefix on '\n' to compute the row,
    // and use display width (not byte or code-point count) on the current
    // line so CJK/wide chars and zero-width combiners land the caret on the
    // right terminal column. Clamp the row to the visible viewport so the
    // caret doesn't leak past the border when the draft exceeds
    // INPUT_MAX_ROWS.
    let byte_cursor = state.cursor.min(state.input.len());
    let prefix = &state.input[..byte_cursor];
    let line_index = prefix.matches('\n').count() as u16;
    let last_line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col_width = prefix[last_line_start..].width() as u16;
    let row = line_index.min(inner.height.saturating_sub(1));
    let cursor_x = inner.x + col_width;
    let cursor_y = inner.y + row;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Popup shown above the input box when the buffer starts with `/` and
/// there is at least one matching command. Rendered last so it paints over
/// the scrollback; `Clear` wipes the underlying region first.
fn render_completion_popup(frame: &mut Frame, input_area: Rect, state: &AppState) {
    let candidates = state.completion_candidates();
    if candidates.is_empty() {
        return;
    }
    let max_rows = 8u16;
    let height = candidates.len().min(max_rows as usize) as u16 + 2;
    if input_area.y < height {
        return;
    }
    let width = input_area.width.min(60);
    let popup = Rect {
        x: input_area.x,
        y: input_area.y - height,
        width,
        height,
    };

    let selected = state.completion_cursor.min(candidates.len() - 1);
    let items: Vec<ListItem<'_>> = candidates
        .iter()
        .map(|c| {
            let name = Span::styled(
                format!("{:<12}", c.name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
            let desc = Span::styled(c.description.clone(), Style::default().fg(Color::DarkGray));
            ListItem::new(Line::from(vec![name, Span::raw(" "), desc]))
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(selected));

    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(" commands ")
        .title_style(Style::default().fg(Color::DarkGray));
    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut list_state);
}

fn render_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.clone(),
        ContentBlock::Image { mime_type, .. } => format!("[Image: {mime_type}]"),
        ContentBlock::Audio { mime_type, .. } => format!("[Audio: {mime_type}]"),
        ContentBlock::File {
            filename,
            mime_type,
            ..
        } => format!("[File: {filename} ({mime_type})]"),
    }
}
