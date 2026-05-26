//! TUI application state: live viewport content, input buffer, input history,
//! view mode.
//!
//! State mutation is synchronous and side-effect-free. With the inline-viewport
//! rendering model, chat history is *not* held here — it lives in the
//! terminal's own scrollback buffer, written via [`ratatui::Terminal::insert_before`].
//! This module only tracks what's currently *live* in the viewport: the input
//! draft, the streaming-response preview, and the (single) pending approval.

use std::collections::VecDeque;
use std::time::Instant;

use aura_channels::{DashboardSnapshot, SlashCommand, ViewKind};
use ratatui::widgets::TableState;

use aura_tools::{ApprovalDecision, ApprovalQueue, ResourceAccess};

const HISTORY_CAP: usize = 500;

/// An approval request rendered inline in the viewport (when pending) and
/// committed as a one-line summary to scrollback (once resolved).
#[derive(Debug, Clone)]
pub(crate) struct ApprovalChatEntry {
    pub tool: String,
    pub accesses: Vec<ResourceAccess>,
    pub params_preview: String,
    pub state: ApprovalChatState,
}

/// State of an inline approval entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalChatState {
    /// Awaiting user decision. `selected` indexes the option list
    /// (0 = Approve, 1 = Always, 2 = Deny).
    Pending { selected: u8 },
    /// User made a decision; the entry renders as a collapsed summary line.
    Resolved(ApprovalDecision),
}

/// Top-level view the TUI is displaying.
pub(crate) enum ViewMode {
    Chat,
    Dashboard {
        kind: ViewKind,
        snapshot: DashboardSnapshot,
        table_state: TableState,
    },
}

pub(crate) struct AppState {
    pub(crate) mode: ViewMode,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) history: VecDeque<String>,
    pub(crate) history_cursor: Option<usize>,
    /// Trailing partial line of the in-flight agent response — the
    /// suffix that hasn't yet been terminated by a `\n` and committed
    /// to scrollback. Each `AppEvent::StreamDelta` appends to it, and
    /// [`drain_complete_stream_lines`] pops everything up to (and
    /// including) the most recent `\n`. The final `AppEvent::Outgoing`
    /// flushes whatever remains.
    pub(crate) streaming: Option<String>,
    /// Whether the current agent response has already committed any
    /// line to scrollback. Drives the prefix choice: the very first
    /// committed line uses `aura> ` (bold green), subsequent lines use
    /// the `      ` (six-space) continuation indent so the conversation
    /// reads as one coherent block.
    pub(crate) streaming_committed_any: bool,
    /// Trailing partial line of the in-flight reasoning ("thinking")
    /// trace, mirroring [`streaming`] but for dim reasoning lines. Each
    /// `AppEvent::Reasoning` appends; complete lines commit dim as they
    /// form, and the partial flushes before the answer / a tool line.
    pub(crate) reasoning: Option<String>,
    /// Whether the current reasoning run has committed any line — drives
    /// the dim `✻ ` leader (first line) vs the continuation indent. Reset
    /// once a non-reasoning line (tool / answer) ends the run.
    pub(crate) reasoning_committed_any: bool,
    /// Active inline approval prompt. At most one is live at a time; further
    /// queued requests stay on [`ApprovalQueue`] until the head is resolved.
    pub(crate) pending_approval: Option<ApprovalChatEntry>,
    /// Full list of slash commands for completion; sourced once at startup
    /// from `SlashHandler::commands()`.
    pub(crate) commands: Vec<SlashCommand>,
    /// Selection cursor within the currently filtered completion list.
    pub(crate) completion_cursor: usize,
    /// Ctrl-D exit confirmation gate. Records the timestamp of the first
    /// Ctrl-D press with an empty input; a second Ctrl-D within the window
    /// (see [`CONFIRM_EXIT_WINDOW`]) commits the exit. Cleared by any other
    /// key so the confirmation doesn't linger across unrelated input.
    pub(crate) confirm_exit_at: Option<Instant>,
    /// Shared pending-approval queue. Cloned into the event loop so key
    /// handlers can drain entries. `None` means approval gating is disabled
    /// for the test harness — production always wires this up.
    pub(crate) approval: Option<ApprovalQueue>,
    /// User submissions parked while the agent is mid-response or a
    /// resource approval is pending. Each `AppEvent::Outgoing` pops the
    /// next one and dispatches it, so messages always commit to
    /// scrollback in chronological send order
    /// (`you>1 / aura>1 / you>2 / aura>2`) instead of interleaving with
    /// in-flight streams.
    pub(crate) outgoing_queue: VecDeque<String>,
    /// User messages that have been dispatched to the agent but whose
    /// `AppEvent::Outgoing` reply hasn't arrived yet. Tracked separately
    /// from [`streaming`] / [`pending_approval`] because there's a race
    /// window between dispatch and the first `StreamDelta`: without
    /// this counter, a second submission inside that window would be
    /// dispatched concurrently and the two responses would interleave.
    pub(crate) outstanding_responses: usize,
}

/// How long the "press Ctrl-D again to exit" prompt stays armed. Matches
/// the typical double-press window in shells with `ignoreeof`.
pub(crate) const CONFIRM_EXIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl AppState {
    pub(crate) fn new() -> Self {
        Self {
            mode: ViewMode::Chat,
            input: String::new(),
            cursor: 0,
            history: VecDeque::new(),
            history_cursor: None,
            streaming: None,
            streaming_committed_any: false,
            reasoning: None,
            reasoning_committed_any: false,
            pending_approval: None,
            commands: Vec::new(),
            completion_cursor: 0,
            confirm_exit_at: None,
            approval: None,
            outgoing_queue: VecDeque::new(),
            outstanding_responses: 0,
        }
    }

    /// True iff the chat loop is currently waiting on the agent to
    /// finish a response or for the user to resolve a pending approval.
    /// Submissions made while this is true should be queued rather than
    /// committed immediately, otherwise they'd interleave between the
    /// streaming preview and the eventual `aura> …` reply in scrollback.
    pub(crate) fn is_busy(&self) -> bool {
        self.streaming.is_some()
            || self.pending_approval.is_some()
            || self.outstanding_responses > 0
    }

    /// Bump the outstanding-response counter — call right before
    /// dispatching a real user message to the agent. The counter stays
    /// elevated through the inevitable streaming phase and is cleared
    /// by [`note_response_received`] when `AppEvent::Outgoing` lands.
    pub(crate) fn note_response_pending(&mut self) {
        self.outstanding_responses += 1;
    }

    /// Decrement the outstanding-response counter. Idempotent at zero
    /// so spurious `Outgoing` events (e.g. an agent restart sending an
    /// empty reply) don't underflow the counter.
    pub(crate) fn note_response_received(&mut self) {
        self.outstanding_responses = self.outstanding_responses.saturating_sub(1);
    }

    /// Park a user submission until the agent finishes streaming and
    /// any pending approval resolves. Cleared one-at-a-time by
    /// [`AppState::dequeue_submission`].
    pub(crate) fn queue_submission(&mut self, text: String) {
        self.outgoing_queue.push_back(text);
    }

    /// Pop the next parked submission, if any. Called by the run loop
    /// after each `AppEvent::Outgoing` so queued messages dispatch in
    /// chronological order.
    pub(crate) fn dequeue_submission(&mut self) -> Option<String> {
        self.outgoing_queue.pop_front()
    }

    pub(crate) fn with_approval(mut self, shared: ApprovalQueue) -> Self {
        self.approval = Some(shared);
        self
    }

    /// True iff an inline approval prompt is currently live in the viewport.
    pub(crate) fn approval_pending(&self) -> bool {
        self.pending_approval.is_some()
    }

    /// Install a fresh approval entry as the live prompt. Caller has
    /// already confirmed no entry is currently pending.
    pub(crate) fn set_pending_approval(&mut self, entry: ApprovalChatEntry) {
        self.pending_approval = Some(entry);
    }

    /// Move the selection cursor up on the live approval.
    pub(crate) fn approval_select_prev(&mut self) {
        if let Some(entry) = self.pending_approval.as_mut()
            && let ApprovalChatState::Pending { selected } = &mut entry.state
        {
            *selected = selected.saturating_sub(1);
        }
    }

    /// Move the selection cursor down on the live approval.
    pub(crate) fn approval_select_next(&mut self) {
        if let Some(entry) = self.pending_approval.as_mut()
            && let ApprovalChatState::Pending { selected } = &mut entry.state
        {
            *selected = (*selected + 1).min(2);
        }
    }

    /// Return the decision mapped to the currently highlighted option on
    /// the live approval. `None` when no approval is pending.
    pub(crate) fn active_approval_selected_decision(&self) -> Option<ApprovalDecision> {
        let entry = self.pending_approval.as_ref()?;
        let ApprovalChatState::Pending { selected } = entry.state else {
            return None;
        };
        Some(match selected {
            0 => ApprovalDecision::Approve,
            1 => ApprovalDecision::ApproveAlways,
            _ => ApprovalDecision::Deny,
        })
    }

    /// Resolve the live approval with the given decision. Returns the
    /// resolved entry (for the caller to commit as a summary line) plus
    /// the next queued approval (if any) to install as the new live
    /// prompt. Returns `None` if no approval was pending.
    pub(crate) fn resolve_active_approval(
        &mut self,
        decision: ApprovalDecision,
    ) -> Option<ResolvedApproval> {
        let mut entry = self.pending_approval.take()?;
        entry.state = ApprovalChatState::Resolved(decision);

        if let Some(queue) = self.approval.as_ref() {
            queue.resolve_head(decision);
            if let Some(req) = queue.peek_head() {
                self.pending_approval = Some(ApprovalChatEntry {
                    tool: req.tool,
                    accesses: req.accesses,
                    params_preview: req.params_preview,
                    state: ApprovalChatState::Pending { selected: 0 },
                });
            }
        }
        Some(ResolvedApproval { resolved: entry })
    }

    pub(crate) fn set_commands(&mut self, commands: Vec<SlashCommand>) {
        self.commands = commands;
    }

    /// Seed the input history from a previously-persisted snapshot. Entries
    /// are treated as chronological (oldest first); if the snapshot exceeds
    /// `HISTORY_CAP` the oldest entries are dropped.
    pub(crate) fn set_history(&mut self, entries: Vec<String>) {
        let skip = entries.len().saturating_sub(HISTORY_CAP);
        self.history = entries.into_iter().skip(skip).collect();
        self.history_cursor = None;
    }

    /// Return the filtered completion list for the current input. Only
    /// active when the buffer starts with `/` and the cursor sits on the
    /// command token (i.e. no whitespace between `/` and cursor). Returns
    /// an empty slice whenever completion should not be shown.
    pub(crate) fn completion_candidates(&self) -> Vec<&SlashCommand> {
        if !self.input.starts_with('/') {
            return Vec::new();
        }
        let prefix_end = self
            .input
            .find(char::is_whitespace)
            .unwrap_or(self.input.len());
        if self.cursor > prefix_end {
            return Vec::new();
        }
        let prefix = &self.input[..prefix_end];
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .collect()
    }

    pub(crate) fn completion_select_prev(&mut self) {
        let n = self.completion_candidates().len();
        if n == 0 {
            self.completion_cursor = 0;
            return;
        }
        self.completion_cursor = if self.completion_cursor == 0 {
            n - 1
        } else {
            self.completion_cursor - 1
        };
    }

    pub(crate) fn completion_select_next(&mut self) {
        let n = self.completion_candidates().len();
        if n == 0 {
            self.completion_cursor = 0;
            return;
        }
        self.completion_cursor = (self.completion_cursor + 1) % n;
    }

    /// Accept the highlighted completion candidate. Replaces the prefix
    /// up to the first whitespace with the candidate name + trailing
    /// space, so arguments can follow naturally.
    pub(crate) fn completion_accept(&mut self) -> bool {
        let candidates = self.completion_candidates();
        if candidates.is_empty() {
            return false;
        }
        let idx = self.completion_cursor.min(candidates.len() - 1);
        let name = candidates[idx].name.clone();
        let prefix_end = self
            .input
            .find(char::is_whitespace)
            .unwrap_or(self.input.len());
        let suffix = self.input[prefix_end..].to_string();
        self.input = format!("{name} {}", suffix.trim_start());
        self.cursor = name.len() + 1;
        self.completion_cursor = 0;
        true
    }

    /// Append a chunk of text to the currently streaming assistant response.
    /// Creates a fresh streaming buffer on the first chunk.
    pub(crate) fn append_stream_delta(&mut self, delta: &str) {
        self.streaming
            .get_or_insert_with(String::new)
            .push_str(delta);
    }

    /// Drain every newline-terminated line currently in the streaming
    /// buffer, leaving the trailing partial (if any) for the next
    /// delta. Used by the event loop to commit complete lines to
    /// scrollback the moment they're ready, instead of buffering the
    /// whole response in a viewport-side preview.
    pub(crate) fn drain_complete_stream_lines(&mut self) -> Vec<String> {
        let Some(buf) = self.streaming.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(idx) = buf.find('\n') {
            let mut line: String = buf.drain(..=idx).collect();
            // Strip the terminating `\n`; CRs from `\r\n` get stripped too.
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
            out.push(line);
        }
        out
    }

    /// Take whatever partial line remains in the streaming buffer,
    /// leaving `streaming = None`. Returns `None` if no partial is
    /// pending. Called when finalizing the response on `Outgoing`.
    pub(crate) fn take_stream_partial(&mut self) -> Option<String> {
        let partial = self.streaming.take()?;
        if partial.is_empty() {
            None
        } else {
            Some(partial)
        }
    }

    /// Clear the streaming buffer + reset the "first line committed"
    /// flag. Called after `Outgoing` finalises the response so the
    /// next stream starts with `aura> ` again. Also clears any leftover
    /// reasoning state (normally already flushed before finalize).
    pub(crate) fn clear_stream(&mut self) {
        self.streaming = None;
        self.streaming_committed_any = false;
        self.reasoning = None;
        self.reasoning_committed_any = false;
    }

    /// Append a chunk to the in-flight reasoning trace. Mirrors
    /// [`append_stream_delta`] for the dim reasoning buffer.
    pub(crate) fn append_reasoning_delta(&mut self, delta: &str) {
        self.reasoning
            .get_or_insert_with(String::new)
            .push_str(delta);
    }

    /// Drain every newline-terminated reasoning line, leaving the
    /// trailing partial for the next chunk. Mirrors
    /// [`drain_complete_stream_lines`].
    pub(crate) fn drain_complete_reasoning_lines(&mut self) -> Vec<String> {
        let Some(buf) = self.reasoning.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(idx) = buf.find('\n') {
            let mut line: String = buf.drain(..=idx).collect();
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
            out.push(line);
        }
        out
    }

    /// Take whatever partial reasoning line remains, leaving
    /// `reasoning = None`. Returns `None` when empty. Called to flush the
    /// reasoning run before a tool line / the answer / finalize.
    pub(crate) fn take_reasoning_partial(&mut self) -> Option<String> {
        let partial = self.reasoning.take()?;
        if partial.is_empty() {
            None
        } else {
            Some(partial)
        }
    }

    pub(crate) fn take_input(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_cursor = None;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            self.remember(trimmed.to_string());
            Some(trimmed.to_string())
        }
    }

    fn remember(&mut self, line: String) {
        if self.history.back().is_some_and(|last| last == &line) {
            return;
        }
        if self.history.len() >= HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(line);
    }

    pub(crate) fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = prev_char_boundary(&self.input, self.cursor);
        self.input.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub(crate) fn delete_char(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        let next = next_char_boundary(&self.input, self.cursor);
        self.input.drain(self.cursor..next);
    }

    pub(crate) fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = prev_char_boundary(&self.input, self.cursor);
    }

    pub(crate) fn move_right(&mut self) {
        if self.cursor >= self.input.len() {
            return;
        }
        self.cursor = next_char_boundary(&self.input, self.cursor);
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = self.input.len();
    }

    /// True when no `\n` precedes the cursor — i.e. the insertion point sits
    /// on the first logical line of the input buffer.
    pub(crate) fn cursor_at_first_line(&self) -> bool {
        !self.input[..self.cursor].contains('\n')
    }

    /// True when no `\n` follows the cursor — the cursor is on the last
    /// logical line of the buffer.
    pub(crate) fn cursor_at_last_line(&self) -> bool {
        !self.input[self.cursor..].contains('\n')
    }

    /// Move the cursor up one logical line, preserving byte-column where
    /// possible and clamping to the previous line's length. No-op when
    /// already on the first line.
    pub(crate) fn move_up_line(&mut self) {
        if self.cursor_at_first_line() {
            return;
        }
        let prefix = &self.input[..self.cursor];
        let cur_line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = self.cursor - cur_line_start;
        let prev_newline = cur_line_start - 1;
        let prev_line_start = self.input[..prev_newline]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let prev_line_len = prev_newline - prev_line_start;
        let target = prev_line_start + col.min(prev_line_len);
        self.cursor = clamp_to_boundary(&self.input, target);
    }

    /// Move the cursor down one logical line, preserving byte-column where
    /// possible and clamping to the next line's length. No-op when already
    /// on the last line.
    pub(crate) fn move_down_line(&mut self) {
        if self.cursor_at_last_line() {
            return;
        }
        let prefix = &self.input[..self.cursor];
        let cur_line_start = prefix.rfind('\n').map(|i| i + 1).unwrap_or(0);
        let col = self.cursor - cur_line_start;
        let next_newline = cur_line_start + self.input[cur_line_start..].find('\n').unwrap();
        let next_line_start = next_newline + 1;
        let next_line_end = self.input[next_line_start..]
            .find('\n')
            .map(|i| next_line_start + i)
            .unwrap_or(self.input.len());
        let next_line_len = next_line_end - next_line_start;
        let target = next_line_start + col.min(next_line_len);
        self.cursor = clamp_to_boundary(&self.input, target);
    }

    pub(crate) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_cursor.is_none() && !self.input.is_empty() {
            return;
        }
        let new_cursor = match self.history_cursor {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => self.history.len() - 1,
        };
        self.history_cursor = Some(new_cursor);
        self.input = self.history[new_cursor].clone();
        self.cursor = self.input.len();
    }

    pub(crate) fn history_next(&mut self) {
        let Some(i) = self.history_cursor else { return };
        if i + 1 >= self.history.len() {
            self.history_cursor = None;
            self.input.clear();
            self.cursor = 0;
        } else {
            self.history_cursor = Some(i + 1);
            self.input = self.history[i + 1].clone();
            self.cursor = self.input.len();
        }
    }

    pub(crate) fn enter_dashboard(&mut self, kind: ViewKind, snapshot: DashboardSnapshot) {
        let mut table_state = TableState::default();
        if !snapshot.rows.is_empty() {
            table_state.select(Some(0));
        }
        self.mode = ViewMode::Dashboard {
            kind,
            snapshot,
            table_state,
        };
    }

    pub(crate) fn refresh_dashboard(&mut self, new_snapshot: DashboardSnapshot) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
        {
            if new_snapshot.rows.is_empty() {
                table_state.select(None);
            } else if let Some(sel) = table_state.selected()
                && sel >= new_snapshot.rows.len()
            {
                table_state.select(Some(new_snapshot.rows.len() - 1));
            }
            *snapshot = new_snapshot;
        }
    }

    pub(crate) fn exit_dashboard(&mut self) {
        self.mode = ViewMode::Chat;
    }

    pub(crate) fn dashboard_kind(&self) -> Option<ViewKind> {
        match &self.mode {
            ViewMode::Dashboard { kind, .. } => Some(*kind),
            ViewMode::Chat => None,
        }
    }

    pub(crate) fn dashboard_select_prev(&mut self) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
            && !snapshot.rows.is_empty()
        {
            let current = table_state.selected().unwrap_or(0);
            let next = current.saturating_sub(1);
            table_state.select(Some(next));
        }
    }

    pub(crate) fn dashboard_select_next(&mut self) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
            && !snapshot.rows.is_empty()
        {
            let last = snapshot.rows.len() - 1;
            let current = table_state.selected().unwrap_or(0);
            let next = (current + 1).min(last);
            table_state.select(Some(next));
        }
    }

    pub(crate) fn dashboard_page(&mut self, delta: isize) {
        if let ViewMode::Dashboard {
            snapshot,
            table_state,
            ..
        } = &mut self.mode
            && !snapshot.rows.is_empty()
        {
            let last = snapshot.rows.len() as isize - 1;
            let current = table_state.selected().unwrap_or(0) as isize;
            let next = (current + delta).clamp(0, last) as usize;
            table_state.select(Some(next));
        }
    }
}

/// Outcome of resolving the live approval prompt. The resolved entry is
/// returned for the caller to commit to scrollback as a one-line summary.
/// Any next-queued entry is promoted to the new live prompt internally
/// (see [`AppState::resolve_active_approval`]) so the caller doesn't need
/// to touch it — a redraw will surface it.
pub(crate) struct ResolvedApproval {
    pub resolved: ApprovalChatEntry,
}

/// Round `idx` down to the nearest UTF-8 char boundary.
fn clamp_to_boundary(s: &str, mut idx: usize) -> usize {
    if idx > s.len() {
        idx = s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_round_trip_ascii() {
        let mut app = AppState::new();
        for c in "hello".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "hello");
        app.backspace();
        app.backspace();
        assert_eq!(app.input, "hel");
    }

    #[test]
    fn multiline_cursor_predicates_track_newlines() {
        let mut app = AppState::new();
        app.input = "a\nbc".to_string();
        app.cursor = 0;
        assert!(app.cursor_at_first_line());
        assert!(!app.cursor_at_last_line());
        app.cursor = 2;
        assert!(!app.cursor_at_first_line());
        assert!(app.cursor_at_last_line());
    }

    #[test]
    fn move_up_and_down_line_clamp_to_line_length() {
        let mut app = AppState::new();
        app.input = "abcd\nef\nghij".to_string();
        app.cursor = 12;
        app.move_up_line();
        assert_eq!(app.cursor, 7);
        app.move_up_line();
        assert_eq!(app.cursor, 2);
        app.move_down_line();
        assert_eq!(app.cursor, 7);
    }

    #[test]
    fn move_up_line_is_noop_on_first_line() {
        let mut app = AppState::new();
        app.input = "only line".to_string();
        app.cursor = 3;
        app.move_up_line();
        assert_eq!(app.cursor, 3);
    }

    #[test]
    fn insert_and_backspace_multibyte() {
        let mut app = AppState::new();
        app.insert_char('中');
        app.insert_char('文');
        assert_eq!(app.input, "中文");
        app.backspace();
        assert_eq!(app.input, "中");
    }

    #[test]
    fn take_input_trims_and_remembers() {
        let mut app = AppState::new();
        for c in "  hi  ".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.take_input().as_deref(), Some("hi"));
        assert_eq!(app.history.back().map(String::as_str), Some("hi"));
        assert_eq!(app.input, "");
    }

    #[test]
    fn take_input_rejects_empty() {
        let mut app = AppState::new();
        assert!(app.take_input().is_none());
    }

    #[test]
    fn take_input_dedupes_consecutive_history() {
        let mut app = AppState::new();
        for c in "foo".chars() {
            app.insert_char(c);
        }
        app.take_input();
        for c in "foo".chars() {
            app.insert_char(c);
        }
        app.take_input();
        assert_eq!(app.history.len(), 1);
    }

    #[test]
    fn history_prev_walks_back_then_clamps() {
        let mut app = AppState::new();
        for cmd in ["a", "b", "c"] {
            app.input = cmd.to_string();
            app.cursor = app.input.len();
            app.take_input();
        }
        app.history_prev();
        assert_eq!(app.input, "c");
        app.history_prev();
        assert_eq!(app.input, "b");
        app.history_prev();
        app.history_prev();
        assert_eq!(app.input, "a");
    }

    #[test]
    fn history_prev_is_inert_with_nonempty_draft() {
        let mut app = AppState::new();
        app.input = "past".to_string();
        app.cursor = app.input.len();
        app.take_input();
        app.input = "drafting".to_string();
        app.cursor = app.input.len();
        app.history_prev();
        assert_eq!(app.input, "drafting");
        assert!(app.history_cursor.is_none());
    }

    #[test]
    fn history_prev_keeps_walking_once_in_history_mode() {
        let mut app = AppState::new();
        for cmd in ["a", "b"] {
            app.input = cmd.to_string();
            app.cursor = app.input.len();
            app.take_input();
        }
        app.history_prev();
        assert_eq!(app.input, "b");
        app.history_prev();
        assert_eq!(app.input, "a");
    }

    #[test]
    fn history_next_returns_to_empty_buffer() {
        let mut app = AppState::new();
        app.input = "only".to_string();
        app.cursor = app.input.len();
        app.take_input();
        app.history_prev();
        assert_eq!(app.input, "only");
        app.history_next();
        assert_eq!(app.input, "");
        assert!(app.history_cursor.is_none());
    }

    #[test]
    fn enter_and_exit_dashboard() {
        let mut app = AppState::new();
        let snap = DashboardSnapshot {
            title: "Skills".into(),
            columns: vec!["name".into()],
            rows: vec![vec!["calc".into()]],
            footer: None,
        };
        app.enter_dashboard(ViewKind::Skills, snap);
        assert_eq!(app.dashboard_kind(), Some(ViewKind::Skills));
        app.exit_dashboard();
        assert_eq!(app.dashboard_kind(), None);
    }
}
