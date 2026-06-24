//! TUI application state: live viewport content, input buffer, input history,
//! view mode.
//!
//! State mutation is synchronous and side-effect-free. With the inline-viewport
//! rendering model, the live chat history lives in the terminal's own
//! scrollback buffer, written via [`ratatui::Terminal::insert_before`]. This
//! module tracks what's currently *live* in the viewport (the input draft, the
//! streaming-response preview, the single pending approval) plus a bounded
//! [`TranscriptBlock`] log of what was committed, kept solely so the on-screen
//! conversation can be re-rendered after a resize refresh wipes the terminal's
//! scrollback — see [`AppState::record_block`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use baybo_channels::{DashboardSnapshot, SlashCommand, ViewKind};
use baybo_model::SessionId;
use ratatui::text::Line;
use ratatui::widgets::TableState;

use baybo_tools::{ApprovalDecision, ApprovalQueue, ResourceAccess};

const HISTORY_CAP: usize = 500;

/// An approval request rendered inline in the viewport (when pending) and
/// committed as a one-line summary to scrollback (once resolved).
#[derive(Debug, Clone)]
pub(crate) struct ApprovalChatEntry {
    pub call_id: String,
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

/// A tool call in flight — started but not yet completed. Lives in
/// [`AppState::running_tools`], which the working zone renders as live
/// `● tool(label)` rows. The `● tool` line is **not** committed to scrollback
/// at start: with append-only native scrollback that would detach a concurrent
/// sibling's result from its tool. Instead the whole block — this line, any
/// `approval_lines` resolved mid-call, then the `⎿` result — commits as a unit
/// when the matching `ToolCompleted` arrives, so the result sits under its tool.
#[derive(Debug, Clone)]
pub(crate) struct RunningTool {
    pub call_id: String,
    pub tool: String,
    pub label: Option<String>,
    /// Resolved-approval summary line(s) for this call, buffered so they land
    /// between the tool's `●` and `⎿` when the block commits on completion.
    pub approval_lines: Vec<Line<'static>>,
}

/// Kind of the last block committed to scrollback. Drives single-blank
/// separation: consecutive `Answer` (or `Tool`) lines stack tight as one
/// block, but a change of kind (or any `Other` block) gets one leading
/// separator blank. There are no trailing blanks — the always-reserved
/// working row separates the last block from the input box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Answer,
    Tool,
    Other,
}

/// One committed scrollback block, retained (bounded) so the on-screen
/// conversation can be re-rendered after a resize refresh wipes the terminal's
/// scrollback. The inline-viewport model keeps no other transcript.
///
/// Width-dependent blocks store their *source* so replay re-renders them at the
/// new width; the rest store their already-rendered lines, which re-wrap
/// cleanly at any width. The banner isn't stored — replay reprints it from the
/// session id.
#[derive(Clone)]
pub(crate) enum TranscriptBlock {
    /// User message / mid-turn steer. Stored as text because its full-width
    /// highlighted bar is width-dependent (rebuilt by `render_user_lines`).
    User(String),
    /// Assistant answer body (rendered lines; re-wraps on replay).
    Answer(Vec<Line<'static>>),
    /// Tool block — `● tool` + `⎿ summary`, or a resolved-approval summary.
    Tool(Vec<Line<'static>>),
    /// Standalone block: status / log / system note.
    Other(Vec<Line<'static>>),
    /// `cooked for …` footer (re-rendered from the elapsed duration).
    Cooked(Duration),
}

/// Cap on retained transcript lines. The replay log is bounded so a long-lived
/// session doesn't grow it without limit and a resize doesn't re-dump an
/// unbounded history into scrollback; the oldest blocks are evicted past this.
const TRANSCRIPT_MAX_LINES: usize = 4000;

/// Rows a transcript block occupies, for the [`TRANSCRIPT_MAX_LINES`] cap.
/// `User` counts its source newlines (the bar is one row per text line);
/// `Cooked` is a single footer row.
fn block_line_count(block: &TranscriptBlock) -> usize {
    match block {
        TranscriptBlock::User(text) => text.lines().count().max(1),
        TranscriptBlock::Answer(lines)
        | TranscriptBlock::Tool(lines)
        | TranscriptBlock::Other(lines) => lines.len(),
        TranscriptBlock::Cooked(_) => 1,
    }
}

pub(crate) struct AppState {
    pub(crate) mode: ViewMode,
    /// Session id shown in the banner. Tracked here (set at startup, updated
    /// on `/new`) so the resize-refresh path can reprint the banner without a
    /// reference to the transport.
    pub(crate) session_id: SessionId,
    /// Current active LLM shown below the input box. `None` hides the footer.
    pub(crate) model_label: Option<String>,
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
    /// committed line uses `baybo> ` (bold green), subsequent lines use
    /// the `      ` (six-space) continuation indent so the conversation
    /// reads as one coherent block.
    pub(crate) streaming_committed_any: bool,
    /// When the current turn became active. Drives the animated
    /// "working" indicator and its elapsed-seconds readout in the live
    /// region; `None` whenever no turn we initiated is in flight. Set by
    /// [`note_response_pending`], cleared by [`note_response_received`]
    /// when the last outstanding response lands — or by
    /// [`reset_working`] when `/stop` cancels the turn (which sends no
    /// final response to clear it).
    pub(crate) working_since: Option<Instant>,
    /// Tools that have started but not yet completed. The start line commits
    /// to scrollback immediately; the matching `ToolCompleted` (by `call_id`)
    /// removes the entry before committing the result summary. Cleared at turn
    /// end / on `/stop` via [`clear_stream`].
    pub(crate) running_tools: Vec<RunningTool>,
    /// Animation counter advanced once per working-indicator tick; the dot's
    /// pulse colour is `frame % WORKING_PULSE.len()`. A redraw-driven colour
    /// cycle (not terminal blink, which many terminals don't render).
    pub(crate) spinner_tick: usize,
    /// Kind of the last block committed to scrollback (`None` at start / after
    /// a clear). Drives the leading-separator spacing — see [`BlockKind`].
    pub(crate) last_block: Option<BlockKind>,
    /// Whether the last row committed to scrollback is blank. Lets the
    /// commit helpers emit *exactly one* separator blank between blocks —
    /// never doubling up, never missing one.
    pub(crate) last_row_blank: bool,
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
    /// Plain (non-slash) messages submitted while a turn is in flight.
    /// Unlike [`outgoing_queue`], these are dispatched to the agent
    /// **immediately** (mid-turn steering — the agent folds them in once
    /// its current tool batch finishes) and only their *display* is
    /// deferred: each shows as a `↳` line under the working indicator
    /// until the running tool batch fully drains, then commits to
    /// scrollback as a `you>` line below that batch's results — matching
    /// the order the agent actually saw them. A turn that ends with steers
    /// still pending commits them at `Outgoing` (they fall to the next
    /// turn server-side, so the order still reads correctly).
    pub(crate) pending_interjections: VecDeque<String>,
    /// Slash commands submitted while a turn is in flight. Held back —
    /// not dispatched — and drained one-at-a-time on `AppEvent::Outgoing`
    /// (see the run loop), so a `/skills` or unknown-skill slash runs
    /// *after* the current turn rather than spawning a concurrent turn
    /// that would interleave in scrollback. Plain messages no longer use
    /// this path; they steer via [`pending_interjections`].
    pub(crate) outgoing_queue: VecDeque<String>,
    /// User messages that have been dispatched to the agent but whose
    /// `AppEvent::Outgoing` reply hasn't arrived yet. Tracked separately
    /// from [`streaming`] / [`pending_approval`] because there's a race
    /// window between dispatch and the first `StreamDelta`: without
    /// this counter, a second submission inside that window would be
    /// dispatched concurrently and the two responses would interleave.
    pub(crate) outstanding_responses: usize,
    /// Bounded log of committed scrollback blocks, in commit order, replayed
    /// to re-render the conversation after a resize refresh wipes the
    /// terminal's scrollback. Rebuilt as a side effect of replay; cleared on
    /// `/clear` / `/new` / Ctrl-L. See [`TranscriptBlock`].
    transcript: VecDeque<TranscriptBlock>,
    /// Running line total of [`transcript`], maintained so eviction past
    /// [`TRANSCRIPT_MAX_LINES`] doesn't have to re-sum the whole deque.
    transcript_lines: usize,
}

/// How long the "press Ctrl-D again to exit" prompt stays armed. Matches
/// the typical double-press window in shells with `ignoreeof`.
pub(crate) const CONFIRM_EXIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

impl AppState {
    pub(crate) fn new() -> Self {
        Self {
            mode: ViewMode::Chat,
            session_id: SessionId::from(""),
            model_label: None,
            input: String::new(),
            cursor: 0,
            history: VecDeque::new(),
            history_cursor: None,
            streaming: None,
            streaming_committed_any: false,
            working_since: None,
            running_tools: Vec::new(),
            spinner_tick: 0,
            last_block: None,
            last_row_blank: true,
            pending_approval: None,
            commands: Vec::new(),
            completion_cursor: 0,
            confirm_exit_at: None,
            approval: None,
            pending_interjections: VecDeque::new(),
            outgoing_queue: VecDeque::new(),
            outstanding_responses: 0,
            transcript: VecDeque::new(),
            transcript_lines: 0,
        }
    }

    /// True iff the chat loop is currently waiting on the agent to
    /// finish a response or for the user to resolve a pending approval.
    /// Submissions made while this is true should be queued rather than
    /// committed immediately, otherwise they'd interleave between the
    /// streaming preview and the eventual `baybo> …` reply in scrollback.
    pub(crate) fn is_busy(&self) -> bool {
        self.streaming.is_some()
            || self.pending_approval.is_some()
            || self.outstanding_responses > 0
    }

    /// Bump the outstanding-response counter — call right before
    /// dispatching a turn-initiating user message to the agent. The
    /// counter stays elevated through the inevitable streaming phase and
    /// is cleared by [`note_response_received`] when `AppEvent::Outgoing`
    /// lands. Also arms the "working" clock on the leading edge (0 → 1)
    /// so the animated indicator and its elapsed readout start ticking.
    pub(crate) fn note_response_pending(&mut self) {
        if self.outstanding_responses == 0 {
            self.working_since = Some(Instant::now());
        }
        self.outstanding_responses += 1;
    }

    /// Decrement the outstanding-response counter. Idempotent at zero
    /// so spurious `Outgoing` events (e.g. an agent restart sending an
    /// empty reply) don't underflow the counter. Disarms the "working"
    /// clock once the last response lands.
    pub(crate) fn note_response_received(&mut self) {
        self.outstanding_responses = self.outstanding_responses.saturating_sub(1);
        if self.outstanding_responses == 0 {
            self.working_since = None;
        }
    }

    /// Force the live "working" state back to idle. Used by `/stop`,
    /// which cancels the in-flight turn server-side and so never delivers
    /// the `Outgoing` that would otherwise clear the indicator.
    pub(crate) fn reset_working(&mut self) {
        self.outstanding_responses = 0;
        self.working_since = None;
        self.clear_stream();
    }

    /// Whether the animated working indicator should render right now: a
    /// turn we initiated is in flight and no approval prompt is stealing
    /// the live region (during an approval the agent is blocked on the
    /// user, not working).
    pub(crate) fn show_working(&self) -> bool {
        self.working_since.is_some() && self.pending_approval.is_none()
    }

    /// Seconds elapsed since the working clock armed, for the indicator's
    /// `(Ns · …)` readout. Zero when idle.
    pub(crate) fn working_elapsed_secs(&self) -> u64 {
        self.working_since
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// Record a tool as started (in flight) until [`take_running_tool`]
    /// removes it on completion. The working zone renders it as a live
    /// `● tool(label)` row meanwhile; its scrollback block is deferred to
    /// completion. See [`RunningTool`].
    pub(crate) fn push_running_tool(
        &mut self,
        call_id: String,
        tool: String,
        label: Option<String>,
    ) {
        self.running_tools.push(RunningTool {
            call_id,
            tool,
            label,
            approval_lines: Vec::new(),
        });
    }

    /// Remove the in-flight tool matching `call_id` (its completion arrived).
    pub(crate) fn take_running_tool(&mut self, call_id: &str) -> Option<RunningTool> {
        self.running_tools
            .iter()
            .position(|t| t.call_id == call_id)
            .map(|i| self.running_tools.remove(i))
    }

    /// The in-flight tools, oldest first — rendered as live `● tool` rows.
    pub(crate) fn running_tools(&self) -> &[RunningTool] {
        &self.running_tools
    }

    /// Buffer a resolved-approval summary onto its in-flight tool (matched by
    /// `call_id`) so it commits between that tool's `●` and `⎿`. Returns the
    /// lines back as `Err` when no such tool is tracked, so the caller can
    /// commit them standalone instead.
    pub(crate) fn buffer_approval_line(
        &mut self,
        call_id: &str,
        lines: Vec<Line<'static>>,
    ) -> Result<(), Vec<Line<'static>>> {
        match self.running_tools.iter_mut().find(|t| t.call_id == call_id) {
            Some(rt) => {
                rt.approval_lines.extend(lines);
                Ok(())
            }
            None => Err(lines),
        }
    }

    /// Advance the working-indicator pulse one frame.
    pub(crate) fn tick_spinner(&mut self) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
    }

    /// Arm the working clock if it isn't already. Covers turn activity that
    /// arrives without a local turn-initiating dispatch — a steer that fell
    /// through to its own turn, or any externally-driven streaming turn on
    /// this session — so the indicator tracks every streaming turn, not
    /// just the ones this client started.
    pub(crate) fn ensure_working_clock(&mut self) {
        if self.working_since.is_none() {
            self.working_since = Some(Instant::now());
        }
    }

    /// Park a plain message as a mid-turn steer: it has already been
    /// dispatched to the agent, and this only tracks the line shown under
    /// the working indicator until the running tool batch drains and
    /// commits it to scrollback. See [`pending_interjections`].
    pub(crate) fn queue_interjection(&mut self, text: String) {
        self.pending_interjections.push_back(text);
    }

    /// Drain every pending steer line, in submission order. Called when a
    /// tool batch fully drains (commit them below its results) or when the
    /// turn finalises (fallback). Empties the queue.
    pub(crate) fn take_pending_interjections(&mut self) -> Vec<String> {
        self.pending_interjections.drain(..).collect()
    }

    /// Lines to render under the working indicator, in submission order:
    /// pending steers first (dispatched, awaiting a tool-batch boundary),
    /// then deferred slash commands (awaiting turn end). Both read as "queued".
    pub(crate) fn queued_display_lines(&self) -> impl Iterator<Item = &str> {
        self.pending_interjections
            .iter()
            .chain(self.outgoing_queue.iter())
            .map(String::as_str)
    }

    /// Park a slash command until the current turn ends. Cleared
    /// one-at-a-time by [`AppState::dequeue_submission`].
    pub(crate) fn queue_submission(&mut self, text: String) {
        self.outgoing_queue.push_back(text);
    }

    /// Pop the next parked slash command, if any. Called by the run loop
    /// after each `AppEvent::Outgoing` so deferred slashes run in
    /// chronological order.
    pub(crate) fn dequeue_submission(&mut self) -> Option<String> {
        self.outgoing_queue.pop_front()
    }

    /// Discard every deferred slash command without running it. Called by
    /// `/stop`: the interrupt returns the session to idle, and a slash parked
    /// for "after this turn" has no turn to follow (the cancelled turn sends
    /// no `Outgoing` to drain it), so it would otherwise linger as a `↳` line
    /// and later fire after an unrelated turn.
    pub(crate) fn clear_deferred_submissions(&mut self) {
        self.outgoing_queue.clear();
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
                    call_id: req.call_id,
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

    pub(crate) fn set_model_label(&mut self, label: Option<String>) {
        self.model_label = label
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
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

    /// Clear the streaming buffer + reset the "first line committed" flag,
    /// any open tool-run bracket, and any in-flight tool entries. Called
    /// after `Outgoing` finalises the response (and by [`reset_working`] on
    /// `/stop`) so the next answer starts clean.
    pub(crate) fn clear_stream(&mut self) {
        self.streaming = None;
        self.streaming_committed_any = false;
        self.running_tools.clear();
    }

    /// Append a committed block to the resize-replay log, evicting the oldest
    /// blocks once the retained line count exceeds [`TRANSCRIPT_MAX_LINES`]
    /// (the most recent block is always kept). Called by the scrollback-commit
    /// helpers right after they insert.
    pub(crate) fn record_block(&mut self, block: TranscriptBlock) {
        self.transcript_lines += block_line_count(&block);
        self.transcript.push_back(block);
        while self.transcript_lines > TRANSCRIPT_MAX_LINES && self.transcript.len() > 1 {
            if let Some(evicted) = self.transcript.pop_front() {
                self.transcript_lines = self
                    .transcript_lines
                    .saturating_sub(block_line_count(&evicted));
            }
        }
    }

    /// Drop the replay log. Called on `/clear` / `/new` / Ctrl-L, where the
    /// on-screen conversation is intentionally reset — a later resize must not
    /// resurrect the cleared history.
    pub(crate) fn clear_transcript(&mut self) {
        self.transcript.clear();
        self.transcript_lines = 0;
    }

    /// Take the replay log out for a resize refresh, leaving it empty. Replay
    /// re-commits each block through the same helpers, which re-record it — so
    /// the log rebuilds itself to the same content as a side effect.
    pub(crate) fn take_transcript(&mut self) -> VecDeque<TranscriptBlock> {
        self.transcript_lines = 0;
        std::mem::take(&mut self.transcript)
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

    #[test]
    fn working_clock_arms_on_first_pending_and_clears_at_zero() {
        let mut app = AppState::new();
        assert!(!app.show_working());
        app.note_response_pending();
        assert!(app.show_working());
        app.note_response_pending();
        app.note_response_received();
        assert!(app.show_working(), "still one outstanding response");
        app.note_response_received();
        assert!(!app.show_working(), "cleared once the last response lands");
    }

    #[test]
    fn approval_hides_working_indicator() {
        let mut app = AppState::new();
        app.note_response_pending();
        assert!(app.show_working());
        app.set_pending_approval(ApprovalChatEntry {
            call_id: "call-1".into(),
            tool: "Bash".into(),
            accesses: Vec::new(),
            params_preview: String::new(),
            state: ApprovalChatState::Pending { selected: 0 },
        });
        assert!(
            !app.show_working(),
            "an approval prompt owns the live region instead"
        );
    }

    #[test]
    fn ensure_working_clock_is_idempotent() {
        let mut app = AppState::new();
        app.ensure_working_clock();
        let armed = app.working_since;
        assert!(armed.is_some());
        app.ensure_working_clock();
        assert_eq!(app.working_since, armed, "second call must not re-arm");
    }

    #[test]
    fn steers_and_deferred_slashes_share_the_queued_display() {
        let mut app = AppState::new();
        app.queue_interjection("steer one".into());
        app.queue_submission("/skills".into());
        let shown: Vec<&str> = app.queued_display_lines().collect();
        assert_eq!(shown, vec!["steer one", "/skills"]);

        // A tool boundary drains only the steers; the deferred slash stays.
        assert_eq!(app.take_pending_interjections(), vec!["steer one"]);
        let shown: Vec<&str> = app.queued_display_lines().collect();
        assert_eq!(shown, vec!["/skills"]);
    }

    #[test]
    fn clear_deferred_submissions_drops_parked_slashes_but_keeps_steers() {
        let mut app = AppState::new();
        app.queue_submission("/skills".into());
        app.queue_submission("/clear".into());
        app.queue_interjection("steer".into());

        // `/stop` discards parked slashes (no turn left to drain them)…
        app.clear_deferred_submissions();
        assert!(
            app.dequeue_submission().is_none(),
            "deferred slashes are dropped, not just hidden"
        );
        // …but a steer is server-side state the interrupt commits itself, so
        // clearing the slash queue must leave the interjection queue alone.
        let shown: Vec<&str> = app.queued_display_lines().collect();
        assert_eq!(shown, vec!["steer"]);
    }

    #[test]
    fn reset_working_returns_to_idle_without_dropping_steers() {
        let mut app = AppState::new();
        app.note_response_pending();
        app.append_stream_delta("half a sentence");
        app.queue_interjection("steer".into());
        app.reset_working();
        assert!(!app.show_working());
        assert!(!app.is_busy());
        assert!(app.streaming.is_none());
        // The caller commits pending steers itself before resetting, so
        // reset must not silently discard them.
        assert_eq!(app.pending_interjections.len(), 1);
    }

    #[test]
    fn push_and_take_running_tool_round_trip() {
        let mut app = AppState::new();
        app.push_running_tool("c1".into(), "Bash".into(), Some("ls".into()));
        app.push_running_tool("c2".into(), "Read".into(), None);
        assert_eq!(app.running_tools().len(), 2);

        let taken = app.take_running_tool("c1").expect("c1 was in flight");
        assert_eq!(taken.tool, "Bash");
        assert_eq!(taken.label.as_deref(), Some("ls"));
        assert_eq!(app.running_tools().len(), 1, "only c1 removed");
        assert!(app.take_running_tool("missing").is_none());
    }

    #[test]
    fn buffer_approval_line_attaches_to_its_tool_else_hands_lines_back() {
        let mut app = AppState::new();
        app.push_running_tool("c1".into(), "Bash".into(), None);

        assert!(
            app.buffer_approval_line("c1", vec![Line::from("● approved: Bash")])
                .is_ok()
        );
        assert_eq!(
            app.take_running_tool("c1").unwrap().approval_lines.len(),
            1,
            "the decision rides the tool's deferred block"
        );

        // No matching in-flight tool: the lines come back so the caller can
        // commit them standalone rather than lose the decision.
        let back = app.buffer_approval_line("gone", vec![Line::from("x")]);
        assert!(back.is_err());
    }

    #[test]
    fn reset_working_clears_in_flight_tools() {
        let mut app = AppState::new();
        app.note_response_pending();
        app.push_running_tool("c1".into(), "Bash".into(), None);
        app.reset_working();
        assert!(
            app.running_tools().is_empty(),
            "/stop drops the live tool list (no completion will arrive)"
        );
    }

    #[test]
    fn transcript_evicts_oldest_past_cap_but_keeps_newest() {
        let mut app = AppState::new();
        // A lone oversized block is retained — the newest block is never
        // evicted, however large.
        app.record_block(TranscriptBlock::Answer(vec![
            Line::from("x");
            TRANSCRIPT_MAX_LINES + 50
        ]));
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.transcript_lines, TRANSCRIPT_MAX_LINES + 50);

        // A newer block pushes the total over the cap, evicting the old one.
        app.record_block(TranscriptBlock::User("hi".into()));
        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(
            app.transcript.front(),
            Some(TranscriptBlock::User(_))
        ));
        assert_eq!(app.transcript_lines, 1);
    }

    #[test]
    fn transcript_clear_and_take_empty_the_log() {
        let mut app = AppState::new();
        app.record_block(TranscriptBlock::User("a".into()));
        app.record_block(TranscriptBlock::Tool(vec![Line::from("b")]));
        assert_eq!(app.transcript.len(), 2);

        let taken = app.take_transcript();
        assert_eq!(taken.len(), 2, "take returns the blocks for replay");
        assert!(app.transcript.is_empty(), "take leaves the log empty");
        assert_eq!(app.transcript_lines, 0);

        app.record_block(TranscriptBlock::Other(vec![Line::from("c")]));
        app.clear_transcript();
        assert!(app.transcript.is_empty());
        assert_eq!(app.transcript_lines, 0);
    }
}
