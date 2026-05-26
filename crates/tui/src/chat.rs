//! Chat view renderer.
//!
//! With the inline-viewport rendering model, this module has two
//! responsibilities:
//!
//! 1. [`render`] paints the *live region* at the bottom of the screen —
//!    optional streaming preview + optional pending-approval block + input
//!    box + completion popup.
//! 2. The `render_*_lines` helpers produce `Vec<Line<'static>>` for each
//!    kind of conversation entry. The event loop hands those to
//!    `terminal.insert_before(...)` so they land in the terminal's native
//!    scrollback.

use aura_model::ContentBlock;
use aura_tools::{ApprovalDecision, ResourceAccess};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::app::{AppState, ApprovalChatEntry, ApprovalChatState};
use crate::event::{LogLevel, LogRecord};

/// Maximum rows the input box grows to before it clips. Beyond this, the
/// cursor may scroll off-screen.
pub(crate) const INPUT_MAX_ROWS: u16 = 10;

/// Render the live region for chat mode. The viewport now exactly matches
/// the needed height (input + optional approval), so there's no empty
/// space between the latest scrollback message and the input box.
/// Streaming text is committed to scrollback line-by-line (Codex style)
/// rather than buffered into a viewport preview.
pub(crate) fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let input_h = input_box_height(state);
    if let Some(entry) = state.pending_approval.as_ref() {
        let approval_h = approval_pending_height(entry);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(approval_h), Constraint::Length(input_h)])
            .split(area);
        render_approval_block(frame, chunks[0], entry);
        render_input(frame, chunks[1], state);
        render_completion_popup(frame, chunks[1], state);
    } else {
        render_input(frame, area, state);
        render_completion_popup(frame, area, state);
    }
}

/// Number of rows the pending-approval prompt occupies when rendered.
/// Used by `lib.rs` to size the dynamic inline viewport so the prompt
/// fits without clipping.
pub(crate) fn approval_pending_height(entry: &ApprovalChatEntry) -> u16 {
    render_approval_pending_lines(entry).len() as u16
}

pub(crate) fn input_box_height(state: &AppState) -> u16 {
    let lines = state.input.matches('\n').count().saturating_add(1) as u16;
    (lines.min(INPUT_MAX_ROWS)).saturating_add(2)
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let hint = Line::from(Span::styled(
        " shift+enter · alt+enter = newline ",
        Style::default().fg(Color::DarkGray),
    ))
    .right_aligned();
    // Surface the queued-submission depth in the title so the user can
    // tell their Enter "took effect" even though the message hasn't
    // appeared in scrollback yet — it will flush after the in-flight
    // agent response finishes.
    let title = if state.outgoing_queue.is_empty() {
        " input ".to_string()
    } else {
        format!(" input · {} queued ", state.outgoing_queue.len())
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(title)
        .title_top(hint);
    let inner = block.inner(area);
    let paragraph = Paragraph::new(state.input.as_str()).block(block);
    frame.render_widget(paragraph, area);

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

fn render_approval_block(frame: &mut Frame, area: Rect, entry: &ApprovalChatEntry) {
    if area.height == 0 {
        return;
    }
    let lines = render_approval_pending_lines(entry);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

// ---- Line-rendering helpers (used by lib.rs for terminal.insert_before) ----

pub(crate) fn render_user_lines(text: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
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
        out.push(Line::from(spans));
        first = false;
    }
    if first {
        out.push(Line::from(vec![prefix, Span::raw("")]));
    }
    out
}

pub(crate) fn render_assistant_lines(blocks: &[ContentBlock]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let prefix = Span::styled(
        "aura> ",
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );
    let mut first = true;
    for block in blocks {
        let Some(rendered) = render_block(block) else {
            continue;
        };
        for (i, text) in rendered.lines().enumerate() {
            let spans = if first && i == 0 {
                vec![prefix.clone(), Span::raw(text.to_string())]
            } else {
                vec![Span::raw("      "), Span::raw(text.to_string())]
            };
            out.push(Line::from(spans));
        }
        first = false;
    }
    out
}

/// One rendered line of a streaming agent response. The first line of
/// the response uses the `aura> ` (bold green) prefix; every subsequent
/// line uses the six-space continuation indent so the conversation
/// reads as one coherent block. Callers set `is_continuation` based on
/// `AppState::streaming_committed_any` — see [`crate::app`].
pub(crate) fn render_stream_line(text: &str, is_continuation: bool) -> Vec<Line<'static>> {
    let leader = if is_continuation {
        Span::raw("      ")
    } else {
        Span::styled(
            "aura> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    };
    vec![Line::from(vec![leader, Span::raw(text.to_string())])]
}

/// One dim line of the agent's reasoning ("thinking") trace. The first
/// line of a run gets a `✻ ` leader; continuations align under it. All
/// `DarkGray` so it reads as background working, distinct from the
/// `aura> ` answer. `is_continuation` mirrors
/// `AppState::reasoning_committed_any`.
pub(crate) fn render_reasoning_line(text: &str, is_continuation: bool) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let leader = if is_continuation { "  " } else { "✻ " };
    vec![Line::from(vec![
        Span::styled(leader, dim),
        Span::styled(text.to_string(), dim),
    ])]
}

/// A `⏺ tool(label)` line committed the moment a tool call is dispatched.
/// The bullet + name are cyan; the optional human label is dim in parens.
pub(crate) fn render_tool_started(tool: &str, label: Option<&str>) -> Vec<Line<'static>> {
    let mut spans = vec![
        Span::styled("⏺ ", Style::default().fg(Color::Cyan)),
        Span::styled(
            tool.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(label) = label.filter(|l| !l.is_empty()) {
        spans.push(Span::styled(
            format!("({label})"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    vec![Line::from(spans)]
}

/// A `⎿ summary` line committed when a tool call finishes, coloured by
/// `status` (`"error"` red, `"denied"` yellow, else dim).
pub(crate) fn render_tool_completed(status: &str, summary: &str) -> Vec<Line<'static>> {
    let color = match status {
        "error" => Color::Red,
        "denied" => Color::Yellow,
        _ => Color::DarkGray,
    };
    vec![Line::from(vec![
        Span::styled("  ⎿ ", Style::default().fg(Color::DarkGray)),
        Span::styled(summary.to_string(), Style::default().fg(color)),
    ])]
}

/// Render the non-text portion of a finalised assistant response. Text
/// blocks are skipped because they've already been streamed line-by-line
/// to scrollback; only blocks that aren't covered by the stream
/// (currently just the CronCreate hint via [`render_block`]) need to be
/// flushed at `Outgoing` time. `started` should mirror
/// `AppState::streaming_committed_any` so the prefix is correct: if the
/// response opened with no streamed text at all, the first non-text
/// line still gets the `aura> ` prefix.
pub(crate) fn render_non_text_blocks(blocks: &[ContentBlock], started: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut first = !started;
    for block in blocks {
        if matches!(block, ContentBlock::Text(_)) {
            continue;
        }
        let Some(rendered) = render_block(block) else {
            continue;
        };
        for line in rendered.lines() {
            let leader = if first {
                Span::styled(
                    "aura> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("      ")
            };
            out.push(Line::from(vec![leader, Span::raw(line.to_string())]));
            first = false;
        }
    }
    out
}

pub(crate) fn render_system_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|t| {
            Line::from(Span::styled(
                t.to_string(),
                Style::default().fg(Color::DarkGray),
            ))
        })
        .collect()
}

pub(crate) fn render_log_lines(record: &LogRecord) -> Vec<Line<'static>> {
    let (label, color) = match record.level {
        LogLevel::Error => ("error", Color::Red),
        LogLevel::Warn => ("warn ", Color::Yellow),
        LogLevel::Info => ("info ", Color::Cyan),
    };
    let prefix = Span::styled(
        format!("{label} "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    );
    let target = Span::styled(
        format!("[{}] ", record.target),
        Style::default().fg(Color::DarkGray),
    );
    let mut out = Vec::new();
    let mut iter = record.message.lines();
    if let Some(first) = iter.next() {
        out.push(Line::from(vec![
            prefix.clone(),
            target.clone(),
            Span::raw(first.to_string()),
        ]));
    } else {
        out.push(Line::from(vec![prefix, target]));
    }
    for rest in iter {
        out.push(Line::from(vec![
            Span::raw("      "),
            Span::raw(rest.to_string()),
        ]));
    }
    out
}

/// Lines for the *resolved* approval summary (single collapsed line).
pub(crate) fn render_approval_resolved_lines(entry: &ApprovalChatEntry) -> Vec<Line<'static>> {
    let ApprovalChatState::Resolved(decision) = entry.state else {
        return Vec::new();
    };
    let detail = access_summary(&entry.accesses);
    let (verb, color) = match decision {
        ApprovalDecision::Approve => ("approved", Color::Green),
        ApprovalDecision::ApproveAlways => ("approved (always)", Color::Green),
        ApprovalDecision::Deny => ("denied", Color::Red),
    };
    let mut spans = vec![
        Span::styled(
            "aura> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{verb}: {}", entry.tool),
            Style::default().fg(color),
        ),
    ];
    if !detail.is_empty() {
        spans.push(Span::styled(
            format!(" ({detail})"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    vec![Line::from(spans)]
}

/// Lines for the persistent banner committed once at startup (and on /new).
///
/// Renders a Codex-style rounded-corner box framing session info, followed
/// by a single dim hint line. `max_width` caps the rendered width so a
/// narrow terminal doesn't wrap the box-drawing characters; callers
/// typically pass `terminal.size()?.width`.
pub(crate) fn render_banner_lines(
    session_id: &str,
    cwd: &str,
    version: &str,
    max_width: u16,
) -> Vec<Line<'static>> {
    // Reserve 4 cols: 2 box edges + 1 padding each side.
    let usable = (max_width as usize).saturating_sub(4).max(20);
    let edge = Style::default().fg(Color::DarkGray);

    let header_full = format!("Aura TUI (v{version})");
    let session_text = clip_for_box(&format!("session:   {session_id}"), usable);
    let session_w = session_text.width();
    let dir_text = clip_for_box(&format!("directory: {cwd}"), usable);
    let dir_w = dir_text.width();
    // Width of the header is "> " + product+version; computed from the
    // composed string so the box sizes itself correctly.
    let header_visible = format!(">_ {header_full}");
    let header_w = header_visible.width().min(usable);

    let header_spans = vec![
        Span::styled(
            ">_ ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(header_full, Style::default().add_modifier(Modifier::BOLD)),
    ];

    let rows: Vec<(Vec<Span<'static>>, usize)> = vec![
        (header_spans, header_w),
        (vec![Span::raw("")], 0),
        (vec![Span::raw(session_text)], session_w),
        (vec![Span::raw(dir_text)], dir_w),
    ];

    let inner_w = rows.iter().map(|(_, w)| *w).max().unwrap_or(0);
    let frame_w = inner_w + 2;

    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 3);
    out.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(frame_w)),
        edge,
    )));
    for (spans, w) in rows {
        let pad_n = inner_w.saturating_sub(w);
        let mut row: Vec<Span<'static>> = Vec::with_capacity(spans.len() + 3);
        row.push(Span::styled("│ ", edge));
        row.extend(spans);
        row.push(Span::raw(" ".repeat(pad_n + 1)));
        row.push(Span::styled("│", edge));
        out.push(Line::from(row));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(frame_w)),
        edge,
    )));
    out.push(Line::from(Span::styled(
        "type / for commands · /quit or Ctrl-D twice to leave",
        Style::default().fg(Color::DarkGray),
    )));
    out
}

/// Truncate `text` so its display width fits `max_w`. When clipping a path
/// or session id, prefer to keep the tail (the most identifying part) and
/// elide the head with `…`. For other text we clip from the right with `…`
/// — this is a structural fallback for very narrow terminals.
fn clip_for_box(text: &str, max_w: usize) -> String {
    if text.width() <= max_w {
        return text.to_string();
    }
    if max_w <= 1 {
        return "…".to_string();
    }
    // For lines shaped as `key: value`, keep the tail of `value`.
    if let Some((label, value)) = text.split_once(':') {
        let label_w = label.width() + 2; // include ": "
        if label_w + 2 < max_w {
            let value_budget = max_w - label_w - 1; // 1 for the ellipsis
            let mut acc_w = 0usize;
            let mut start_byte = value.len();
            for (i, ch) in value.char_indices().rev() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if acc_w + cw > value_budget {
                    break;
                }
                acc_w += cw;
                start_byte = i;
            }
            return format!("{label}: …{}", &value[start_byte..].trim_start());
        }
    }
    // Fallback: right-clip with ellipsis.
    let budget = max_w.saturating_sub(1);
    let mut acc_w = 0usize;
    let mut end_byte = 0;
    for (i, ch) in text.char_indices() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if acc_w + cw > budget {
            break;
        }
        acc_w += cw;
        end_byte = i + ch.len_utf8();
    }
    format!("{}…", &text[..end_byte])
}

/// Lines for the *pending* approval block, drawn into the live viewport.
fn render_approval_pending_lines(entry: &ApprovalChatEntry) -> Vec<Line<'static>> {
    let ApprovalChatState::Pending { selected } = entry.state else {
        return Vec::new();
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(vec![
        Span::styled(
            "aura> ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("wants to run ", Style::default().fg(Color::Yellow)),
        Span::styled(
            entry.tool.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if !entry.accesses.is_empty() {
        out.push(Line::from(""));
        for acc in &entry.accesses {
            let mut spans = vec![Span::raw("      ")];
            spans.extend(format_access(acc));
            out.push(Line::from(spans));
        }
    }
    if !entry.params_preview.is_empty() {
        out.push(Line::from(""));
        for param_line in entry.params_preview.lines() {
            out.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(param_line.to_string(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    out.push(Line::from(""));
    let options = ["[a] Approve", "[A] Always approve", "[d] Deny"];
    for (i, label) in options.iter().enumerate() {
        let is_selected = i as u8 == selected;
        let spans = if is_selected {
            vec![
                Span::raw("      "),
                Span::styled(
                    "> ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    *label,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
            ]
        } else {
            vec![
                Span::raw("        "),
                Span::styled(*label, Style::default().fg(Color::DarkGray)),
            ]
        };
        out.push(Line::from(spans));
    }
    out
}

fn access_summary(accesses: &[ResourceAccess]) -> String {
    let Some(first) = accesses.first() else {
        return String::new();
    };
    match first {
        ResourceAccess::ReadFile { path } => path.display().to_string(),
        ResourceAccess::WriteFile { path } => path.display().to_string(),
        ResourceAccess::Http { host } => host.clone(),
        ResourceAccess::ExecCommand { command } => command.clone(),
        ResourceAccess::Env { vars } => vars.join(", "),
    }
}

fn format_access(acc: &ResourceAccess) -> Vec<Span<'static>> {
    let (verb, target) = match acc {
        ResourceAccess::ReadFile { path } => ("needs read access to", path.display().to_string()),
        ResourceAccess::WriteFile { path } => ("needs write access to", path.display().to_string()),
        ResourceAccess::Http { host } => {
            if host == "*" {
                ("needs network access", String::new())
            } else {
                ("needs network access to", host.clone())
            }
        }
        ResourceAccess::ExecCommand { command } => ("needs to run", command.clone()),
        ResourceAccess::Env { vars } => ("needs to read env vars", vars.join(", ")),
    };
    let mut spans = vec![
        Span::raw("  • "),
        Span::styled(
            verb,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !target.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(target));
    }
    spans
}

/// Count how many terminal rows a list of lines will occupy when rendered
/// with `Wrap { trim: false }` at the given width. Used to size the
/// `insert_before` buffer height so wrapped text isn't clipped.
pub(crate) fn wrapped_height(lines: &[Line<'_>], width: u16) -> u16 {
    if width == 0 {
        return lines.len() as u16;
    }
    let width = width as usize;
    let mut total: u16 = 0;
    for line in lines {
        let w = line.width().max(1);
        let rows = w.div_ceil(width).max(1) as u16;
        total = total.saturating_add(rows);
    }
    total.max(1)
}

/// Warning appended below a `CronCreate` tool call whose schedule recurs.
/// TUI sessions are ephemeral — triggers fired while the gateway is down
/// are lost, and a fresh TUI session will not replay them.
const TUI_CRON_RECURRING_HINT: &str = "⚠ This cron is tied to the TUI channel. \
    It only fires while `aura gateway` is running, and a new TUI session \
    will not replay triggers missed while the gateway was down. For \
    long-lived recurring jobs, prefer a persistent channel \
    (telegram/discord/http).";

fn render_block(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(text) => Some(text.clone()),
        ContentBlock::Image { mime_type, .. } => Some(format!("[Image: {mime_type}]")),
        ContentBlock::Audio { mime_type, .. } => Some(format!("[Audio: {mime_type}]")),
        ContentBlock::File {
            filename,
            mime_type,
            ..
        } => Some(format!("[File: {filename} ({mime_type})]")),
        ContentBlock::ToolUse { name, input, .. } => (name == "CronCreate"
            && input
                .get("schedule")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty()))
        .then(|| TUI_CRON_RECURRING_HINT.to_string()),
        ContentBlock::ToolResult { .. } => None,
        ContentBlock::Thinking { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Concatenate the textual content of every span on a line. Used to
    /// snapshot just the visible characters of a rendered banner.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn tool_started_line_shows_bullet_name_and_label() {
        let lines = render_tool_started("Read", Some("foo.rs"));
        let text = line_text(&lines[0]);
        assert!(text.starts_with("⏺ "), "got {text:?}");
        assert!(text.contains("Read"));
        assert!(text.contains("(foo.rs)"));
    }

    #[test]
    fn tool_started_line_omits_empty_label() {
        assert_eq!(line_text(&render_tool_started("now", None)[0]), "⏺ now");
        assert_eq!(line_text(&render_tool_started("now", Some(""))[0]), "⏺ now");
    }

    #[test]
    fn tool_completed_line_shows_connector_and_summary() {
        let text = line_text(&render_tool_completed("ok", "200 lines")[0]);
        assert!(text.contains('⎿'), "got {text:?}");
        assert!(text.contains("200 lines"));
    }

    #[test]
    fn reasoning_line_leader_differs_by_continuation() {
        let first = line_text(&render_reasoning_line("hmm", false)[0]);
        let cont = line_text(&render_reasoning_line("more", true)[0]);
        assert!(first.starts_with("✻ "), "got {first:?}");
        assert!(cont.starts_with("  ") && !cont.contains('✻'), "got {cont:?}");
    }

    #[test]
    fn banner_renders_top_bottom_and_aligned_content() {
        let out = render_banner_lines("sess-abc-12345", "/data/aura", "0.1.0", 80);
        // Structure: top edge, 4 content rows, bottom edge, hint = 7 lines.
        assert_eq!(out.len(), 7, "{:#?}", out);
        let texts: Vec<String> = out.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭') && texts[0].ends_with('╮'));
        assert!(texts[6].contains("Ctrl-D"));
        assert!(texts[1].contains(">_ Aura TUI (v0.1.0)"));
        assert!(texts[3].contains("session:"));
        assert!(texts[3].contains("sess-abc-12345"));
        assert!(texts[4].contains("directory:"));
        assert!(texts[4].contains("/data/aura"));
        let top_w = texts[0].chars().count();
        for (idx, t) in texts.iter().enumerate().take(6) {
            // Every framed row should have the same visual width as the top
            // edge so the right border lines up vertically.
            assert_eq!(t.chars().count(), top_w, "row {idx} width drifts: {t:?}");
        }
    }

    #[test]
    fn banner_clips_long_directory_with_left_ellipsis() {
        let long = "/some/very/very/very/very/deep/path/that/will/overflow/the/box";
        let out = render_banner_lines("s", long, "0.1.0", 50);
        let texts: Vec<String> = out.iter().map(line_text).collect();
        let dir_row = texts.iter().find(|t| t.contains("directory:")).unwrap();
        assert!(
            dir_row.contains('…'),
            "expected ellipsis in clipped directory row: {dir_row}"
        );
    }
}
