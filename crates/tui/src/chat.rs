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

use baybo_model::ContentBlock;
use baybo_tools::{ApprovalDecision, ResourceAccess};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::app::{AppState, ApprovalChatEntry, ApprovalChatState, RunningTool};
use crate::event::{LogLevel, LogRecord};
use crate::markdown::{self, Gutter};

/// Maximum rows the input box grows to before it clips. Beyond this, the
/// cursor may scroll off-screen.
pub(crate) const INPUT_MAX_ROWS: u16 = 10;

/// Render the live region for chat mode. The viewport now exactly matches the
/// needed height (working zone + queued lines + optional approval/completion +
/// input + optional model footer). Streaming text is committed to scrollback
/// line-by-line (Codex style) rather than buffered into a viewport preview.
pub(crate) fn render(frame: &mut Frame, area: Rect, state: &mut AppState) {
    // Build the live region top→bottom: the working zone (blank spacer +, while
    // busy, the in-flight tool list or the Cooking indicator), then any queued
    // lines, a pending approval block, the slash-completion popup, the input
    // box, and the model footer.
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

    render_working_zone(frame, chunks[0], state);
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

/// Rows the working zone occupies: always one spacer row, plus its content
/// (below) while a turn is in flight.
pub(crate) fn working_zone_height(state: &AppState) -> u16 {
    WORKING_SPACER_ROWS + working_content_height(state)
}

/// Rows the working zone's content (below the spacer) occupies while a turn is
/// in flight: one row per in-flight tool, stacked above the always-present
/// `Cooking…` indicator row (which carries the overall elapsed + `/stop` hint).
/// Zero when idle or while an approval owns the live region.
fn working_content_height(state: &AppState) -> u16 {
    if !state.show_working() {
        return 0;
    }
    state.running_tools().len() as u16 + WORKING_INDICATOR_ROWS
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

/// The live working zone, bottom-aligned in its reserved area (the row(s) above
/// stay blank — the breathing room it sits below). While a turn is in flight it
/// lists each **in-flight tool** as a colour-pulsing `● tool(label)` row, with
/// the `Cooking…` indicator (overall elapsed + `/stop` hint) as the bottom row
/// below them; with no tool running it's just that one `Cooking…` row. Renders
/// nothing when idle (the whole zone stays blank).
fn render_working_zone(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    // Paint the reserved spacer row(s) explicitly; relying on the default buffer
    // can leave the live content visually attached to the scrollback tail.
    frame.render_widget(Clear, area);
    if !state.show_working() {
        return;
    }
    let content_h = working_content_height(state);
    if content_h == 0 || content_h > area.height {
        return;
    }
    // Bottom-align the content; the rows above it are the reserved spacer.
    let top = area.y + area.height - content_h;
    let row = |i: u16| Rect {
        x: area.x,
        y: top + i,
        width: area.width,
        height: 1,
    };
    // In-flight tools stack above the Cooking… indicator, their dots pulsing in
    // unison with it (same `WORKING_PULSE` step) so the whole list reads as
    // actively working. The indicator owns the elapsed + `/stop`, so the tool
    // rows stay name-only.
    let running = state.running_tools();
    let pulse = WORKING_PULSE[state.spinner_tick % WORKING_PULSE.len()];
    for (i, t) in running.iter().enumerate() {
        let line = running_tool_line(&t.tool, t.label.as_deref(), pulse);
        frame.render_widget(Paragraph::new(line), row(i as u16));
    }
    let indicator = working_indicator_line(state.spinner_tick, state.working_elapsed_secs());
    frame.render_widget(Paragraph::new(indicator), row(running.len() as u16));
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

/// One live row for an in-flight tool: a `●` pulsing in unison with the working
/// clock (same `WORKING_PULSE` step as the Cooking… indicator, so the dot
/// breathes and the list reads as actively working), the cyan tool name, and the
/// dim label in parens. Otherwise mirrors [`render_tool_started`] (same glyph)
/// so the row settles into place when the block commits to scrollback — only the
/// dot's pulse stops. Pure so the styling is unit-testable without a frame.
pub(crate) fn running_tool_line(tool: &str, label: Option<&str>, pulse: Color) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{ANSWER_DOT} "),
            Style::default().fg(pulse).add_modifier(Modifier::BOLD),
        ),
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
/// `baybo> ` prefix). Continuation lines indent under it by [`ANSWER_INDENT`].
const ANSWER_DOT: &str = "●";
/// Continuation indent for assistant answers — the display width of `"● "`,
/// so wrapped/continued lines align under the text rather than the dot.
const ANSWER_INDENT: &str = "  ";

/// Display width of [`ANSWER_DOT`] plus its trailing space, reserved on every
/// answer row so wrapped markdown aligns under the text rather than the dot.
pub(crate) const ANSWER_LEADER_WIDTH: usize = 2;

/// Columns a rendered answer's markdown content may occupy at `width`.
pub(crate) fn answer_content_width(width: u16) -> usize {
    (width as usize).saturating_sub(ANSWER_LEADER_WIDTH).max(1)
}

/// Prefix already-wrapped markdown rows with the answer leader: the pale dot on
/// the response's very first row, the matching indent on every row after it.
/// `continuation` mirrors `AppState::streaming_committed_any`.
pub(crate) fn render_answer_rows(
    rows: Vec<Line<'static>>,
    continuation: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut first = !continuation;
    for row in rows {
        let mut spans = vec![answer_leader(!first)];
        spans.extend(row.spans);
        out.push(Line::from(spans));
        first = false;
    }
    out
}

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

/// Quote leader opening a user message's highlighted bar.
const USER_LEADER: &str = "> ";
/// Continuation indent for a user message's wrapped and subsequent rows.
const USER_INDENT: &str = "  ";

pub(crate) fn render_user_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    // The user's own messages render as a full-width highlighted bar: a `> `
    // quote leader on the first line, then the row padded out with spaces so
    // the background spans the whole line. `width` is the terminal width at
    // commit time. Explicit bright-white bg + black fg (rather than
    // `REVERSED`, which renders muted/grey on many terminals) keeps the bar
    // bright and theme-independent.
    //
    // Rows are wrapped here rather than by `Paragraph`: `wrapped_height` would
    // under-count the word-wrapped rows and the tail would be dropped from
    // scrollback, and only a display-width wrap breaks an unspaced Han message
    // at all.
    let style = Style::default().bg(Color::White).fg(Color::Black);
    let total = (width as usize).max(USER_LEADER.width());
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let gutter = if index == 0 {
            Gutter::hanging(vec![Span::raw(USER_LEADER)], vec![Span::raw(USER_INDENT)])
        } else {
            Gutter::uniform(vec![Span::raw(USER_INDENT)])
        };
        for row in markdown::wrap(&[Span::raw(line.to_string())], total, &gutter) {
            let body: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            out.push(Line::from(Span::styled(markdown::pad(&body, total), style)));
        }
    }
    if out.is_empty() {
        out.push(Line::from(Span::styled(
            markdown::pad(USER_LEADER, total),
            style,
        )));
    }
    out
}

/// A whole assistant response rendered from its final blocks, for a delivery
/// that never streamed (a cron trigger, or a synthetic completion message).
pub(crate) fn render_assistant_lines(blocks: &[ContentBlock], width: u16) -> Vec<Line<'static>> {
    render_answer_rows(block_rows(blocks, false, width), false)
}

/// Markdown-render the text blocks and wrap the rest, separating blocks with a
/// blank row. `text_only` skips [`ContentBlock::Text`], for a response whose
/// prose already streamed.
fn block_rows(blocks: &[ContentBlock], skip_text: bool, width: u16) -> Vec<Line<'static>> {
    let content = answer_content_width(width);
    let mut out: Vec<Line<'static>> = Vec::new();
    for block in blocks {
        let is_text = matches!(block, ContentBlock::Text(_));
        if skip_text && is_text {
            continue;
        }
        let Some(rendered) = block_source_text(block) else {
            continue;
        };
        let rows = if is_text {
            markdown::render_document(&rendered, content)
        } else {
            markdown::wrap(&[Span::raw(rendered)], content, &Gutter::none())
        };
        if rows.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.extend(rows);
    }
    out
}

/// The `● tool(label)` head line of a tool block. The bullet matches the
/// assistant answer's dot (`●`, same size) but in cyan; the optional human
/// label is dim in parens. While the call is in flight this same shape renders
/// live (via [`running_tool_line`]); it commits to scrollback as part of
/// [`tool_completed_block`] when the call finishes.
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

/// Assemble a completed tool's full scrollback block, committed as one unit when
/// `ToolCompleted` lands: the deferred `● tool` head line (from the in-flight
/// [`RunningTool`]), any approval resolved mid-call, then the `⎿` result. This
/// pairing is what keeps a result directly under its own tool even when sibling
/// tools in the same response ran concurrently (the agent emits every
/// `ToolStarted` up front, then every `ToolCompleted`, so committing the head
/// line at start would strand the results below all the heads in append-only
/// scrollback). The caller only builds this for a completion whose `call_id`
/// still matches a tracked tool; a stale completion (e.g. after `/stop` cleared
/// the running set) is dropped at the call site rather than rendered headless.
pub(crate) fn tool_completed_block(
    running: &RunningTool,
    status: &str,
    summary: &str,
) -> Vec<Line<'static>> {
    let mut lines = render_tool_started(&running.tool, running.label.as_deref());
    lines.extend(running.approval_lines.iter().cloned());
    lines.extend(render_tool_completed(status, summary));
    lines
}

/// Footer committed when a turn (turn) finishes: a "cooked for <duration>"
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
/// (currently just the CronCreate hint via [`block_source_text`]) need to be
/// flushed at `Outgoing` time. `started` should mirror
/// `AppState::streaming_committed_any` so the leader is correct: if the
/// response opened with no streamed text at all, the first non-text line
/// still gets the pale answer dot.
pub(crate) fn render_non_text_blocks(
    blocks: &[ContentBlock],
    started: bool,
    width: u16,
) -> Vec<Line<'static>> {
    render_answer_rows(block_rows(blocks, true, width), started)
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
    version: &str,
    max_width: u16,
) -> Vec<Line<'static>> {
    // Reserve 4 cols: 2 box edges + 1 padding each side.
    let usable = (max_width as usize).saturating_sub(4).max(20);
    let edge = Style::default().fg(Color::DarkGray);

    let header_full = format!("Baybo TUI (v{version})");
    let session_text = clip_for_box(&format!("session: {session_id}"), usable);
    let session_w = session_text.width();
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

/// Truncate `text` so its display width fits `max_w`. When clipping a session
/// id, prefer to keep the tail (the most identifying part) and elide the head
/// with `…`. For other text we clip from the right with `…` — this is a
/// structural fallback for very narrow terminals.
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
            return format!("{label}: …{}", value[start_byte..].trim_start());
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
    It only fires while `baybo gateway` is running, and a new TUI session \
    will not replay triggers missed while the gateway was down. For \
    long-lived recurring turns, prefer a persistent channel \
    (telegram/discord/http).";

/// The markdown (or placeholder) source a content block contributes to an
/// answer. `None` for a block that renders nothing.
pub(crate) fn block_source_text(block: &ContentBlock) -> Option<String> {
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
        // The `Cooking…` line is the no-tool-running indicator; in-flight tools
        // get their own `running_tool_line` rows instead.
        let text = line_text(&working_indicator_line(0, 3));
        assert!(text.starts_with("● "), "leads with the dot: {text:?}");
        assert!(text.contains("Cooking…"), "got {text:?}");
        assert!(
            !text.contains("Read"),
            "no tool name on the status line: {text:?}"
        );
        assert!(text.contains("(3s · /stop to interrupt)"), "got {text:?}");
    }

    #[test]
    fn running_tool_line_pulses_dot_and_shows_name_label() {
        // Same glyphs each tick (name-only `● tool(label)`), but the dot takes
        // the passed pulse colour so it breathes with the Cooking… indicator.
        let a = running_tool_line("Read", Some("foo.rs"), Color::DarkGray);
        let b = running_tool_line("Read", Some("foo.rs"), Color::White);
        assert_eq!(line_text(&a), "● Read(foo.rs)");
        assert_eq!(
            line_text(&a),
            line_text(&b),
            "glyphs are stable across ticks"
        );
        assert_ne!(
            a.spans[0].style.fg, b.spans[0].style.fg,
            "only the dot colour pulses"
        );
    }

    #[test]
    fn tool_completed_block_pairs_result_directly_under_its_tool() {
        let rt = RunningTool {
            call_id: "c1".into(),
            tool: "Bash".into(),
            label: Some("ls".into()),
            approval_lines: Vec::new(),
        };
        let block = tool_completed_block(&rt, "ok", "5 lines");
        assert_eq!(block.len(), 2, "head + result");
        assert!(
            line_text(&block[0]).starts_with("● Bash"),
            "{:?}",
            line_text(&block[0])
        );
        assert!(
            line_text(&block[1]).contains("⎿ 5 lines"),
            "{:?}",
            line_text(&block[1])
        );
    }

    #[test]
    fn tool_completed_block_puts_resolved_approval_between_head_and_result() {
        let rt = RunningTool {
            call_id: "c1".into(),
            tool: "Bash".into(),
            label: None,
            approval_lines: vec![Line::from("● approved: Bash")],
        };
        let block = tool_completed_block(&rt, "ok", "done");
        assert_eq!(block.len(), 3, "head + approval + result");
        assert!(line_text(&block[0]).starts_with("● Bash"));
        assert_eq!(line_text(&block[1]), "● approved: Bash");
        assert!(line_text(&block[2]).contains("⎿ done"));
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
    fn working_zone_lists_running_tools_above_cooking_indicator() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = AppState::new();
        app.note_response_pending();
        app.push_running_tool("c1".into(), "Bash".into(), Some("ls -la".into()));
        app.push_running_tool("c2".into(), "Read".into(), Some("hosts".into()));

        // spacer + one row per running tool + the Cooking… indicator row.
        assert_eq!(working_zone_height(&app), 4);

        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal.draw(|f| render(f, f.area(), &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        assert!(
            buffer_row_text(buf, 0).trim().is_empty(),
            "spacer row stays blank: {:?}",
            buffer_row_text(buf, 0)
        );
        assert!(
            buffer_row_text(buf, 1).contains("Bash"),
            "first in-flight tool: {:?}",
            buffer_row_text(buf, 1)
        );
        assert!(
            buffer_row_text(buf, 2).contains("Read"),
            "second in-flight tool: {:?}",
            buffer_row_text(buf, 2)
        );
        let indicator = buffer_row_text(buf, 3);
        assert!(
            indicator.contains("Cooking…") && indicator.contains("/stop to interrupt"),
            "Cooking indicator (elapsed + /stop) sits below the list: {indicator:?}"
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
            baybo_channels::SlashCommand::new("/clear", "clear"),
            baybo_channels::SlashCommand::new("/quit", "quit"),
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
            baybo_channels::SlashCommand::new("/clear", "clear"),
            baybo_channels::SlashCommand::new("/quit", "quit"),
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
        let answer = render_answer_rows(vec![Line::from("answer")], false);
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
    fn a_han_user_message_wraps_instead_of_losing_its_tail() {
        // Before wrapping here, this went out as one over-wide logical line and
        // `wrapped_height` under-counted the rows `Paragraph` needed, so the
        // tail was dropped from scrollback outright.
        let text = "帮我把这个函数重构一下谢谢";
        for width in 8u16..40 {
            let lines = render_user_lines(text, width);
            let joined: String = lines.iter().map(line_text).collect();
            for ch in text.chars() {
                assert!(
                    joined.contains(ch),
                    "width {width} lost {ch:?} from {joined:?}"
                );
            }
            for line in &lines {
                assert_eq!(
                    line_text(line).width(),
                    width as usize,
                    "every row fills the bar exactly at width {width}"
                );
            }
        }
    }

    #[test]
    fn a_long_user_message_wraps_onto_more_rows() {
        let lines = render_user_lines("alpha bravo charlie delta echo foxtrot", 16);
        assert!(lines.len() > 1, "a message wider than the bar must wrap");
        assert!(line_text(&lines[0]).starts_with("> "));
        for line in lines.iter().skip(1) {
            assert!(line_text(line).starts_with("  "), "{:?}", line_text(line));
        }
    }

    #[test]
    fn answer_rows_use_the_dot_then_the_indent() {
        let rows = vec![Line::from("hi"), Line::from("there")];
        let fresh = render_answer_rows(rows.clone(), false);
        assert!(
            line_text(&fresh[0]).starts_with("● "),
            "the response's first row gets the dot: {:?}",
            line_text(&fresh[0])
        );
        assert!(
            line_text(&fresh[1]).starts_with("  ") && !line_text(&fresh[1]).contains('●'),
            "later rows indent, no dot: {:?}",
            line_text(&fresh[1])
        );
        let continued = render_answer_rows(rows, true);
        assert!(
            continued
                .iter()
                .all(|r| line_text(r).starts_with("  ") && !line_text(r).contains('●')),
            "a continued response never re-draws the dot"
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
        let out = render_banner_lines("sess-abc-12345", "0.1.0", 80);
        // Structure: top edge, 3 content rows (header, blank, session), bottom
        // edge, hint = 6 lines. The directory row is intentionally omitted.
        assert_eq!(out.len(), 6, "{:#?}", out);
        let texts: Vec<String> = out.iter().map(line_text).collect();
        assert!(texts[0].starts_with('╭') && texts[0].ends_with('╮'));
        assert!(texts[5].contains("Ctrl-D"));
        assert!(texts[1].contains(">_ Baybo TUI (v0.1.0)"));
        assert!(texts[3].contains("session:"));
        assert!(texts[3].contains("sess-abc-12345"));
        assert!(
            !texts.iter().any(|t| t.contains("directory:")),
            "directory row must not be rendered: {texts:#?}"
        );
        let top_w = texts[0].chars().count();
        for (idx, t) in texts.iter().enumerate().take(5) {
            // Every framed row should have the same visual width as the top
            // edge so the right border lines up vertically.
            assert_eq!(t.chars().count(), top_w, "row {idx} width drifts: {t:?}");
        }
    }

    #[test]
    fn banner_clips_long_session_with_left_ellipsis() {
        let long = "session-id-that-is-far-too-long-to-fit-inside-the-narrow-banner-box";
        let out = render_banner_lines(long, "0.1.0", 50);
        let texts: Vec<String> = out.iter().map(line_text).collect();
        let session_row = texts.iter().find(|t| t.contains("session:")).unwrap();
        assert!(
            session_row.contains('…'),
            "expected ellipsis in clipped session row: {session_row}"
        );
    }
}
