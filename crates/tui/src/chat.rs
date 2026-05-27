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

use std::time::Duration;

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

/// Render the live region for chat mode. The viewport now exactly matches the
/// needed height (working zone + queued lines + optional approval/completion +
/// input + optional model footer). Streaming text is committed to scrollback
/// line-by-line (Codex style) rather than buffered into a viewport preview.
pub(crate) fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    // Build the live region top→bottom: a blank spacer, the working indicator
    // row only while busy, then any queued lines, a pending approval block,
    // the slash-completion popup, the input box, and the model footer.
    let queued: Vec<&str> = state.queued_display_lines().collect();
    let approval = state.pending_approval.as_ref();
    // The slash-completion popup is a real section just above the input box,
    // not a float — an inline viewport's buffer doesn't extend above itself,
    // so drawing above it would write out of bounds.
    let popup_h = completion_popup_height(state);
    let input_h = input_box_height(state);
    let model_h = model_footer_height(state);

    let mut constraints = Vec::with_capacity(queued.len() + 5);
    constraints.push(Constraint::Length(working_zone_height(state)));
    for _ in &queued {
        constraints.push(Constraint::Length(1));
    }
    if let Some(entry) = approval {
        constraints.push(Constraint::Length(approval_pending_height(entry)));
    }
    if popup_h > 0 {
        constraints.push(Constraint::Length(popup_h));
    }
    constraints.push(Constraint::Length(input_h));
    if model_h > 0 {
        constraints.push(Constraint::Length(model_h));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    render_working_line(frame, chunks[0], state);
    let mut next = 1;
    for line in &queued {
        render_queued_line(frame, chunks[next], line);
        next += 1;
    }
    if let Some(entry) = approval {
        render_approval_block(frame, chunks[next], entry);
        next += 1;
    }
    if popup_h > 0 {
        render_completion_popup(frame, chunks[next], state);
        next += 1;
    }
    render_input(frame, chunks[next], state);
    next += 1;
    if model_h > 0 {
        render_model_footer(frame, chunks[next], state);
    }
}

const WORKING_SPACER_ROWS: u16 = 1;
const WORKING_INDICATOR_ROWS: u16 = 1;

/// Rows the working zone occupies: one spacer row while idle, and spacer plus
/// indicator row while a turn is actively working.
pub(crate) fn working_zone_height(state: &AppState) -> u16 {
    WORKING_SPACER_ROWS
        + if state.show_working() {
            WORKING_INDICATOR_ROWS
        } else {
            0
        }
}

/// Colours the working dot pulses through, one step per animation tick — a
/// gentle dim→bright→dim breath. Driven by repainting (the elapsed-counter
/// tick), not terminal blink, so it animates everywhere.
const WORKING_PULSE: &[Color] = &[
    Color::DarkGray,
    Color::Cyan,
    Color::White,
    Color::White,
    Color::Cyan,
    Color::DarkGray,
];

/// The status word shown when the agent is working but no tool is running.
const WORKING_WORD: &str = "Cooking";

/// Hint appended to the working line; `/stop` cancels the in-flight turn.
const STOP_HINT: &str = "/stop to interrupt";

/// The animated working indicator, rendered on the **bottom** row of the
/// reserved working zone while a turn is in flight (the row(s) above stay
/// blank — the breathing room the indicator sits below). A colour-pulsing `●`
/// — same glyph/size as the answer dot, so it never reflows — followed by
/// `Cooking…`, then the live elapsed counter + `/stop` hint. Renders nothing
/// when idle (the whole zone stays blank).
fn render_working_line(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    // Paint the reserved spacer row explicitly; relying on the default buffer
    // can leave the live indicator visually attached to the scrollback tail.
    frame.render_widget(Clear, area);
    if !state.show_working() {
        return;
    }
    let line = working_indicator_line(state.spinner_tick, state.working_elapsed_secs());
    let indicator_row = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    frame.render_widget(Paragraph::new(line), indicator_row);
}

/// Build the working line. Pure so the colour pulse and `(Ns · …)` readout can
/// be unit-tested without a frame.
pub(crate) fn working_indicator_line(spinner_tick: usize, elapsed_secs: u64) -> Line<'static> {
    let accent = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    // Equal-width, dot-sized `●` (the same glyph as answers/tools), animated
    // by cycling its colour each tick — visible even where blink isn't.
    let pulse = WORKING_PULSE[spinner_tick % WORKING_PULSE.len()];
    let mut spans = vec![Span::styled(
        format!("{ANSWER_DOT} "),
        Style::default().fg(pulse).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled(format!("{WORKING_WORD}…"), accent));
    spans.push(Span::styled(
        format!(" ({elapsed_secs}s · {STOP_HINT})"),
        dim,
    ));
    Line::from(spans)
}

/// One `↳ <text>` line under the working indicator for a message the user
/// queued mid-turn (a dispatched steer awaiting a tool boundary, or a
/// deferred slash awaiting turn end). Newlines are flattened to keep it a
/// single row; the terminal truncates an over-long preview.
fn render_queued_line(frame: &mut Frame, area: Rect, text: &str) {
    if area.height == 0 {
        return;
    }
    let preview = text.replace('\n', " ");
    let line = Line::from(vec![
        Span::styled("↳ ", Style::default().fg(Color::Cyan)),
        Span::styled(preview, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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

pub(crate) fn model_footer_height(state: &AppState) -> u16 {
    if state.model_label.is_some() { 1 } else { 0 }
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let hint = Line::from(Span::styled(
        " shift+enter · alt+enter = newline ",
        Style::default().fg(Color::DarkGray),
    ))
    .right_aligned();
    // Queued submissions surface as `↳` lines under the working
    // indicator (see `render`), so the title stays a plain label.
    let block = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::horizontal(1))
        .title(" input ")
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

const MODEL_FOOTER_PREFIX: &str = "model: ";
const MODEL_FOOTER_INDENT: &str = "  ";

fn render_model_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let Some(label) = state.model_label.as_deref() else {
        return;
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(model_footer_line(label)), area);
}

pub(crate) fn model_footer_line(label: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw(MODEL_FOOTER_INDENT),
        Span::styled(MODEL_FOOTER_PREFIX, Style::default().fg(Color::DarkGray)),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Max command rows the completion popup shows before it stops growing.
const COMPLETION_MAX_ROWS: usize = 8;

/// Rows the slash-completion popup occupies (0 when not showing). It is a
/// layout section just above the input box — sized into the inline viewport
/// so it never draws outside the viewport buffer.
pub(crate) fn completion_popup_height(state: &AppState) -> u16 {
    let n = state.completion_candidates().len();
    if n == 0 {
        0
    } else {
        // +2 for the bordered block (top + bottom).
        (n.min(COMPLETION_MAX_ROWS) as u16).saturating_add(2)
    }
}

fn render_completion_popup(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let candidates = state.completion_candidates();
    if candidates.is_empty() {
        return;
    }
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

    frame.render_widget(Clear, area);
    frame.render_stateful_widget(list, area, &mut list_state);
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

/// Pale dot marking an assistant answer's first line (replaces the old
/// `aura> ` prefix). Continuation lines indent under it by [`ANSWER_INDENT`].
const ANSWER_DOT: &str = "●";
/// Continuation indent for assistant answers — the display width of `"● "`,
/// so wrapped/continued lines align under the text rather than the dot.
const ANSWER_INDENT: &str = "  ";

/// Leading span for an assistant answer line: a pale dot on the first line,
/// a matching blank indent on continuations.
fn answer_leader(is_continuation: bool) -> Span<'static> {
    if is_continuation {
        Span::raw(ANSWER_INDENT)
    } else {
        Span::styled(
            format!("{ANSWER_DOT} "),
            Style::default().fg(Color::DarkGray),
        )
    }
}

pub(crate) fn render_user_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    // The user's own messages render as a full-width highlighted bar: a `> `
    // quote leader on the first line, then the row padded out with spaces so
    // the background spans the whole line. `width` is the terminal width at
    // commit time. Explicit bright-white bg + black fg (rather than
    // `REVERSED`, which renders muted/grey on many terminals) keeps the bar
    // bright and theme-independent.
    let style = Style::default().bg(Color::White).fg(Color::Black);
    let total = width as usize;
    let mut out = Vec::new();
    let mut first = true;
    for line in text.lines() {
        let body = format!("{}{line}", if first { "> " } else { "  " });
        out.push(Line::from(Span::styled(pad_to_width(body, total), style)));
        first = false;
    }
    if first {
        out.push(Line::from(Span::styled(
            pad_to_width("> ".to_string(), total),
            style,
        )));
    }
    out
}

/// Right-pad `s` with spaces so its display width fills `width` columns (a
/// no-op when it already meets or exceeds it), so a styled background covers
/// the whole row.
fn pad_to_width(s: String, width: usize) -> String {
    let w = s.width();
    if w >= width {
        s
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

pub(crate) fn render_assistant_lines(blocks: &[ContentBlock]) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut first = true;
    for block in blocks {
        let Some(rendered) = render_block(block) else {
            continue;
        };
        for (i, text) in rendered.lines().enumerate() {
            out.push(Line::from(vec![
                answer_leader(!(first && i == 0)),
                Span::raw(text.to_string()),
            ]));
        }
        first = false;
    }
    out
}

/// One rendered line of a streaming agent response. The first line of the
/// response carries the pale answer dot; every subsequent line uses the
/// matching continuation indent so the answer reads as one coherent block.
/// Callers set `is_continuation` based on `AppState::streaming_committed_any`
/// — see [`crate::app`].
pub(crate) fn render_stream_line(text: &str, is_continuation: bool) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        answer_leader(is_continuation),
        Span::raw(text.to_string()),
    ])]
}

/// A `● tool(label)` line committed the moment a tool call is dispatched.
/// The bullet matches the assistant answer's dot (`●`, same size) but in
/// cyan; the optional human label is dim in parens.
pub(crate) fn render_tool_started(tool: &str, label: Option<&str>) -> Vec<Line<'static>> {
    let mut spans = vec![
        Span::styled(format!("{ANSWER_DOT} "), Style::default().fg(Color::Cyan)),
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

/// Footer committed when a turn (job) finishes: a "cooked for <duration>"
/// stamp of the wall-clock the turn took.
pub(crate) fn render_cooked_for_line(elapsed: Duration) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        format!("{ANSWER_DOT} cooked for {}", format_duration(elapsed)),
        Style::default(),
    ))]
}

/// Compact wall-clock rendering for the turn-done footer: `8s`, `1m 05s`.
fn format_duration(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// A dim turn-status line (`⟳ …`) for a coarse phase transition — today
/// context compaction start/end. Unknown phases render verbatim.
pub(crate) fn render_status_line(phase: &str) -> Vec<Line<'static>> {
    let text = match phase {
        "compacting" => "Compacting context…",
        "compacted" => "Context compacted",
        other => other,
    };
    let dim = Style::default().fg(Color::DarkGray);
    vec![Line::from(vec![
        Span::styled("⟳ ", dim),
        Span::styled(text.to_string(), dim),
    ])]
}

/// Render the non-text portion of a finalised assistant response. Text
/// blocks are skipped because they've already been streamed line-by-line
/// to scrollback; only blocks that aren't covered by the stream
/// (currently just the CronCreate hint via [`render_block`]) need to be
/// flushed at `Outgoing` time. `started` should mirror
/// `AppState::streaming_committed_any` so the leader is correct: if the
/// response opened with no streamed text at all, the first non-text line
/// still gets the pale answer dot.
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
            out.push(Line::from(vec![
                answer_leader(!first),
                Span::raw(line.to_string()),
            ]));
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
        answer_leader(false),
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
        answer_leader(false),
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

    fn buffer_row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
            .collect()
    }

    #[test]
    fn tool_started_line_shows_bullet_name_and_label() {
        let lines = render_tool_started("Read", Some("foo.rs"));
        let text = line_text(&lines[0]);
        // Same dot as the assistant answer (`●`), so they line up in size.
        assert!(text.starts_with("● "), "got {text:?}");
        assert!(text.contains("Read"));
        assert!(text.contains("(foo.rs)"));
    }

    #[test]
    fn tool_started_line_omits_empty_label() {
        assert_eq!(line_text(&render_tool_started("now", None)[0]), "● now");
        assert_eq!(line_text(&render_tool_started("now", Some(""))[0]), "● now");
    }

    #[test]
    fn working_indicator_shows_cooking_when_idle() {
        let text = line_text(&working_indicator_line(0, 0));
        assert!(text.starts_with("● "), "leads with the dot: {text:?}");
        assert!(text.contains("Cooking…"), "got {text:?}");
        assert!(text.contains("(0s · /stop to interrupt)"), "got {text:?}");
    }

    #[test]
    fn working_indicator_dot_pulses_colour_with_tick() {
        // The dot is the same glyph each tick (equal-width, no reflow) but a
        // different colour — a repaint-driven pulse, not terminal blink.
        let a = working_indicator_line(0, 0);
        let b = working_indicator_line(1, 0);
        assert_eq!(line_text(&a).chars().next(), line_text(&b).chars().next());
        assert_ne!(a.spans[0].style.fg, b.spans[0].style.fg);
    }

    #[test]
    fn working_indicator_stays_status_only() {
        let text = line_text(&working_indicator_line(0, 3));
        assert!(text.starts_with("● "), "leads with the dot: {text:?}");
        assert!(text.contains("Cooking…"), "got {text:?}");
        assert!(!text.contains("Read"), "tools render as messages: {text:?}");
        assert!(text.contains("(3s · /stop to interrupt)"), "got {text:?}");
    }

    #[test]
    fn render_keeps_blank_row_above_working_indicator() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = AppState::new();
        app.note_response_pending();
        let mut terminal = Terminal::new(TestBackend::new(48, 6)).unwrap();

        terminal.draw(|f| render(f, f.area(), &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let spacer = buffer_row_text(buf, 0);
        let indicator = buffer_row_text(buf, 1);
        assert!(
            spacer.trim().is_empty(),
            "spacer row must stay blank: {spacer:?}"
        );
        assert!(
            indicator.trim_start().starts_with("● Cooking"),
            "working indicator should render below spacer: {indicator:?}"
        );
    }

    #[test]
    fn render_idle_keeps_single_blank_row_above_input() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = AppState::new();
        let mut terminal = Terminal::new(TestBackend::new(48, 4)).unwrap();

        terminal.draw(|f| render(f, f.area(), &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let spacer = buffer_row_text(buf, 0);
        let input_top = buffer_row_text(buf, 1);
        assert!(
            spacer.trim().is_empty(),
            "idle spacer row must stay blank: {spacer:?}"
        );
        assert!(
            input_top.contains("input"),
            "input should start directly below one spacer row: {input_top:?}"
        );
    }

    #[test]
    fn render_model_footer_below_input_when_model_is_known() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = AppState::new();
        app.set_model_label(Some("openai/gpt-5-codex".into()));
        let mut terminal = Terminal::new(TestBackend::new(48, 5)).unwrap();

        terminal.draw(|f| render(f, f.area(), &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let footer = buffer_row_text(buf, 4);
        assert_eq!(model_footer_height(&app), 1);
        assert!(
            footer.starts_with("  model: openai/gpt-5-codex"),
            "model footer should render below input: {footer:?}"
        );
    }

    #[test]
    fn completion_popup_height_counts_candidates_plus_border() {
        let mut app = AppState::new();
        app.set_commands(vec![
            aura_channels::SlashCommand::new("/clear", "clear"),
            aura_channels::SlashCommand::new("/quit", "quit"),
        ]);
        assert_eq!(completion_popup_height(&app), 0, "no popup on empty input");
        app.insert_char('/');
        assert_eq!(
            completion_popup_height(&app),
            4,
            "2 candidates + 2 border rows"
        );
    }

    #[test]
    fn render_with_slash_completion_stays_in_bounds() {
        // Regression: the completion popup is a layout section inside the
        // viewport, not an out-of-bounds float above it, so rendering it must
        // not panic even in a short area.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = AppState::new();
        app.set_commands(vec![
            aura_channels::SlashCommand::new("/clear", "clear"),
            aura_channels::SlashCommand::new("/quit", "quit"),
        ]);
        app.insert_char('/');
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        terminal.draw(|f| render(f, f.area(), &mut app)).unwrap();
    }

    #[test]
    fn cooked_for_line_has_dot_answer_text_colour_and_duration() {
        let line = &render_cooked_for_line(std::time::Duration::from_secs(75))[0];
        let text = line_text(line);
        assert!(text.starts_with("● cooked for "), "got {text:?}");
        assert!(text.contains("1m 15s"), "minutes formatting: {text:?}");
        let answer = render_stream_line("answer", false);
        let answer_text_fg = answer[0].spans[1].style.fg;
        assert!(
            line.spans.iter().all(|s| s.style.fg == answer_text_fg),
            "footer should match answer text foreground"
        );
    }

    #[test]
    fn user_lines_are_full_width_highlighted_with_quote_prefix() {
        let width = 20u16;
        let lines = render_user_lines("hello\nworld", width);
        assert_eq!(lines.len(), 2);
        assert!(
            line_text(&lines[0]).starts_with("> hello"),
            "first line quotes: {:?}",
            line_text(&lines[0])
        );
        for l in &lines {
            let text = line_text(l);
            assert_eq!(text.width(), width as usize, "bar fills the row: {text:?}");
            assert!(
                l.spans.iter().all(|s| s.style.bg == Some(Color::White)),
                "user line must carry the bright bar background: {text:?}"
            );
        }
    }

    #[test]
    fn stream_line_uses_answer_dot_then_indent() {
        let first = line_text(&render_stream_line("hi", false)[0]);
        let cont = line_text(&render_stream_line("more", true)[0]);
        assert!(
            first.starts_with("● "),
            "first line gets the dot: {first:?}"
        );
        assert!(
            cont.starts_with("  ") && !cont.contains('●'),
            "continuation indents, no dot: {cont:?}"
        );
    }

    #[test]
    fn tool_completed_line_shows_connector_and_summary() {
        let text = line_text(&render_tool_completed("ok", "200 lines")[0]);
        assert!(text.contains('⎿'), "got {text:?}");
        assert!(text.contains("200 lines"));
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
