use std::sync::Arc;

use baybo_channels::{AgentEvent, AgentOutput, OutgoingMessage, StatusPhase, ToolStatus};
use baybo_context::ContextManager;
use baybo_llm::{
    Attribution, BillableLlm, BoundBilledLlm, ChatRequest, LlmResponse, StreamEvent, TokenUsage,
    ToolDefinitionForLlm,
};
use baybo_memory::{Memory, MemoryContext, MemoryScope};
use baybo_model::{
    ChatMessage, ContentBlock, LlmEntryName, MessageSource, ThinkingContent, TurnId,
};
use baybo_turn::{TurnInput, TurnLifecycle, TurnOutput};
use futures::StreamExt;
use tokio::sync::mpsc;

use baybo_model::{ControlEventKind, LineageKind, Session, TriggerSource};
use baybo_tools::{ApprovalDecision, ReadTracker, ToolConcurrency, ToolOutput, ToolRegistry};
use baybo_trace::{
    CompressionTrigger, LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanRecorder, StepHandle,
    StepKind,
};
use tracing::{debug, info, warn};

use crate::runtime::compression::CompressionRunner;
use crate::runtime::error_recovery::ErrorHandler;
use crate::runtime::progress_observer::{
    ObserverState, ProgressObserverRunner, build_observer_prompt, channel_wants_progress,
    should_fire_observer,
};
use crate::runtime::scope::TurnSpec;

use crate::runtime::tool_executor::{ExecutedTool, ToolExecutor};
use crate::security::SecurityGateway;
use tokio_util::sync::CancellationToken;

/// The maximum amount of text we'll hold in the streaming buffer waiting
/// for a placeholder to complete. If a chunk ends with an open `[{` but no
/// closing `}]` arrives within this many bytes, we flush anyway — no real
/// placeholder is this long, so holding further would be a DoS vector.
const STREAM_BUFFER_HIGH_WATER: usize = 128;

/// Max characters in a `ToolCompleted` progress summary before it is
/// truncated with an ellipsis. Presentation-only — the full result still
/// reaches the LLM (capped separately) and the trace.
const TOOL_SUMMARY_MAX: usize = 80;

/// Cap on `MultiModalText::llm_images` forwarded to the model per
/// iteration. Each image bills real provider tokens, and a page-each
/// screenshot loop would otherwise stack unbounded vision input; newest
/// win (FIFO eviction) because earlier screenshots are typically stale —
/// the page state has moved on.
const MAX_LLM_IMAGES_PER_ITERATION: usize = 8;

/// Upper bound on how many [`ToolConcurrency::Concurrent`] tool calls
/// run at once within a single LLM response. A
/// [`ToolConcurrency::Exclusive`] call (any tool that mutates state)
/// acquires *all* of these permits, so it runs alone — it waits for
/// in-flight pool calls to drain and blocks any other pool call until it
/// returns. A [`ToolConcurrency::Independent`] call (`spawn_subagent`)
/// acquires no permit and self-bounds out-of-band, so the pool never
/// throttles subagent fan-out. Like the per-tool timeout ceiling, the
/// cap lives in code rather than `baybo.json`.
const MAX_CONCURRENT_TOOL_CALLS: usize = 10;

/// Trim and length-cap a single-line summary string. Char-based so a
/// multibyte boundary is never split.
fn truncate_summary(s: &str) -> String {
    let s = s.trim();
    let mut out: String = s.chars().take(TOOL_SUMMARY_MAX).collect();
    if s.chars().count() > TOOL_SUMMARY_MAX {
        out.push('…');
    }
    out
}

/// Render a tool's textual output as a short progress caption: the
/// trimmed single line when it is one short line, else a line count.
/// Never returns raw multi-line content — the caller still sanitizes it
/// for leaks before it leaves the agent.
fn summarize_text(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return "no output".to_string();
    }
    let lines = trimmed.lines().count();
    if lines <= 1 {
        truncate_summary(trimmed)
    } else {
        format!("{lines} lines")
    }
}

/// Content identity of a media block. Keys on the blob's digest, never on the
/// `blob_id` — every `put` mints a fresh read token, so the same bytes staged
/// twice carry two different ids.
fn media_digest(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Image { blob, .. }
        | ContentBlock::Audio { blob, .. }
        | ContentBlock::File { blob, .. } => blob.content_digest(),
        _ => None,
    }
}

/// Fold a tool's attachments into the turn's accumulator, skipping content the
/// turn already staged. `AttachFile` called twice on one path stages the same
/// bytes twice; the reply must show the user that file once. A block with no
/// digest (a malformed id, or a non-media block a tool wrongly passed) has no
/// identity to compare, so it rides through untouched.
fn extend_unique_attachments(acc: &mut Vec<ContentBlock>, incoming: &[ContentBlock]) {
    for block in incoming {
        if let Some(digest) = media_digest(block)
            && acc
                .iter()
                .filter_map(media_digest)
                .any(|seen| seen == digest)
        {
            continue;
        }
        acc.push(block.clone());
    }
}

/// Append LLM-visible images while keeping the accumulator at or under
/// [`MAX_LLM_IMAGES_PER_ITERATION`] via FIFO eviction — newest win, since
/// an earlier screenshot is stale once the page state has moved on.
fn push_bounded_images<I: IntoIterator<Item = ContentBlock>>(
    dst: &mut Vec<ContentBlock>,
    items: I,
) {
    for item in items {
        if dst.len() >= MAX_LLM_IMAGES_PER_ITERATION {
            dst.remove(0);
        }
        dst.push(item);
    }
}

/// Name what the user is about to receive rather than counting blocks —
/// the work block reads better as "report.pdf" than "1 attachment(s)".
fn summarize_attachments(blocks: &[ContentBlock]) -> String {
    let named: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::File { filename, .. } => Some(filename.as_str()),
            ContentBlock::Image { mime_type, .. } | ContentBlock::Audio { mime_type, .. } => {
                Some(mime_type.as_str())
            }
            _ => None,
        })
        .collect();
    if named.is_empty() {
        return format!("{} attachment(s)", blocks.len());
    }
    truncate_summary(&named.join(", "))
}

/// Derive the `(status, summary)` for a finished tool call's
/// `ToolCompleted` progress event from its result. Presentation-only and
/// content-light; the summary still passes the leak boundary before it is
/// emitted. Mirrors the result match the loop runs for the LLM-facing
/// `tool_result` text.
fn tool_completion_summary(executed: &ExecutedTool) -> (ToolStatus, String) {
    // A recorded `Deny` is the authoritative denial signal, whatever shape the
    // refusal took: the pre-execute gate raises a typed `ToolError::Denied`,
    // but a tool that prompts MID-CALL folds the refusal into its own (untyped)
    // error — and both must read as "denied", not "the tool crashed".
    if executed.approval == Some(ApprovalDecision::Deny) {
        return (ToolStatus::Denied, "denied".to_string());
    }
    match &executed.output {
        Ok(ToolOutput::Text(s)) => (ToolStatus::Ok, summarize_text(s)),
        Ok(ToolOutput::Json(_)) => (ToolStatus::Ok, "ok".to_string()),
        Ok(ToolOutput::WithAttachments { attachments, .. }) => {
            (ToolStatus::Ok, summarize_attachments(attachments))
        }
        Ok(ToolOutput::MultiModalText { llm_images, .. }) => {
            (ToolStatus::Ok, format!("{} image(s)", llm_images.len()))
        }
        Ok(ToolOutput::Error(msg)) => (ToolStatus::Error, truncate_summary(msg)),
        Err(e) => (ToolStatus::Error, truncate_summary(&e.to_string())),
    }
}

/// Compute the byte index at which it is safe to flush `pending` to the
/// output stream. Anything after that index is withheld because it might
/// be the beginning of a `[{REDACTED_SECRET_...}]` placeholder split
/// across chunks.
///
/// Returns `pending.len()` when no partial placeholder is pending, or a
/// smaller value pointing at the earliest `[` of a potential placeholder.
/// If the buffer grows past `STREAM_BUFFER_HIGH_WATER` we flush the whole
/// thing to avoid unbounded buffering on pathological input.
fn safe_flush_boundary(pending: &str) -> usize {
    if pending.len() > STREAM_BUFFER_HIGH_WATER {
        return pending.len();
    }
    // Placeholders open with `[{`. If the last `[{` has no matching `}]`
    // after it, it might be a placeholder split across chunks — withhold
    // from that `[{`. A lone trailing `[` could become `[{` when the next
    // chunk lands, so withhold it too.
    if let Some(idx) = pending.rfind("[{") {
        let tail = &pending[idx..];
        if !tail.contains("}]") {
            return idx;
        }
    }
    if pending.ends_with('[') {
        return pending.len() - 1;
    }
    pending.len()
}

/// Drop leading whitespace from `chunk` until the first non-whitespace
/// character has been observed across the stream.
///
/// `stripped` is the cross-chunk flag tracking "have we emitted any real
/// content yet?". While `false`, whitespace at the head of `chunk` is
/// removed; the first non-whitespace char flips the flag and every
/// subsequent call is a pass-through (interior whitespace, paragraph
/// breaks, code-block indentation are all preserved).
///
/// Returns `true` when the (possibly trimmed) chunk has content the
/// caller should emit, `false` when the chunk was pure whitespace and
/// should be skipped.
fn skip_leading_whitespace(chunk: &mut String, stripped: &mut bool) -> bool {
    if *stripped {
        return !chunk.is_empty();
    }
    let after = chunk.trim_start();
    if after.is_empty() {
        chunk.clear();
        return false;
    }
    if after.len() != chunk.len() {
        let kept = after.to_string();
        *chunk = kept;
    }
    *stripped = true;
    true
}

/// An empty assistant response — the sentinel a cancelled provider call
/// resolves to (the in-flight request was dropped before it produced
/// anything), so the cancel handling has a uniform `LlmResponse` to inspect.
fn empty_llm_response() -> LlmResponse {
    LlmResponse {
        content: String::new(),
        content_blocks: Vec::new(),
        tool_calls: Vec::new(),
        usage: TokenUsage::default(),
        thinking: None,
    }
}

/// Blocks safe to persist from an assistant turn cancelled mid-stream:
/// rendered text and completed thinking, with the cancelled-turn marker
/// ([`baybo_context::prompts::cancelled_turn`]) appended so the model knows the
/// turn was cut short. The marker is model-facing framing; display surfaces
/// strip it before rendering the partial reply. A streamed-but-undispatched
/// `ToolUse` is dropped — persisting a `tool_use` with no matching
/// `tool_result` would wedge the next request's provider validation.
fn salvage_partial_blocks(resp: &LlmResponse) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = resp
        .content_blocks
        .iter()
        .filter(|b| match b {
            ContentBlock::Text(t) => !t.trim().is_empty(),
            ContentBlock::Thinking { .. } => true,
            _ => false,
        })
        .cloned()
        .collect();
    if blocks.is_empty() {
        return blocks;
    }
    // Fold the marker into the trailing text rather than emitting a second
    // adjacent text block; a thinking-only salvage gets its own marker block.
    match blocks.last_mut() {
        Some(ContentBlock::Text(t)) => t.push_str(baybo_context::prompts::cancelled_turn::SUFFIX),
        _ => blocks.push(ContentBlock::Text(
            baybo_context::prompts::cancelled_turn::marker_block_text(),
        )),
    }
    blocks
}

/// Error returned by `call_llm` when the turn is cancelled while the LLM
/// call is in flight. Carries the partial assistant content salvaged from a
/// mid-stream cancel so `run_iteration` can persist it — keeping the
/// cancelled turn's work block alive across a page reload — before
/// propagating the abort. `partial` is empty when nothing was produced
/// (a non-streaming call dropped before its single response).
#[derive(Debug)]
struct CancelledTurn {
    partial: Vec<ContentBlock>,
}

impl std::fmt::Display for CancelledTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LLM call cancelled mid-flight")
    }
}

impl std::error::Error for CancelledTurn {}

/// `try_send` drops on a full channel: notices are non-load-bearing
/// (the verdict still lands in the trace) and `SessionNotifier::emit`
/// is sync, so blocking the tool path on backpressure would be worse
/// than losing the line.
struct DeltaTxNotifier {
    tx: tokio::sync::mpsc::Sender<AgentOutput>,
    session_id: baybo_model::SessionId,
    user_id: String,
    channel: baybo_model::ChannelType,
}

impl baybo_tools::SessionNotifier for DeltaTxNotifier {
    fn emit(&self, level: baybo_tools::NoticeLevel, summary: &str, detail: &str) {
        let level = match level {
            baybo_tools::NoticeLevel::Info => baybo_channels::NoticeLevel::Info,
            baybo_tools::NoticeLevel::Warn => baybo_channels::NoticeLevel::Warn,
            baybo_tools::NoticeLevel::Error => baybo_channels::NoticeLevel::Error,
        };
        let text = if detail.is_empty() {
            summary.to_string()
        } else {
            format!("{summary}: {detail}")
        };
        let _ = self.tx.try_send(AgentOutput {
            session_id: self.session_id.clone(),
            user_id: self.user_id.clone(),
            channel: self.channel.clone(),
            // The one mid_turn=true source: every SessionNotifier emission is
            // by construction an aside from inside a running tool call.
            event: AgentEvent::Notice {
                level,
                text,
                mid_turn: true,
                durable_id: None,
            },
        });
    }
}

/// What one `LlmIteration` step's body produced. The terminal-vs-loop
/// distinction lives here (rather than the body short-circuiting via
/// `?`) so the `with_step` wrapper sees a clean `Ok(...)` either way
/// and closes the step before the parent loop runs the next thing.
enum IterationOutcome {
    /// Final assistant response — caller returns this from `run_inner`.
    /// `outgoing.content` is both the channel-bound reply and the persisted
    /// assistant turn: text / thinking, plus any media the turn's tools
    /// produced (`ToolOutput::WithAttachments`) folded in at the tail.
    Final { outgoing: OutgoingMessage },
    /// LLM emitted tool calls; loop continues. `task_mutated` is `true`
    /// when one of this iteration's tool calls changed the planning
    /// checklist, so the caller refreshes the reminder before the next
    /// LLM call.
    Continue { task_mutated: bool },
}

/// Captured inputs for the deferred `Memory::on_turn_complete` write. Built
/// inside `with_turn`'s body at the Final-iteration boundary and returned up
/// so `run()` can fire `spawn_turn_complete_write` **after** `with_turn`
/// commits the turn — otherwise a cancel-race in `with_turn`'s post-body
/// window could let a memorized turn outlive a `Cancelled` turn row.
struct PendingMemoryWrite {
    user_input: Vec<ContentBlock>,
    final_output: Vec<ContentBlock>,
}

pub struct UserInterjectionInput {
    pub content: Vec<ContentBlock>,
    pub platform_msg_id: String,
}

/// Source of mid-turn user messages ("interjections") that arrived while the
/// loop was running. Consulted at each tool boundary (after a tool batch, before
/// the next LLM call) — never mid-call, so injection stays non-preemptive.
/// Implemented by the actor over its mailbox (draining the leading run of
/// non-slash `UserInput`s); a fake stands in for it in tests. Returns each
/// injectable message in arrival order, including the channel idempotency key so
/// reconnect/history replay can dedup its user bubble. Empty means nothing is
/// queued. See `docs/mid-turn-user-interjection.md`.
///
/// `Send` supertrait so the `&mut dyn InterjectionSource` the loop holds across
/// `.await` points keeps the agent task `Send`.
pub trait InterjectionSource: Send {
    fn drain_injectable(&mut self) -> Vec<UserInterjectionInput>;
    /// Drop any queued injectable messages without running them. Used when a
    /// turn is `/stop`-cancelled so client-fired interjections still sitting in
    /// the mailbox don't run as follow-up turns once the actor resumes its loop.
    fn discard_pending(&mut self) {
        let _ = self.drain_injectable();
    }
}

/// Core conversation loop: LLM call -> parse -> Tool/Skill dispatch -> repeat.
/// The recall query for a turn, or `None` for turn kinds that don't recall.
/// Memory recall/write run only for `UserChat` and `Cron` turns — `Compact`,
/// `Spawned` (subagent), `SubagentNotification`, and `CronNotification` have no
/// direct user input and would pollute or double-write (a `CronNotification`
/// also runs no LLM call at all, so there is nothing to recall *for*). The
/// exhaustive match forces a classification when a new `TurnInput` variant is
/// added.
fn memory_recall_query(input: &TurnInput) -> Option<Vec<ContentBlock>> {
    match input {
        TurnInput::UserChat { content } => Some(content.clone()),
        TurnInput::Cron { action_payload } => Some(cron_prompt_blocks(action_payload)),
        TurnInput::Compact
        | TurnInput::Spawned { .. }
        | TurnInput::SubagentNotification { .. }
        | TurnInput::CronNotification { .. } => None,
    }
}

/// Best-effort extraction of a cron fire's prompt text for the recall query.
/// The cron router writes `action_payload` as `{cron_job_id, prompt}` (an
/// opaque trace blob — see `baybo_turn::TurnInput::Cron`); a missing or non-string
/// `prompt` yields an empty query, so recall degrades to a no-op rather than
/// coupling hard to that shape.
fn cron_prompt_blocks(action_payload: &serde_json::Value) -> Vec<ContentBlock> {
    match action_payload.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => vec![ContentBlock::Text(p.to_string())],
        _ => Vec::new(),
    }
}

/// True for sessions spawned as a subagent (lineage `Subagent`); false for
/// root sessions (no lineage). The single home for the subagent-vs-root
/// classification — exhaustive on `LineageKind` so a new spawn kind forces a
/// decision here rather than silently defaulting at each call site.
fn is_subagent(session: &Session) -> bool {
    match &session.lineage {
        None => false,
        Some(l) => match &l.kind {
            LineageKind::Subagent => true,
        },
    }
}

/// Whether the `on_session_end` memory hook should fire for this session.
/// The session-level analogue of [`memory_recall_query`]: only sessions a person
/// would call "theirs" — root `User`/`Cron` sessions, not subagents.
/// Subagent actors send `ActorStop` when they finish, but their shutdown
/// is not a user-session ending. The exhaustive `TriggerSource` arm forces a
/// classification when a new trigger variant is added.
fn should_fire_session_end(session: &Session) -> bool {
    let user_trigger = match &session.trigger {
        TriggerSource::User | TriggerSource::Cron { .. } => true,
    };
    user_trigger && !is_subagent(session)
}

pub struct AgentLoop {
    /// Currently-active client, re-resolved from `llm_pool` at the
    /// start of each turn ([`Self::refresh_active_llm`]) so a config
    /// hot-reload takes effect on the next message.
    llm_client: Arc<BillableLlm>,
    /// Auxiliary model for this turn, re-resolved alongside
    /// [`Self::llm_client`]. Drives title generation and everything a
    /// tool reaches through `ToolContext::lite_llm` (the Bash risk
    /// judges, WebFetch's page summary). Equals `llm_client` when no
    /// lite model is configured anywhere.
    ///
    /// Context compression and the progress observer deliberately stay
    /// on `llm_client`: their input is the session transcript, i.e. the
    /// exact prefix provider prompt-caching keeps warm, and that cache is
    /// per-model — sending it to a second model turns every call into a
    /// cold full-transcript read.
    lite_client: Arc<BillableLlm>,
    /// Hot-swappable pool handle this loop re-resolves against per turn.
    llm_pool: crate::runtime::llm_pool::LlmPoolHandle,
    /// The pin this loop resolves: `None` ⇒ pool default (user / cron
    /// actors); `Some` ⇒ a subagent's pinned entry name.
    initial_llm: Option<LlmEntryName>,
    /// The model WITHIN `initial_llm`'s entry (a `model_list` id), or
    /// `None` for the entry's default model. Paired with `initial_llm`
    /// through every re-resolve so a per-session model pick takes effect.
    initial_model: Option<String>,
    /// Per-session reasoning-effort pin, set on every turn's `ChatRequest`
    /// (`None` ⇒ the entry's construction-time default). Consumed only by
    /// openai-subscription. This is the chat header's thinking level, kept
    /// PER-SESSION rather than a global entry edit.
    initial_effort: Option<String>,
    tool_registry: Arc<ToolRegistry>,
    tool_executor: Arc<ToolExecutor>,
    context_manager: ContextManager,
    max_iterations: usize,
    security_gateway: Arc<SecurityGateway>,
    error_handler: ErrorHandler,
    /// Cross-session manager — used by passes that operate across sessions
    /// (the session-end memory write, the progress observer's durable
    /// shadow, title generation). Distinct from the `SessionManager`
    /// plumbed inside `ContextManager` because that one is
    /// per-session-bound.
    sessions: Option<Arc<crate::SessionManager>>,
    /// Pluggable long-term memory. `None` disables every memory hook (recall,
    /// `on_turn_complete`) — the runtime wires `None` until a real
    /// implementation is registered.
    memory: Option<Arc<dyn Memory>>,
    /// Durable per-session planning checklist (`Task*`). The loop
    /// loads it each turn — and after any checklist-mutating tool call — to
    /// refresh the transient reminder the model sees via `ContextManager`.
    /// Always present in production (sourced from the `Store` bundle).
    task_store: Arc<dyn baybo_store::TaskStore>,
    /// Monotonic per-turn counter (one tick per `run_inner`) backing the task
    /// reminder throttle below. In-memory: a rehydrated actor restarts at 0,
    /// which just re-grants the start-of-session grace window.
    turn_counter: u64,
    /// `turn_counter` value when the model last ran a checklist-mutating tool
    /// (`TaskCreate` / `TaskUpdate`). `0` until the first management.
    last_task_management_turn: u64,
    /// `turn_counter` value when the task reminder was last injected. `0` until
    /// the first injection. With both starting at `0`, the throttle holds the
    /// reminder for the first [`TURNS_SINCE_WRITE`] turns of a session.
    last_reminder_turn: u64,
    /// Once-per-actor title-generation guard. Durable `Session.title`
    /// prevents repeats after rehydration.
    title_generation: Option<tokio::task::JoinHandle<()>>,
    /// Live-title broadcaster. `None` disables title generation.
    title_sink: Option<Arc<dyn crate::runtime::title::SessionTitleSink>>,
    /// Read-before-write tracker for this session's `Edit`/`Write` tools.
    /// Lives for the actor's lifetime so a `Read` in one turn satisfies an
    /// `Edit` in a later turn, and is threaded into every tool call via
    /// [`ToolExecutor::execute`]. On hydration it is rebuilt from the restored
    /// transcript ([`ReadTracker::rebuild_from_messages`]) — each `Read` result
    /// row persists the fingerprint it observed — so a read survives actor
    /// eviction / process restart. Any gap fails closed (forces a re-read).
    read_tracker: ReadTracker,
}

/// Construction bundle for [`AgentLoop`]. Every field maps 1:1 to a
/// field on the loop; required deps are bare, optional deps are
/// `Option<T>`. Callers populate it via struct-literal syntax and pass
/// it to [`AgentLoop::from_config`] — no chained setters, no
/// post-construction mutability.
pub struct AgentLoopConfig {
    /// Process-wide pool of guarded LLM clients keyed by entry name.
    pub llm_pool: crate::runtime::llm_pool::LlmPoolHandle,
    /// Initial pick for the active LLM. `None` ⇒ pool default.
    pub initial_llm: Option<LlmEntryName>,
    /// Model within `initial_llm`'s entry (`None` ⇒ the entry's default).
    pub initial_model: Option<String>,
    /// Per-session reasoning effort (`None` ⇒ entry default).
    pub initial_effort: Option<String>,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_executor: Arc<ToolExecutor>,
    pub context_manager: ContextManager,
    pub max_iterations: usize,
    pub security_gateway: Arc<SecurityGateway>,
    /// Cross-session manager. Used by the session-end memory write, the
    /// progress observer's durable shadow, and title generation.
    pub sessions: Option<Arc<crate::SessionManager>>,
    /// Pluggable long-term memory handle — one registered implementation, or
    /// `None` to disable the memory hooks (recall / `on_turn_complete`).
    pub memory: Option<Arc<dyn Memory>>,
    /// Durable per-session planning-checklist store backing the `Task*` tools
    /// and the per-turn reminder.
    pub task_store: Arc<dyn baybo_store::TaskStore>,
    /// Live-title broadcaster. `None` disables title generation.
    pub title_sink: Option<Arc<dyn crate::runtime::title::SessionTitleSink>>,
}

/// Task-reminder throttle (mirrors Claude Code's `TODO_REMINDER_CONFIG`): the
/// model-facing reminder is injected only once the model has gone
/// `TURNS_SINCE_WRITE` turns without managing tasks AND it has been at least
/// `TURNS_BETWEEN_REMINDERS` turns since the last reminder — so it nudges
/// periodically instead of riding every request. The web `TaskList` surface is
/// **not** throttled (it tracks the live list).
const TURNS_SINCE_WRITE: u64 = 10;
const TURNS_BETWEEN_REMINDERS: u64 = 10;

/// User-facing line for a compaction that could not be applied because the
/// summarizer call failed. Shared by the threshold path and `/compact` so the
/// same event reads the same way wherever it surfaces. The conversation is
/// deliberately left intact — nothing was dropped to make room.
fn compaction_failed_text(reason: &str) -> String {
    format!("Context compaction failed; the conversation is unchanged. Reason: {reason}")
}

/// What [`AgentLoop::compact_now`] hands back for the caller to ship as a
/// notice. The severity travels with the text so a failed compaction cannot
/// reach the user as an `Info` line that reads like a confirmation.
pub struct CompactionNotice {
    pub level: baybo_channels::NoticeLevel,
    pub text: String,
}

impl CompactionNotice {
    fn info(text: &str) -> Self {
        Self {
            level: baybo_channels::NoticeLevel::Info,
            text: text.to_string(),
        }
    }

    fn warn(text: &str) -> Self {
        Self {
            level: baybo_channels::NoticeLevel::Warn,
            text: text.to_string(),
        }
    }
}

/// The throttle decision: inject the model-facing task reminder this turn iff the
/// model has gone `TURNS_SINCE_WRITE` turns without managing tasks AND it has
/// been `TURNS_BETWEEN_REMINDERS` turns since the last reminder.
fn should_inject_task_reminder(
    turn_counter: u64,
    last_task_management_turn: u64,
    last_reminder_turn: u64,
) -> bool {
    turn_counter.saturating_sub(last_task_management_turn) >= TURNS_SINCE_WRITE
        && turn_counter.saturating_sub(last_reminder_turn) >= TURNS_BETWEEN_REMINDERS
}

impl AgentLoop {
    pub fn from_config(config: AgentLoopConfig) -> Self {
        let AgentLoopConfig {
            llm_pool,
            initial_llm,
            initial_model,
            initial_effort,
            tool_registry,
            tool_executor,
            context_manager,
            max_iterations,
            security_gateway,
            sessions,
            memory,
            task_store,
            title_sink,
        } = config;
        let (llm_client, lite_client) = {
            let pool = llm_pool.read();
            let (client, _effective_name) =
                pool.resolve(initial_llm.as_ref(), initial_model.as_deref());
            let (lite, _lite_name) =
                pool.resolve_lite(initial_llm.as_ref(), initial_model.as_deref());
            (client, lite)
        };
        let mut context_manager = context_manager;
        context_manager.set_active_model_context_window(llm_client.model_info().context_window);

        Self {
            llm_client,
            lite_client,
            llm_pool,
            initial_llm,
            initial_model,
            initial_effort,
            tool_registry,
            tool_executor,
            context_manager,
            max_iterations,
            security_gateway,
            error_handler: ErrorHandler::default(),
            sessions,
            memory,
            task_store,
            turn_counter: 0,
            last_task_management_turn: 0,
            last_reminder_turn: 0,
            title_generation: None,
            title_sink,
            read_tracker: ReadTracker::default(),
        }
    }

    /// Delegate to `ContextManager::restore_from_store` — the manager
    /// is bound to its session at construction time and owns the
    /// load path. Kept on `AgentLoop` so `AgentActor::run` doesn't
    /// have to reach inside the loop's private state.
    pub async fn restore_transcript_from_store(&mut self) {
        self.context_manager.restore_from_store().await;
        // Recover the read-before-write tracker from the restored transcript:
        // each `Read` result row carries the fingerprint it observed, so a read
        // that happened before this actor was evicted/restarted still satisfies
        // a later `Edit` without forcing a redundant re-read.
        self.read_tracker
            .rebuild_from_messages(self.context_manager.messages());
    }

    /// Load the session's planning checklist, (throttled) refresh the transient
    /// reminder the model sees, and push the live list to any work-block client
    /// (the web checklist) via `delta_tx`. `inject` is the per-turn throttle
    /// decision: when `false`, the model-facing reminder is cleared (the web
    /// surface is **never** throttled). A store error degrades to leaving the
    /// prior reminder in place (logged) rather than dropping it.
    async fn refresh_task_reminder(
        &mut self,
        session: &Session,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        inject: bool,
    ) {
        let tasks = match self.task_store.list(&session.id).await {
            Ok(tasks) => tasks,
            Err(e) => {
                warn!(session_id = %session.id, error = %e, "failed to load session tasks for the checklist reminder");
                return;
            }
        };
        // Model-facing reminder: only when the throttle allows AND there's a
        // list to show. Otherwise clear it so it doesn't ride this request.
        let shown = inject && !tasks.is_empty();
        self.context_manager
            .refresh_task_reminder(if shown { tasks.as_slice() } else { &[] });
        if shown {
            self.last_reminder_turn = self.turn_counter;
        }
        // Borrow of `tasks` above is done; move it onto the event so the web
        // dashboard can render the live checklist (unthrottled, idempotent).
        if let Some(tx) = delta_tx {
            let _ = tx
                .send(AgentOutput {
                    session_id: session.id.clone(),
                    user_id: session.user.id.clone(),
                    channel: session.channel.clone(),
                    event: AgentEvent::TaskList(tasks),
                })
                .await;
        }
    }

    /// Run the main conversation loop for a single user message.
    ///
    /// When `delta_tx` is `Some`, each text chunk emitted by the LLM is
    /// forwarded as `AgentEvent::AnswerDelta` so adapters that support partial
    /// rendering (e.g. the TUI) can show incremental output. The final
    /// `OutgoingMessage` returned here should still be dispatched by the
    /// caller as `AgentEvent::Message` so non-streaming adapters receive
    /// the canonical response.
    // `turn_input` records why this turn exists (provenance: which trigger
    // kicked it off — User / Cron / Spawned), used for the TurnSpec.
    // The turn's triggering message is appended to the transcript by the
    // actor *before* this runs (via `append_user_message` / `append_cron_fire`
    // / `append_background_notification_prompt_once`), so the loop iterates
    // the current context rather than appending here.
    /// Re-resolve the active client from the (possibly hot-swapped)
    /// pool at the start of a turn. When the resolved model changed
    /// since the last turn, swap the client, rebuild the billed-chat
    /// factory so in-tool LLM calls bill the new model, and update the
    /// context window so compression gates on the new model's limit.
    /// The tokenizer is intentionally left as-is — tiktoken is an
    /// estimate and `TokenCalibration` corrects the drift within a few
    /// turns. See `docs/config-hot-reload.md`.
    fn refresh_active_llm(&mut self) {
        let pool = Arc::clone(&self.llm_pool.read());
        let (client, _name) =
            pool.resolve(self.initial_llm.as_ref(), self.initial_model.as_deref());
        // Resolved unconditionally: the lite cascade can change without the
        // main client changing (an entry gaining a `lite_model`, or the Lite
        // tier being re-pointed), and the pointer check below would then
        // short-circuit past it.
        let (lite, _lite_name) =
            pool.resolve_lite(self.initial_llm.as_ref(), self.initial_model.as_deref());
        self.lite_client = lite;
        // Compare by pointer, not model id: a config reload swaps in a
        // fresh `Arc<BillableLlm>` even when the model id is unchanged
        // (a `base_url`, credential, `reasoning_effort`, or
        // `context_window` edit), and those must take effect too. An
        // unchanged pool returns a clone of the same `Arc`, so this stays
        // a no-op on the common path.
        if Arc::ptr_eq(&client, &self.llm_client) {
            return;
        }
        info!(
            model = %client.model_info().id,
            "rebinding agent loop to the reloaded LLM client for this turn",
        );
        let window = client.model_info().context_window;
        self.llm_client = client;
        self.context_manager.set_active_model_context_window(window);
    }

    /// Re-pin which `baybo.json` entry this loop resolves against and
    /// apply it now (swaps the client + context window via
    /// [`Self::refresh_active_llm`]) so the next turn runs on the new
    /// model. `None` reverts to the pool default. Drives the chat
    /// per-session model switch ([`crate::actor::AgentMessage::SetModel`]);
    /// the actor also persists the pin to `session.state.last_llm` so it
    /// survives eviction.
    pub fn set_initial_llm(
        &mut self,
        llm: Option<LlmEntryName>,
        model: Option<String>,
        effort: Option<String>,
    ) {
        self.initial_llm = llm;
        self.initial_model = model;
        self.initial_effort = effort;
        self.refresh_active_llm();
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &mut self,
        session: &mut Session,
        turn_input: TurnInput,
        turn_lifecycle: &Arc<TurnLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        parent_turn_id: Option<TurnId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        cancel_token: CancellationToken,
        interjections: Option<&mut dyn InterjectionSource>,
        // A recurring cron fire hands its tools a silence handle here so
        // `report_nothing` can suppress the fire's notification. `None` for
        // every other turn (see `AgentActor::dispatch_cron_prompt`).
        notify_silence: Option<baybo_tools::NotifySilence>,
    ) -> anyhow::Result<OutgoingMessage> {
        self.refresh_active_llm();
        // Memory recall query (and write eligibility) for this turn — `None`
        // for kinds that don't participate (Spawned / notification).
        let memory_query = memory_recall_query(&turn_input);
        let is_user_turn = matches!(turn_input.input_kind(), baybo_turn::TurnInputKind::UserChat);
        let spec = TurnSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            input: turn_input,
            parent_turn_id,
        };
        // Capture what the post-`with_turn` memory spawn needs from `self`
        // and `session` before the closure takes `&mut self` + `&mut session`
        // by move — once `with_turn`'s body runs we can no longer touch either
        // directly out here.
        let memory_handle = self.memory.clone();
        let memory_user_id = session.user.id.clone();
        let memory_session_id = session.id.clone();
        let memory_agent_id = session.state.agent_id_or_builtin();
        // The `on_turn_complete` spawn is intentionally OUTSIDE `with_turn`'s
        // body: `with_turn`'s post-body window can still mark the turn
        // `Cancelled` (cancel-race case 3 in `scope.rs`), in which case the
        // body's Ok is suppressed and `with_turn` returns Err — so a spawn
        // launched inside the body would persist memory for a turn the
        // runtime later treats as cancelled. Carry `PendingMemoryWrite` up
        // through `with_turn`'s `T` and fire only once it has returned Ok.
        let (outgoing, pending_write) = crate::runtime::scope::with_turn(
            turn_lifecycle,
            cancel_token.clone(),
            spec,
            |turn_id| async move {
                let (outgoing, pending) = self
                    .run_inner(
                        session,
                        span_recorder,
                        turn_id,
                        delta_tx,
                        cancel_token,
                        interjections,
                        memory_query,
                        is_user_turn,
                        notify_silence.clone(),
                    )
                    .await?;
                let output = if notify_silence.as_ref().is_some_and(|s| s.requested()) {
                    // A recurring cron fire called `report_nothing`: complete
                    // with a reply-less structured result so the Completed edge
                    // carries no reply ordinal and the push dispatcher skips it
                    // (`gateway::push` returns early on `reply_ordinal: None`).
                    // The dispatch is suppressed and the fire session hidden by
                    // `dispatch_cron_prompt`; nothing is deleted.
                    TurnOutput::Structured {
                        value: serde_json::json!({ "notify": "suppressed" }),
                    }
                } else {
                    TurnOutput::Message {
                        content: outgoing.content.clone(),
                        // The reply's persisted ordinal (captured from the store
                        // append) rides the Completed event so push reads exactly
                        // this row without a read-after-write poll.
                        ordinal: outgoing.ordinal,
                    }
                };
                let pending_with_id = pending.map(|p| (turn_id, p));
                Ok((output, (outgoing, pending_with_id)))
            },
        )
        .await?;
        // Past the cancel-race window — `with_turn` returned Ok, so the turn
        // row is `Complete`. Safe to bill the memory write against it.
        if let Some((turn_id, write)) = pending_write {
            Self::spawn_turn_complete_write(
                memory_handle,
                MemoryScope {
                    user_id: memory_user_id,
                    session_id: memory_session_id,
                    turn_id,
                    agent_id: memory_agent_id,
                },
                span_recorder,
                write.user_input,
                write.final_output,
            );
        }
        Ok(outgoing)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        cancel_token: CancellationToken,
        mut interjections: Option<&mut dyn InterjectionSource>,
        memory_query: Option<Vec<ContentBlock>>,
        is_user_turn: bool,
        notify_silence: Option<baybo_tools::NotifySilence>,
    ) -> anyhow::Result<(OutgoingMessage, Option<PendingMemoryWrite>)> {
        self.context_manager.ensure_seeded().await;

        // Fire-and-forget at turn start so the title derives concurrently with
        // the answer (it needs only the question, already in context).
        self.maybe_generate_title(session, span_recorder, turn_id, is_user_turn, &cancel_token)
            .await;

        // Tool-authored notices (`AgentEvent::Notice`) ride the turn-wide
        // delta_tx directly, not the per-iteration `iter_delta_tx`: they
        // are a distinct output variant from the LLM's streamed `AnswerDelta`
        // and must reach the channel on every iteration, independent of
        // any per-iteration streaming decision.
        let notifier: Option<Arc<dyn baybo_tools::SessionNotifier>> = delta_tx.as_ref().map(|tx| {
            Arc::new(DeltaTxNotifier {
                tx: tx.clone(),
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
            }) as Arc<dyn baybo_tools::SessionNotifier>
        });

        // Expand an explicit `/command` skill invocation before the loop:
        // context reads the matching skill's body and appends it (persisted +
        // JSONL-logged) as a hidden agent-context row for the loop to act on.
        self.context_manager.expand_slash_command().await;

        // Recall relevant long-term memories for the triggering input and
        // inject them (framed) before the first LLM call. No-op without a
        // memory impl or for ineligible turn kinds (`memory_query` is `None`).
        if let Some(query) = memory_query.as_deref() {
            self.recall_and_inject(query, session, span_recorder, turn_id, &cancel_token)
                .await;
        }
        // Accumulates this turn's user-authored input (initial prompt + any
        // mid-turn interjections) for the `on_turn_complete` write at turn end.
        let mut turn_user_input: Vec<ContentBlock> = memory_query.clone().unwrap_or_default();
        // Media the turn's tools produced (`ToolOutput::WithAttachments`),
        // folded into the final assistant row so it persists. Dropped on any
        // non-`Final` exit: attachments are durable only when the turn is.
        let mut turn_attachments: Vec<ContentBlock> = Vec::new();

        // Iterative LLM loop
        let mut iterations = 0;
        // A compaction that fails leaves the transcript over threshold, so the
        // gate re-fires on every remaining iteration. Retrying is right — the
        // failure is usually a transient provider error and the transcript
        // only grows — but telling the user again on each one is not.
        let mut compaction_failure_reported = false;
        let turn_started = std::time::Instant::now();
        let mut observer_state = ObserverState::default();
        // Aborts an observer still in flight when the turn ends — the last
        // summary can't be drained once the next iteration is the final answer.
        // Drop guard fires on every exit; child token, so `/stop` reaches it.
        let observer_cancel = cancel_token.child_token();
        let _observer_cancel_guard = observer_cancel.clone().drop_guard();
        // Drives the per-turn planning-checklist reminder: load on the first
        // iteration (to surface tasks a prior turn persisted) and reload after
        // any iteration that ran a checklist-mutating tool, so the web checklist
        // and (when injected) the model reminder stay current.
        let mut task_reminder_dirty = true;
        // Task-reminder throttle: one tick per turn; inject the model-facing
        // reminder only after the model has ignored task management for
        // `TURNS_SINCE_WRITE` turns and not been reminded for
        // `TURNS_BETWEEN_REMINDERS`. Decided once here so it's stable across this
        // turn's iterations; the web `TaskList` emission below ignores it.
        self.turn_counter += 1;
        let inject_task_reminder = should_inject_task_reminder(
            self.turn_counter,
            self.last_task_management_turn,
            self.last_reminder_turn,
        );
        loop {
            // Cooperative cancel checkpoint between iterations. Without
            // this, a `cancel(turn_id, ...)` admin call (which trips the
            // registered token before flipping the row) lets the loop
            // finish whatever it's doing and run another LLM call /
            // compress / tool-call before observing the cancel. Tools
            // and the LLM still get the token via their own paths;
            // this catches the orchestration-layer wait windows.
            if cancel_token.is_cancelled() {
                warn!(turn_id = %turn_id, iterations, "cancel observed at iteration boundary; aborting loop");
                return Err(anyhow::anyhow!("agent loop cancelled"));
            }
            if iterations >= self.max_iterations {
                warn!(max = self.max_iterations, "max iterations reached");
                break;
            }
            iterations += 1;

            // Both gate on iter > 1: iteration 1 is the original turn (no tool
            // batch has run yet to inject after), and a Final response returns
            // before looping, so it never reaches here.
            if iterations > 1 {
                // Drain any messages the user sent while the loop was running
                // and inject them (framed as steering) BEFORE the next LLM call,
                // so the user can steer an in-progress turn. Messages that don't
                // make a boundary fall through to the next turn. See
                // docs/mid-turn-user-interjection.md.
                let drained = self.drain_user_interjections(&mut interjections).await;
                // Recall against each freshly-drained interjection so the next
                // LLM call also sees memory relevant to the steering message,
                // and fold it into this turn's input for the end-of-turn write.
                if memory_query.is_some() {
                    for content in &drained {
                        self.recall_and_inject(
                            content,
                            session,
                            span_recorder,
                            turn_id,
                            &cancel_token,
                        )
                        .await;
                        turn_user_input.extend(content.iter().cloned());
                    }
                }
            }

            // Refresh the checklist reminder (in `ContextManager`) BEFORE the
            // compression gate, so the gate charges the reminder's tokens to the
            // budget and the list rides this request's tail. Only fires on
            // iteration 1 and after a checklist mutation; cheap indexed read.
            if task_reminder_dirty {
                self.refresh_task_reminder(session, delta_tx.as_ref(), inject_task_reminder)
                    .await;
                task_reminder_dirty = false;
            }

            // Proactive compression before building the ChatRequest.
            self.compress_if_needed(
                session,
                span_recorder,
                turn_id,
                &cancel_token,
                delta_tx.as_ref(),
                &mut compaction_failure_reported,
            )
            .await?;

            // Stream deltas on every iteration, not just the first. The
            // final answer can land on any iteration (it follows however
            // many tool-call rounds the model needs), and the TUI renders
            // the final message body *only* from streamed deltas — its
            // `finalize_stream` skips `Text` blocks via
            // `render_non_text_blocks`. An unstreamed post-tool answer
            // would persist and reach the client over the wire yet never
            // render. Streaming each iteration keeps that answer visible.
            let iter_delta_tx = delta_tx.as_ref();

            let outcome = crate::runtime::scope::with_step(
                span_recorder.as_ref(),
                turn_id,
                StepKind::LlmIteration,
                Some((&cancel_token, baybo_turn::CancelReason::ParentCancelled)),
                |step| {
                    let fut = self.run_iteration(
                        session,
                        span_recorder,
                        step,
                        turn_id,
                        iterations,
                        iter_delta_tx,
                        notifier.clone(),
                        &cancel_token,
                        &mut turn_attachments,
                        notify_silence.clone(),
                    );
                    async move { Ok((LifecycleOutcome::Ok, fut.await?)) }
                },
            )
            .await?;

            match outcome {
                IterationOutcome::Final { outgoing } => {
                    // Capture the memory write inputs and return them up to
                    // `run()` — the actual `spawn_turn_complete_write` fires
                    // **after** `with_turn` accepts the turn, so a cancel-race
                    // in `with_turn`'s post-body window can't memorize a
                    // cancelled turn.
                    let pending = memory_query.is_some().then(|| PendingMemoryWrite {
                        user_input: std::mem::take(&mut turn_user_input),
                        final_output: outgoing.content.clone(),
                    });
                    return Ok((outgoing, pending));
                }
                IterationOutcome::Continue { task_mutated } => {
                    // Continue to the next LLM iteration; `run_iteration`
                    // has already appended each tool's result to the
                    // context. If a checklist-mutating tool ran this
                    // iteration, the persisted list changed — reload it
                    // before the next LLM call.
                    if task_mutated {
                        task_reminder_dirty = true;
                        // Reset the throttle's "turns since task management"
                        // anchor: the model just touched the list, so don't nag.
                        self.last_task_management_turn = self.turn_counter;
                    }

                    // Observe only after a resolved tool round: the snapshot is
                    // coherent (no dangling tool_use) and never spawned for a
                    // turn that just ended on the final answer.
                    self.maybe_run_progress_observer(
                        session,
                        span_recorder,
                        turn_id,
                        &cancel_token,
                        &observer_cancel,
                        delta_tx.as_ref(),
                        iterations,
                        turn_started,
                        &mut observer_state,
                    )
                    .await;
                }
            }
        }

        // If we exhausted iterations, return what we have. `turn_attachments`
        // is dropped with the rest of this turn's locals: this path persists
        // no assistant row, so folding media into `content` would deliver it
        // live and then lose it on the next resync.
        let content = vec![ContentBlock::Text(
            "I've reached the maximum number of processing steps. Please try again with a simpler request.".to_string(),
        )];
        // Max-iterations fallback. No assistant row was persisted at
        // the loop end — the early-return path inside `run_iteration`
        // is the only one that calls `append_context_message`, so
        // there's no ordinal to stamp here. `on_turn_complete` is also
        // deliberately NOT fired on this path (`PendingMemoryWrite` is
        // `None`): memory writes only on a clean `IterationOutcome::Final`,
        // so a budget-exhausted (or cancelled / errored, which `?`-return
        // earlier) turn is never memorized.
        Ok((
            OutgoingMessage {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                content,
                reply_to: None,
                metadata: Default::default(),
                ordinal: None,
            },
            None,
        ))
    }

    /// One iteration of the agentic loop, scoped to a single
    /// `LlmIteration` step (opened by [`crate::runtime::scope::with_step`] in
    /// the caller). Calls the LLM, then executes the response's tool
    /// calls concurrently under the per-response permit pool — each call
    /// holds permits per its [`ToolConcurrency`] (`spawn_subagent` is
    /// `Independent`, so it neither waits for nor holds a permit and its
    /// fan-out runs in parallel) — and appends their results to context
    /// in declaration order.
    ///
    /// Returns [`IterationOutcome::Final`] when the LLM produced no
    /// tool calls — that's the final assistant response and the loop
    /// terminates.
    #[allow(clippy::too_many_arguments)]
    async fn run_iteration(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        step: StepHandle,
        turn_id: TurnId,
        iterations: usize,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        notifier: Option<Arc<dyn baybo_tools::SessionNotifier>>,
        cancel_token: &CancellationToken,
        turn_attachments: &mut Vec<ContentBlock>,
        notify_silence: Option<baybo_tools::NotifySilence>,
    ) -> anyhow::Result<IterationOutcome> {
        let (response, llm_span_id) = match self
            .call_llm_with_retry(session, span_recorder, &step, delta_tx, cancel_token)
            .await
        {
            Ok(pair) => pair,
            Err(e) => {
                // Cancelled mid-call: persist any partial assistant content
                // the stream produced before the abort, so the cancelled
                // turn's work block survives a page reload (reconstruction
                // reads it back from `session_messages`). Only text + thinking
                // are salvaged — never a dangling tool_use — so the row is a
                // valid standalone assistant turn for the next request.
                if let Some(cancelled) = e.downcast_ref::<CancelledTurn>()
                    && !cancelled.partial.is_empty()
                {
                    self.context_manager
                        .append(&ChatMessage::assistant(cancelled.partial.clone()))
                        .await;
                }
                return Err(e);
            }
        };

        // If no tool calls, we have the final response.
        if response.tool_calls.is_empty() {
            // Use content_blocks when available, falling back to the
            // text string.
            let mut response_blocks = if response.content_blocks.is_empty() {
                vec![ContentBlock::Text(response.content.clone())]
            } else {
                response.content_blocks.clone()
            };
            // Media the turn's tools produced rides out on this row. The
            // provider conversion drops media blocks from an assistant
            // message, so replaying this row to the LLM stays valid.
            response_blocks.extend(std::mem::take(turn_attachments));

            let final_text = baybo_llm::multimodal::extract_text(&response_blocks);

            info!(
                iterations,
                content_len = final_text.len(),
                "conversation loop complete"
            );

            // The reply blocks are both the channel-bound content and the
            // persisted assistant row. Capture the persisted ordinal so the
            // channel adapter can stamp the live `Frame::Message`.
            let assistant_msg = ChatMessage::assistant(response_blocks.clone());
            let ordinal = self.context_manager.append(&assistant_msg).await;

            return Ok(IterationOutcome::Final {
                outgoing: OutgoingMessage {
                    session_id: session.id.clone(),
                    user_id: session.user.id.clone(),
                    channel: session.channel.clone(),
                    content: response_blocks,
                    reply_to: None,
                    metadata: Default::default(),
                    ordinal,
                },
            });
        }

        // Append assistant message including thinking and tool-call
        // blocks so the LLM sees its own prior reasoning and tool
        // invocations on the next turn.
        let mut assistant_blocks = if response.content_blocks.is_empty() {
            // Fallback: build from the flat content string.
            if response.content.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(response.content.clone())]
            }
        } else {
            response.content_blocks.clone()
        };
        for tc in &response.tool_calls {
            assistant_blocks.push(ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
                signature: tc.signature.clone(),
            });
        }
        let assistant_msg = ChatMessage::assistant(assistant_blocks);
        self.context_manager.append(&assistant_msg).await;

        // Surface each tool call as a live progress line before dispatch
        // (streaming turns only; cron passes `delta_tx = None`).
        // Emitted ahead of `join_all` so the user sees "starting" before
        // any approval prompt the executor raises mid-call.
        for tc in &response.tool_calls {
            let label = self.tool_registry.progress_label(&tc.name, &tc.arguments);
            self.emit_tool_started(delta_tx, session, tc.id.clone(), tc.name.clone(), label)
                .await;
        }

        // Execute tool calls under a bounded-concurrency limiter.
        // Approved resources are shared via a Mutex so concurrent calls
        // see each other's grants immediately. Wrapped in an `Arc` so
        // that any persist-always closure injected into `ToolContext`
        // mid-execution can clone its handle into the executor boundary
        // without a borrow escape.
        //
        // Concurrency model: a per-response `Semaphore` (sized to
        // `MAX_CONCURRENT_TOOL_CALLS`) gates the futures join_all'd
        // below. A `ToolConcurrency::Concurrent` call (a read-only tool)
        // holds one permit, so up to the cap run at once; a
        // `ToolConcurrency::Exclusive` call (any tool with side effects)
        // holds *all* the permits, so it runs alone among pool calls; a
        // `ToolConcurrency::Independent` call (`spawn_subagent`) holds
        // none, so it neither waits nor blocks and self-bounds its own
        // fan-out. The post-execution pass that mutates `session.messages`
        // is kept SEQUENTIAL — appending tool results in original
        // `tool_calls` order keeps the next turn's context byte-stable
        // (prompt cache hits) and matches provider expectations that
        // tool_use ↔ tool_result pairs land in declaration order. The
        // executor + approval gate are already designed for
        // concurrency (TUI gate serialises its prompts internally).
        let approved = std::sync::Arc::new(parking_lot::Mutex::new(
            session.state.approved_resources.clone(),
        ));

        let executor = Arc::clone(&self.tool_executor);
        let session_id_for_calls = session.id.clone();
        let session_trigger_for_calls = session.trigger.clone();
        // The agent this session runs as, so a tool's memory writes and its
        // `Skill` lookups land in the right partition and scope. Unbound
        // sessions resolve to the built-in.
        let agent_for_calls = session.state.agent_id_or_builtin();
        let user_for_calls = session.user.clone();
        // Gate (Copy, captured per closure): only a user-facing session may
        // background a slow command — keeps cron / nested-subagent bash on
        // kill-on-timeout. Mirrors the subagent-conversion gate.
        let background_eligible = session.supports_background_jobs();
        let recorder_for_calls = Arc::clone(span_recorder);
        let step_for_calls = step.clone();
        let notifier_for_calls = notifier.clone();
        // Tools get the LITE client: every consumer of the tool-layer
        // side-LLM slot is an auxiliary call (the Bash risk judges,
        // WebFetch's page summary), and none of them sends the session
        // transcript, so there is no prompt cache to lose.
        let llm_for_calls = Arc::clone(&self.lite_client);
        let registry_for_calls = Arc::clone(&self.tool_registry);
        // Promote the previous response's staged reads before this batch runs,
        // so a `Read` and an `Edit`/`Write` of the same file in THIS response
        // can't authorize each other: the read stays staged until the next
        // response boundary (by when the model has actually seen its result).
        self.read_tracker.begin_response();
        let read_tracker_for_calls = self.read_tracker.clone();
        let notify_silence_for_calls = notify_silence.clone();
        let concurrency_limiter = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TOOL_CALLS));
        let exec_futures = response.tool_calls.iter().map(|tc| {
            let executor = Arc::clone(&executor);
            let notify_silence = notify_silence_for_calls.clone();
            let session_id = session_id_for_calls.clone();
            let session_trigger = session_trigger_for_calls.clone();
            let agent_id = agent_for_calls.clone();
            let user = user_for_calls.clone();
            let approved = Arc::clone(&approved);
            let recorder = Arc::clone(&recorder_for_calls);
            let step = step_for_calls.clone();
            let cancel = cancel_token.child_token();
            let notifier = notifier_for_calls.clone();
            let bind_source = Arc::clone(&llm_for_calls);
            let tool_name = tc.name.clone();
            let arguments = tc.arguments.clone();
            let tool_use_id = tc.id.clone();
            let triggering_llm_span = Some(llm_span_id);
            let limiter = Arc::clone(&concurrency_limiter);
            let read_tracker = read_tracker_for_calls.clone();
            // `Concurrent` → one permit (up to the cap run together);
            // `Exclusive` → every permit, so the call runs alone among
            // pool calls; `Independent` → no permit (self-bounded, e.g.
            // `spawn_subagent`). Unknown tools fail safe to `Exclusive`
            // inside `ToolRegistry::concurrency`.
            let permits = match registry_for_calls.concurrency(&tool_name) {
                ToolConcurrency::Concurrent => Some(1),
                ToolConcurrency::Exclusive => Some(MAX_CONCURRENT_TOOL_CALLS as u32),
                ToolConcurrency::Independent => None,
            };
            async move {
                // Hold a pool permit for the whole call (none for an
                // `Independent` call). `acquire` errors only if the
                // semaphore is closed, which never happens for this
                // per-response limiter.
                let _permit = match permits {
                    Some(n) => limiter.acquire_many_owned(n).await.ok(),
                    None => None,
                };
                debug!(tool = %tool_name, "executing tool call");
                executor
                    .execute(
                        &tool_name,
                        arguments,
                        &session_id,
                        &session_trigger,
                        &agent_id,
                        &user,
                        &approved,
                        &recorder,
                        &step,
                        triggering_llm_span,
                        tool_use_id,
                        None,
                        Some(turn_id),
                        cancel,
                        notifier,
                        Some(&bind_source),
                        background_eligible,
                        read_tracker,
                        notify_silence,
                    )
                    .await
            }
        });
        let tool_results = futures::future::join_all(exec_futures).await;

        // Sequential post-processing: append results in `tool_calls`
        // order so context state stays byte-stable across calls.
        let mut llm_visible_images: Vec<ContentBlock> = Vec::new();
        for (tool_call, executed) in response.tool_calls.iter().zip(tool_results) {
            let (status, raw_summary) = tool_completion_summary(&executed);
            let call_approval = executed.approval;
            self.emit_tool_completed(
                delta_tx,
                session,
                tool_call.id.clone(),
                status,
                raw_summary,
                call_approval,
            )
            .await;

            // Count a grouped subagent spawn into its barrier cohort so the
            // turn-end seal knows the member total. Only a *successful
            // dispatch* counts — a router-side failure (unregistered backend,
            // closed channel, …) comes back as `Ok("[subagent failed: …]")`
            // with no escorted result to ever arrive, so `is_ok()` is too
            // broad: counting it would stall the cohort until its group
            // timeout. A real dispatch returns the ack with its `bg-…` handle.
            let dispatched = matches!(
                &executed.output,
                Ok(ToolOutput::Text(t))
                    if t.starts_with(baybo_model::BACKGROUND_DISPATCH_ACK_PREFIX)
            );
            if tool_call.name == baybo_model::SPAWN_SUBAGENT_TOOL_NAME
                && dispatched
                && let Some(group) = tool_call
                    .arguments
                    .get("group")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                // Namespace the cohort by this turn's `turn_id` so reusing a
                // group name in a later turn opens a fresh cohort rather than
                // extending the prior (still-sealed, still-draining) one. The
                // spawner stamps the escorted member's `group` through the
                // same helper, so routing back into the cohort agrees.
                session
                    .state
                    .background_notifications
                    .register_group_member(turn_id, group);
            }

            let raw_result_text = match &executed.output {
                Ok(ToolOutput::Text(s)) => s.clone(),
                Ok(ToolOutput::Json(v)) => {
                    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
                }
                // Media a tool produced for the *user* (e.g. `AttachFile`) is
                // hoisted onto the turn's final assistant row; the LLM sees
                // only the text result. `MultiModalText`'s images are for the
                // LLM's own next turn: accumulated here and appended as one
                // follow-up user-role message after the tool-result loop, so
                // provider tool_use/tool_result adjacency stays intact.
                Ok(ToolOutput::WithAttachments { text, attachments }) => {
                    extend_unique_attachments(turn_attachments, attachments);
                    text.clone()
                }
                Ok(ToolOutput::MultiModalText { text, llm_images }) => {
                    push_bounded_images(&mut llm_visible_images, llm_images.iter().cloned());
                    text.clone()
                }
                Ok(ToolOutput::Error(msg)) => format!("Error: {msg}"),
                Err(e) => {
                    if let Some(denied) = e.downcast_ref::<baybo_tools::ToolError>()
                        && matches!(denied, baybo_tools::ToolError::Denied { .. })
                    {
                        format!(
                            "The user explicitly denied permission for tool '{}'. \
                             Do NOT retry this tool call. Either use an alternative \
                             approach or inform the user that the operation was skipped.",
                            tool_call.name
                        )
                    } else {
                        format!("Error: {e}")
                    }
                }
            };

            // Cap size before wrapping so the truncation notice lands inside
            // the `<tool_output>` envelope. The scan/format split keeps the
            // injection detector in `baybo-security` while the cap + spill +
            // envelope framing live in `baybo-context`; the loop bridges the
            // two by feeding the scan's rule names into the wrapper.
            let capped = self.context_manager.cap_tool_output(raw_result_text).await;
            let warnings = self.security_gateway.detect_injection(&capped);
            let warning_rules: Vec<&str> = warnings.iter().map(|w| w.rule_name.as_str()).collect();
            let wrapped = baybo_model::wrap_tool_output(&tool_call.name, &capped, &warning_rules);

            // Append tool result to context with the tool_use_id so the
            // LLM can correlate results with their originating calls. The
            // meta rides the persisted row but is never sent to the LLM: a
            // `Read` stamps the fingerprint it recorded (so the
            // read-before-write tracker can be rebuilt on hydration; `get`
            // returns `None` for a failed/virtual read), and any call that
            // raised an approval prompt stamps the decision (so reloads can
            // label the work step). Both `None` collapses to a plain
            // `tool_result`.
            let read_fingerprint = (tool_call.name == baybo_tools::READ_TOOL_NAME)
                .then(|| {
                    tool_call
                        .arguments
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .and_then(|p| self.read_tracker.get(std::path::Path::new(p)))
                })
                .flatten();
            let meta = (read_fingerprint.is_some() || call_approval.is_some()).then_some(
                baybo_model::ToolResultMeta {
                    read_fingerprint,
                    approval: call_approval,
                },
            );
            let tool_msg = ChatMessage::tool_result_with_meta(tool_call.id.clone(), wrapped, meta);
            self.context_manager.append(&tool_msg).await;
        }

        // Images a tool returned for the model (`MultiModalText`, e.g. a
        // browser screenshot) ride ONE follow-up user-role row appended
        // after every tool_result, so the next request's vision path
        // (`user_content_for_block`) picks them up without breaking the
        // provider's tool_use/tool_result adjacency validation.
        // `agent_context` = user role / agent source, so the row is never
        // mistaken for a genuine user prompt.
        if !llm_visible_images.is_empty() {
            let mut content: Vec<ContentBlock> = Vec::with_capacity(llm_visible_images.len() + 1);
            content.push(ContentBlock::Text(
                "[image attachment(s) returned by the tool call(s) above]".to_string(),
            ));
            content.append(&mut llm_visible_images);
            self.context_manager
                .append(&ChatMessage::agent_context(content))
                .await;
        }

        // Flush accumulated approvals back into session state.
        session.state.approved_resources = approved.lock().clone();

        let task_mutated = response
            .tool_calls
            .iter()
            .any(|tc| baybo_model::TASK_MUTATING_TOOL_NAMES.contains(&tc.name.as_str()));
        Ok(IterationOutcome::Continue { task_mutated })
    }

    /// Call the LLM with retry on transient errors using `ErrorHandler`.
    /// Returns the response paired with the `SpanId` of the
    /// **last attempt's** `LlmCall` span — that's the span tools spawned
    /// from this response should pair back to via `ToolCallOrigin`.
    async fn call_llm_with_retry(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<(LlmResponse, baybo_model::SpanId)> {
        let mut attempt = 0u32;
        loop {
            match self
                .call_llm(session, span_recorder, step, delta_tx, cancel_token)
                .await
            {
                Ok(pair) => return Ok(pair),
                Err(e) => {
                    // A `/stop` mid-call aborts the provider request inside
                    // `call_llm`; never retry it — the turn is unwinding, and
                    // a cancellation can otherwise read as a transient error.
                    // A cancellation is not a failure, so it never logs a
                    // give-up line.
                    if cancel_token.is_cancelled() {
                        return Err(e);
                    }
                    if !self.error_handler.should_retry(attempt, &e) {
                        // Terminal after we already retried at least once:
                        // record the attempts consumed so the upstream ERROR
                        // carries the count it otherwise lacks. A first-attempt
                        // non-retriable error already surfaces upstream as-is.
                        if attempt > 0 {
                            warn!(
                                attempts = attempt,
                                error = %e,
                                "giving up on LLM call after retries"
                            );
                        }
                        return Err(e);
                    }
                    let backoff = self.error_handler.backoff_duration(attempt);
                    // One ongoing retry stall re-fires this line every attempt;
                    // sample it (attempts 0/3/6/9) at warn and keep the rest at
                    // debug, so a multi-minute backoff stays visible at info
                    // without flooding it with near-identical lines.
                    if attempt == 0 || attempt.is_multiple_of(3) {
                        warn!(
                            attempt = attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            error = %e,
                            "retrying LLM call after transient error"
                        );
                    } else {
                        debug!(
                            attempt = attempt,
                            backoff_ms = backoff.as_millis() as u64,
                            error = %e,
                            "retrying LLM call after transient error"
                        );
                    }
                    // Honour a cancel that arrives during backoff too, so
                    // `/stop` isn't stalled waiting out the sleep.
                    tokio::select! {
                        biased;
                        _ = cancel_token.cancelled() => return Err(e),
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// Call the LLM with the current session context. Opens an
    /// `LlmCall` span inside `step`, fills it with the response (or
    /// failure) before closing. Cost recording happens automatically:
    /// `SpanRecorder::end_span(LlmCall { tokens })` publishes
    /// `TraceEvent::LlmSpanEnded` for the cost subscriber.
    async fn call_llm(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        cancel_token: &CancellationToken,
    ) -> anyhow::Result<(LlmResponse, baybo_model::SpanId)> {
        let model_info = self.llm_client.model_info();

        // Filtered by the session's channel (owner-only deck tools) and its
        // trigger (`report_nothing`, visible only in a recurring cron fire).
        // Both are session-stable, so the list stays byte-identical across this
        // session's calls and prompt caching holds.
        let tool_defs: Vec<ToolDefinitionForLlm> = self
            .tool_registry
            .tool_definitions_for_session(&session.channel, &session.trigger)
            .into_iter()
            .map(|td| ToolDefinitionForLlm {
                name: td.name,
                description: td.description,
                parameters_schema: td.parameters_schema,
            })
            .collect();

        // Coalesce adjacent same-role user/assistant messages *only* on the
        // wire to the LLM. Skill reminders are stored as standalone
        // `Role::User` entries, which would otherwise produce back-to-back
        // user messages — accepted by Anthropic / OpenAI but rejected by
        // providers that require strict user/assistant alternation.
        //
        // The trace span (and therefore the web UI / session log) keeps the
        // original unmerged transcript so each logical entry — reminder,
        // user prompt, assistant turn — stays separately inspectable.
        // Merging is a transport concern, not a storage one.
        let request = ChatRequest {
            messages: self.context_manager.messages_for_llm(),
            temperature: None,
            tools: tool_defs,
            reasoning_effort: self.initial_effort.clone(),
        };

        let input_messages = self.context_manager.build_call_input_marker().await;

        let cancel = cancel_token.clone();
        crate::runtime::scope::with_llm_span(
            span_recorder.as_ref(),
            step,
            step.turn_id,
            LlmCallBegin {
                model_id: model_info.id.clone(),
                provider: model_info.provider.clone(),
                provider_config_hash: String::new(),
                input_messages,
                temperature: None,
            },
            Some((cancel_token, baybo_turn::CancelReason::ParentCancelled)),
            |span| async move {
                // Bind this call to its `LlmCall` span so the spend lands
                // on the right span. `BoundBilledLlm` does gate → call →
                // record internally — no manual `record_call` afterward.
                let bound = self.llm_client.bind(Attribution {
                    user_id: session.user.id.clone(),
                    session_id: session.id.clone(),
                    turn_id: step.turn_id,
                    span_id: span.span_id,
                    reason: baybo_llm::CallReason::Chat,
                });
                // Run the provider call. The streaming path is cancel-aware
                // internally — it stops consuming and returns whatever it
                // streamed so far. The atomic non-streaming call is raced
                // against the token so a `/stop` (or the idle reaper) aborts
                // the in-flight request by dropping it (a streaming
                // `RecordingStream` still bills its partial usage on drop).
                let (partial_usage, llm_result): (TokenUsage, baybo_llm::Result<LlmResponse>) =
                    match delta_tx {
                        Some(tx) => self.chat_streaming(&bound, &request, session, tx, &cancel).await,
                        None => tokio::select! {
                            biased;
                            _ = cancel.cancelled() => (TokenUsage::default(), Ok(empty_llm_response())),
                            res = bound.chat(&request) => match res {
                                Ok(billed) => (billed.response.usage, Ok(billed.response)),
                                Err(e) => (TokenUsage::default(), Err(e)),
                            },
                        },
                    };

                // Cancelled while the call was in flight: salvage any partial
                // assistant content the stream produced and hand it up via
                // `CancelledTurn` so `run_iteration` persists it (the work
                // block then survives a reload). The span closes `Cancelled`
                // via the cancel context, and the retry loop won't re-issue.
                if cancel.is_cancelled() {
                    // Record the partial output the model produced before the
                    // abort onto the span too — otherwise the trace detail
                    // shows a blank LLM call even though text/thinking were
                    // generated. The streamed text was sanitized per-fragment
                    // but `thinking` accumulates raw, so run the same defensive
                    // scrub the success path applies before either the salvaged
                    // transcript copy or the trace copy escapes.
                    let (output_content, thinking, trace_tool_calls, partial) = match llm_result {
                        Ok(mut resp) => {
                            if let Err(e) =
                                self.security_gateway.sanitize_llm_response(&mut resp).await
                            {
                                warn!(error = %e, "failed to sanitize cancelled LLM response");
                            }
                            let trace_tool_calls = resp
                                .tool_calls
                                .iter()
                                .map(|tc| baybo_trace::LlmToolCallRecord {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                })
                                .collect();
                            let partial = salvage_partial_blocks(&resp);
                            (resp.content, resp.thinking, trace_tool_calls, partial)
                        }
                        Err(_) => (String::new(), None, Vec::new(), Vec::new()),
                    };
                    let finalize = LlmCallResult {
                        output_content,
                        thinking,
                        tool_calls: trace_tool_calls,
                        input_tokens: partial_usage.input_tokens,
                        output_tokens: partial_usage.output_tokens,
                        cached_input_tokens: partial_usage.cached_input_tokens,
                        cache_creation_input_tokens: partial_usage.cache_creation_input_tokens,
                    };
                    return (finalize, Err(anyhow::Error::new(CancelledTurn { partial })));
                }
                let (finalize, value_result) = match llm_result {
                    Ok(mut response) => {
                        // Defensive scrub of LLM output.
                        if let Err(e) = self
                            .security_gateway
                            .sanitize_llm_response(&mut response)
                            .await
                        {
                            warn!(error = %e, "failed to sanitize LLM response");
                        }
                        // Strip leading/trailing whitespace from text output
                        // before persisting. Some providers preface their
                        // first text block with stray newlines (especially
                        // after a thinking section or right after a tool
                        // call) which renders as a tall blank gap above the
                        // assistant message in the web UI. Interior
                        // whitespace is left intact so markdown paragraphs,
                        // code blocks, and lists are unaffected.
                        trim_response_text_edges(&mut response);
                        let trace_tool_calls: Vec<baybo_trace::LlmToolCallRecord> = response
                            .tool_calls
                            .iter()
                            .map(|tc| baybo_trace::LlmToolCallRecord {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments.clone(),
                            })
                            .collect();
                        let finalize = LlmCallResult {
                            output_content: response.content.clone(),
                            thinking: response.thinking.clone(),
                            tool_calls: trace_tool_calls,
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                            cached_input_tokens: response.usage.cached_input_tokens,
                            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                        };
                        (finalize, Ok((response, span.span_id)))
                    }
                    Err(e) => {
                        // Bill the partial-stream tokens so a failed
                        // call still leaves a `cost_records` row.
                        let finalize = LlmCallResult {
                            output_content: String::new(),
                            thinking: None,
                            tool_calls: Vec::new(),
                            input_tokens: partial_usage.input_tokens,
                            output_tokens: partial_usage.output_tokens,
                            cached_input_tokens: partial_usage.cached_input_tokens,
                            cache_creation_input_tokens: partial_usage.cache_creation_input_tokens,
                        };
                        (finalize, Err(anyhow::Error::new(e)))
                    }
                };

                // Cost was already recorded inside the bound call
                // (`BoundBilledLlm`) — synchronously bumping the budget
                // accumulator before the next iteration's `check()`, with
                // disk persistence fire-and-forget. Streaming bills the
                // last-seen usage on stream end/drop; a non-streaming
                // provider error records nothing (no usage to bill).

                // `record_call_actual` self-skips on zero. The
                // assistant message is appended later by
                // `append_context_message`, so the transcript at
                // this point still matches what the provider billed.
                self.context_manager
                    .record_call_actual(finalize.input_tokens);

                (finalize, value_result)
            },
        )
        .await
    }

    /// Drain mid-turn user interjections from `src` and append each as a
    /// faithful `UserInterjection` transcript row (persisted like any user
    /// message). The steering envelope is applied wire-only later by
    /// `ContextManager::messages_for_llm`; here we store the raw text so the
    /// chat surface shows a clean user bubble and the row survives turn
    /// cancellation. A `None` source (cron / subagent / notification turns) is a
    /// no-op — this is a UserChat-turn affordance.
    /// See `docs/mid-turn-user-interjection.md`.
    async fn drain_user_interjections(
        &mut self,
        src: &mut Option<&mut dyn InterjectionSource>,
    ) -> Vec<Vec<ContentBlock>> {
        let Some(src) = src.as_deref_mut() else {
            return Vec::new();
        };
        let drained = src.drain_injectable();
        if drained.is_empty() {
            return Vec::new();
        }
        let count = drained.len();
        let mut content = Vec::with_capacity(count);
        for input in drained {
            // Budgeted at the framed wire size; see `append_user_interjection`.
            self.context_manager
                .append_user_interjection_with_platform_msg_id(
                    input.content.clone(),
                    input.platform_msg_id,
                )
                .await;
            content.push(input.content);
        }
        info!(
            interjections = count,
            "injected mid-turn user interjection(s) before the next LLM call"
        );
        content
    }

    /// Recall memories relevant to `query` and inject each as a framed
    /// [`baybo_model::MessageSource::RecalledMemory`] row before the next LLM
    /// call. No-op when no memory is wired. Recall failure is logged and
    /// swallowed — it must never fail the turn. The impl bills its own
    /// embedding/LLM work against the minted [`Attribution`]; a `MemoryRecall`
    /// trace step marks the operation.
    async fn recall_and_inject(
        &mut self,
        query: &[ContentBlock],
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        cancel_token: &CancellationToken,
    ) {
        // A prompt-less trigger (e.g. a tool-only cron fire whose payload has no
        // `prompt`) yields an empty query — skip recall so a real backend never
        // opens a `MemoryRecall` step or embeds the empty string.
        if query.is_empty() {
            return;
        }
        // If cancellation already tripped before we even called the backend,
        // there's no turn to enrich — skip without opening a step or billing.
        if cancel_token.is_cancelled() {
            return;
        }
        let Some(memory) = self.memory.clone() else {
            return;
        };
        let user_id = session.user.id.clone();
        let session_id = session.id.clone();
        let agent_id = session.state.agent_id_or_builtin();
        let query = query.to_vec();
        let recorder = Arc::clone(span_recorder);
        let recalled = crate::runtime::scope::with_step(
            span_recorder.as_ref(),
            turn_id,
            StepKind::MemoryRecall,
            Some((cancel_token, baybo_turn::CancelReason::ParentCancelled)),
            move |step| async move {
                let ctx = MemoryContext::new(
                    MemoryScope {
                        user_id,
                        session_id,
                        turn_id,
                        agent_id,
                    },
                    recorder,
                    step,
                );
                match memory.recall(&ctx, &query).await {
                    Ok(mems) => Ok((LifecycleOutcome::Ok, mems)),
                    Err(e) => Err(anyhow::Error::new(e)),
                }
            },
        )
        .await;
        let recalled = match recalled {
            Ok(mems) => mems,
            Err(e) => {
                warn!(error = %e, "memory recall failed; continuing without recalled context");
                return;
            }
        };
        // Re-check: cancellation may have tripped while `memory.recall` was
        // in flight. Don't persist recalled rows for a turn we're about to
        // abort — the next iteration-boundary cancel check would return Err
        // immediately and leave dangling memory rows on the transcript.
        if cancel_token.is_cancelled() {
            return;
        }
        // Belt-and-braces dedup: the `Memory` trait says per-session
        // de-duplication is the impl's turn, but mem0 / openviking don't do
        // it today (a recall on the same query a few turns later returns
        // the same `memory` strings). Filter against the live transcript's
        // existing `RecalledMemory` rows so we don't accumulate identical
        // `<recalled_memory>` blocks. Survives actor reap for free — the
        // transcript reloads from the store on rehydration.
        let already_in_transcript: std::collections::HashSet<String> = self
            .context_manager
            .messages()
            .iter()
            .filter(|m| m.source() == MessageSource::RecalledMemory)
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
            })
            .collect();
        let mut seen_this_recall: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for mem in recalled {
            if already_in_transcript.contains(&mem.content) {
                continue;
            }
            // A single recall can also return the same string twice (the
            // backend doesn't promise uniqueness in its result vec).
            if !seen_this_recall.insert(mem.content.clone()) {
                continue;
            }
            self.context_manager
                .append_recalled_memory(vec![ContentBlock::Text(mem.content)])
                .await;
        }
    }

    /// Fire-and-forget the [`Memory::on_session_end`] consolidation write at
    /// actor shutdown. Detached on the tokio runtime root (not bound to the
    /// actor's cancellation token, which the caller cancels immediately after
    /// this returns), so the write survives the actor's teardown.
    ///
    /// Pulls the FULL durable transcript via [`SessionManager::history`] —
    /// the actor's in-memory view may have been compressed, and
    /// `on_session_end`'s contract is to see raw turns.
    ///
    /// No-op when memory is unwired, when `sessions` is unwired (test
    /// harnesses with no cross-session store), or when the session isn't
    /// user-facing per [`should_fire_session_end`] (subagents and
    /// system-triggered actors all send `ActorStop` too, but their shutdown
    /// is not a user-session ending).
    ///
    /// [`SessionManager::history`]: crate::SessionManager::history
    pub fn spawn_session_end_write(&self, span_recorder: &Arc<SpanRecorder>, session: &Session) {
        let Some(memory) = self.memory.clone() else {
            return;
        };
        let Some(sessions) = self.sessions.clone() else {
            return;
        };
        if !should_fire_session_end(session) {
            return;
        }
        let user_id = session.user.id.clone();
        let session_id = session.id.clone();
        let agent_id = session.state.agent_id_or_builtin();
        let recorder = Arc::clone(span_recorder);
        tokio::spawn(async move {
            // `full_transcript` (not `history`) so the impl sees the raw
            // turns per `on_session_end`'s contract — `history` filters out
            // rows marked `superseded_by`, so a session that has been
            // compressed would otherwise lose all the user / assistant turns
            // that the compaction folded into a summary.
            let transcript = match sessions.full_transcript(&session_id).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        error = %e,
                        session_id = %session_id,
                        "memory on_session_end: failed to load durable transcript; skipping write",
                    );
                    return;
                }
            };
            if transcript.is_empty() {
                return;
            }
            // Synthetic TurnId — `on_session_end` isn't tied to a user turn; this
            // id only exists so the trace step + any billed sub-call records
            // share one key. Mirrors how compression mints its own ids for
            // maintenance work.
            let turn_id = TurnId::new();
            let ctx_recorder = Arc::clone(&recorder);
            let result = crate::runtime::scope::with_step(
                recorder.as_ref(),
                turn_id,
                StepKind::MemoryWrite,
                None,
                move |step| async move {
                    let ctx = MemoryContext::new(
                        MemoryScope {
                            user_id,
                            session_id,
                            turn_id,
                            agent_id,
                        },
                        ctx_recorder,
                        step,
                    );
                    match memory.on_session_end(&ctx, &transcript).await {
                        Ok(()) => Ok((LifecycleOutcome::Ok, ())),
                        Err(e) => Err(anyhow::Error::new(e)),
                    }
                },
            )
            .await;
            if let Err(e) = result {
                warn!(error = %e, "memory on_session_end write failed");
            }
        });
    }

    /// Fire-and-forget the [`Memory::on_turn_complete`] write for a finished
    /// exchange. Detached so the actor returns the answer without waiting on
    /// the memory write; the impl bills its work against the minted
    /// [`Attribution`] under a `MemoryWrite` trace step. No-op when no memory
    /// is wired (`memory == None`).
    ///
    /// Free-standing (no `&self`) and takes owned `user_id` / `session_id`
    /// so `run()` can call it AFTER `with_turn` returns — the closure that
    /// drives the iteration loop moves `&mut self` + `&mut session` into
    /// `with_turn`'s body, so the borrow checker won't let us touch either
    /// from `run()` afterwards. Pre-extract `self.memory.clone()` and the
    /// two ids before the closure, then call this with the owned values.
    fn spawn_turn_complete_write(
        memory: Option<Arc<dyn Memory>>,
        scope: MemoryScope,
        span_recorder: &Arc<SpanRecorder>,
        user_input: Vec<ContentBlock>,
        final_output: Vec<ContentBlock>,
    ) {
        let Some(memory) = memory else {
            return;
        };
        let recorder = Arc::clone(span_recorder);
        tokio::spawn(async move {
            let ctx_recorder = Arc::clone(&recorder);
            let turn_id = scope.turn_id;
            let result = crate::runtime::scope::with_step(
                recorder.as_ref(),
                turn_id,
                StepKind::MemoryWrite,
                None,
                move |step| async move {
                    let ctx = MemoryContext::new(scope, ctx_recorder, step);
                    match memory
                        .on_turn_complete(&ctx, &user_input, &final_output)
                        .await
                    {
                        Ok(()) => Ok((LifecycleOutcome::Ok, ())),
                        Err(e) => Err(anyhow::Error::new(e)),
                    }
                },
            )
            .await;
            if let Err(e) = result {
                warn!(error = %e, "memory on_turn_complete write failed");
            }
        });
    }

    /// Append a user-authored message to this session's transcript — both the
    /// in-memory context and the persisted `session_messages` log — ahead
    /// of the turn that runs next. Lets the actor coalesce a burst of user
    /// messages into one turn while keeping each as its own transcript row;
    /// context's `messages_for_llm` collapses the consecutive rows for the
    /// provider call.
    pub async fn append_user_message(&mut self, content: Vec<ContentBlock>) -> anyhow::Result<()> {
        self.append_user_message_with_platform_msg_id(content, String::new())
            .await
    }

    pub async fn append_user_message_with_platform_msg_id(
        &mut self,
        content: Vec<ContentBlock>,
        platform_msg_id: impl Into<String>,
    ) -> anyhow::Result<()> {
        // A coalesced burst can be the first thing a fresh session ever
        // appends; seed the system prompt first so it never lands *after*
        // user content. `ensure_seeded` keys off `messages[0]`, so a leading
        // user row would otherwise make every later turn re-seed.
        self.context_manager.ensure_seeded().await;
        let msg = ChatMessage::user(content).with_platform_msg_id(platform_msg_id);
        self.context_manager.append(&msg).await;
        Ok(())
    }

    /// Append a cron fire's framed prompt as a persisted `Cron`-source row
    /// ahead of the turn. The framing ([`baybo_context::prompts::cron`]) makes
    /// the model treat the fire as a task to perform now rather than a live
    /// user message; `MessageSource::Cron` lets the operator inbox find the
    /// row. Seeds the system prompt first so a fresh cron session never lands
    /// the fire ahead of `messages[0]`.
    pub async fn append_cron_fire(
        &mut self,
        turn_id: &str,
        prompt: &str,
        context: Option<&str>,
    ) -> anyhow::Result<()> {
        self.context_manager.ensure_seeded().await;
        let framed =
            baybo_context::prompts::cron::frame_cron_prompt_with_context(turn_id, prompt, context);
        let msg = ChatMessage::cron_fire(vec![ContentBlock::Text(framed)]);
        self.context_manager.append(&msg).await;
        Ok(())
    }

    /// Append a one-shot cron fire's result to **this** (the scheduling)
    /// conversation as a persisted `Role::Assistant` row. A one-shot origin
    /// delivery supplies `source_event_id` so a crash replay returns the
    /// existing row instead of appending another copy; recurring fires pass
    /// `None` because each fire owns a fresh conversation.
    ///
    /// No inference runs: `content` is the fire's own reply, already framed
    /// with a scheduled-task header ([`baybo_context::prompts::cron`]). The
    /// row is stamped `MessageSource::CronNotification`, so chat surfaces can
    /// badge it and the next real turn reads it back as something the
    /// assistant already reported. Seeds the system prompt first — the
    /// notification can be the first thing an otherwise-cold session appends.
    ///
    /// `None` when the session runs with no durable store (tests); the caller
    /// then has no ordinal to push or to record on the turn.
    pub async fn append_cron_notification(
        &mut self,
        content: Vec<ContentBlock>,
        source_event_id: Option<&str>,
    ) -> Option<baybo_session::SessionMessageAppendOutcome> {
        self.context_manager.ensure_seeded().await;
        let message = ChatMessage::cron_notification(content);
        match source_event_id {
            Some(source_event_id) => {
                self.context_manager
                    .append_idempotent(source_event_id, &message)
                    .await
            }
            None => self
                .context_manager
                .append(&message)
                .await
                .map(|ordinal| baybo_session::SessionMessageAppendOutcome::Inserted { ordinal }),
        }
    }

    /// Append a subagent-spawned session's initial prompt as a persisted
    /// agent-context row ahead of the turn (hidden from chat surfaces).
    pub async fn append_spawned_prompt(
        &mut self,
        content: Vec<ContentBlock>,
    ) -> anyhow::Result<()> {
        self.context_manager.ensure_seeded().await;
        let msg = ChatMessage::agent_context(content);
        self.context_manager.append(&msg).await;
        Ok(())
    }

    /// Append the public, non-LLM completion reply for a background batch.
    /// The stable source-event id keeps crash replay from creating a second
    /// assistant bubble.
    pub async fn append_background_completion_reply_once(
        &mut self,
        content: Vec<ContentBlock>,
        source_event_id: &str,
    ) -> Option<baybo_session::SessionMessageAppendOutcome> {
        self.context_manager.ensure_seeded().await;
        self.context_manager
            .append_idempotent(source_event_id, &ChatMessage::assistant(content))
            .await
    }

    /// Append the crash-replayable hidden prompt that carries a background
    /// batch into its analysis turn. Existing rows are returned without
    /// duplicating the live context window.
    pub async fn append_background_notification_prompt_once(
        &mut self,
        content: Vec<ContentBlock>,
        source_event_id: &str,
    ) -> Option<baybo_session::SessionMessageAppendOutcome> {
        self.context_manager.ensure_seeded().await;
        self.context_manager
            .append_idempotent(source_event_id, &ChatMessage::agent_context(content))
            .await
    }

    /// Arm or clear the request-time background-notification retry cue. Applied
    /// only while the transcript tail is an assistant row; never persisted. See
    /// [`baybo_context::ContextManager::set_notification_cue`].
    pub fn set_notification_cue(&mut self, armed: bool) {
        self.context_manager.set_notification_cue(armed);
    }

    /// Run a streaming chat request, forwarding each text chunk through
    /// `delta_tx` while accumulating the full response to return.
    ///
    /// The delta stream is the single place where real plaintext may
    /// legitimately leave the agent: each chunk is scanned for leaks
    /// (tokenized into the placeholder form that the accumulated
    /// `content` remembers) and then revealed via the vault just before
    /// being sent to the adapter. The returned `LlmResponse.content`
    /// remains in placeholder form so trace / memory / next-turn context
    /// never see real secrets.
    ///
    /// Returns `(TokenUsage, Result)` so partial usage seen before a
    /// stream error still bills. Providers that only emit usage in
    /// the terminal `Final` event (today: OpenAI / Anthropic via rig)
    /// yield `TokenUsage::default()` on a mid-stream drop — the row
    /// still lands in `cost_records` with zero counts so operators
    /// see the failed call instead of silent under-billing.
    ///
    /// Cancel-aware: a `/stop` (or idle reaper) tripping `cancel` stops
    /// consuming and returns whatever was streamed so far as `Ok(partial)`
    /// (the caller salvages it). Dropping the stream aborts the in-flight
    /// HTTP request and bills the partial usage via `RecordingStream::drop`.
    async fn chat_streaming(
        &self,
        bound: &BoundBilledLlm,
        request: &ChatRequest,
        session: &Session,
        delta_tx: &mpsc::Sender<AgentOutput>,
        cancel: &CancellationToken,
    ) -> (TokenUsage, baybo_llm::Result<LlmResponse>) {
        let mut stream = match tokio::select! {
            biased;
            _ = cancel.cancelled() => return (TokenUsage::default(), Ok(empty_llm_response())),
            opened = bound.chat_stream(request) => opened,
        } {
            Ok(s) => s,
            Err(e) => return (TokenUsage::default(), Err(e)),
        };
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut usage = TokenUsage::default();
        let mut thinking = String::new();
        let mut thinking_blocks: Vec<ContentBlock> = Vec::new();

        // Buffer for a trailing fragment that might be the start of a
        // placeholder (e.g. the chunk ends in "[{REDACTED_S"). We hold
        // it back until a safe boundary is seen.
        let mut pending = String::new();

        // Some providers preface their text with stray newlines (often
        // right after a thinking section or tool call). Drop them on the
        // wire so the user doesn't watch the message render with a tall
        // blank gap above. Once we've seen any non-whitespace char the
        // flag flips and subsequent chunks pass through verbatim — interior
        // formatting is preserved.
        let mut leading_stripped = false;

        // Separate buffer for reasoning ("thinking") fragments — same
        // placeholder-safe flush discipline as the answer text, but
        // streamed as ephemeral `Reasoning` rather than answer `AnswerDelta`.
        let mut pending_reasoning = String::new();

        loop {
            // Stop consuming the moment the turn is cancelled, falling through
            // to flush + return the partial. The stream drops on function exit,
            // aborting the in-flight request and billing the partial usage.
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                ev = stream.next() => ev,
            };
            let Some(event) = next else { break };
            let event = match event {
                Ok(e) => e,
                Err(e) => return (usage, Err(e)),
            };
            match event {
                StreamEvent::Text(chunk) => {
                    pending.push_str(&chunk);

                    let flush_to = safe_flush_boundary(&pending);
                    if flush_to > 0 {
                        let mut flushable: String = pending.drain(..flush_to).collect();
                        if !skip_leading_whitespace(&mut flushable, &mut leading_stripped) {
                            continue;
                        }
                        self.stream_emit(&flushable, session, &mut content, delta_tx)
                            .await;
                    }
                }
                StreamEvent::ToolCall(info) => tool_calls.push(info),
                StreamEvent::Reasoning(r) => {
                    // Raw accumulates into `thinking` for the response (sanitized
                    // wholesale at finalize); a parallel buffer streams it to the
                    // channel through the same leak boundary the answer text uses.
                    thinking.push_str(&r);
                    pending_reasoning.push_str(&r);
                    let flush_to = safe_flush_boundary(&pending_reasoning);
                    if flush_to > 0 {
                        let flushable: String = pending_reasoning.drain(..flush_to).collect();
                        self.stream_emit_reasoning(&flushable, session, delta_tx)
                            .await;
                    }
                }
                StreamEvent::ThinkingBlock(block) => thinking_blocks.push(block),
                StreamEvent::Usage(u) => usage = u,
            }
        }

        // Flush any remaining buffered text.
        if !pending.is_empty() {
            let mut flushable = std::mem::take(&mut pending);
            if skip_leading_whitespace(&mut flushable, &mut leading_stripped) {
                self.stream_emit(&flushable, session, &mut content, delta_tx)
                    .await;
            }
        }

        // Flush any remaining buffered reasoning.
        if !pending_reasoning.is_empty() {
            let flushable = std::mem::take(&mut pending_reasoning);
            self.stream_emit_reasoning(&flushable, session, delta_tx)
                .await;
        }

        // Build content_blocks: thinking blocks first (providers expect
        // them before text), then text.
        let mut content_blocks = thinking_blocks;

        // Providers that stream reasoning only as deltas (DeepSeek thinking
        // mode and other OpenAI-compatible endpoints) never emit a complete
        // thinking block, so synthesize one from the accumulated text. Without
        // it the reasoning is dropped from the persisted assistant turn and
        // can't be echoed back next request — which DeepSeek rejects with a
        // 400 "reasoning_content must be passed back".
        if content_blocks.is_empty() && !thinking.is_empty() {
            content_blocks.push(ContentBlock::Thinking {
                id: None,
                content: vec![ThinkingContent::Text {
                    text: thinking.clone(),
                    signature: None,
                }],
            });
        }

        if !content.is_empty() {
            content_blocks.push(ContentBlock::Text(content.clone()));
        }

        (
            usage,
            Ok(LlmResponse {
                content,
                content_blocks,
                tool_calls,
                usage,
                thinking: if thinking.is_empty() {
                    None
                } else {
                    Some(thinking)
                },
            }),
        )
    }

    /// Tokenize a single stream fragment:
    /// scan for leaks, mint placeholders, persist to vault, substitute. The
    /// placeholder-form is both appended to `content` (so the accumulated
    /// `LlmResponse` stays sanitized) and delivered as-is to the adapter, so
    /// the streaming view matches the final persisted message.
    async fn stream_emit(
        &self,
        fragment: &str,
        session: &Session,
        content: &mut String,
        delta_tx: &mpsc::Sender<AgentOutput>,
    ) {
        let sanitized = match self
            .security_gateway
            .sanitize_stream_fragment(fragment)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to sanitize stream fragment; dropping");
                return;
            }
        };

        content.push_str(&sanitized);

        if delta_tx
            .send(AgentOutput {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                event: AgentEvent::AnswerDelta(sanitized),
            })
            .await
            .is_err()
        {
            debug!("delta receiver dropped, continuing without forwarding");
        }
    }

    /// Stream a reasoning ("thinking") fragment to the channel as
    /// `AgentEvent::Reasoning`, through the same leak boundary as `AnswerDelta`.
    /// Reasoning is ephemeral progress, so it `try_send`s and drops on a
    /// full channel rather than backpressuring the LLM stream.
    async fn stream_emit_reasoning(
        &self,
        fragment: &str,
        session: &Session,
        delta_tx: &mpsc::Sender<AgentOutput>,
    ) {
        let sanitized = match self
            .security_gateway
            .sanitize_stream_fragment(fragment)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to sanitize reasoning fragment; dropping");
                return;
            }
        };
        let _ = delta_tx.try_send(AgentOutput {
            session_id: session.id.clone(),
            user_id: session.user.id.clone(),
            channel: session.channel.clone(),
            event: AgentEvent::Reasoning(sanitized),
        });
    }

    /// Emit a `ToolStarted` progress event before a tool call runs. The
    /// `label` (from `Tool::progress_label`, derived from LLM-written
    /// arguments) passes the leak boundary first. No-op when the turn
    /// isn't streaming (`delta_tx` is `None`: cron).
    async fn emit_tool_started(
        &self,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        session: &Session,
        call_id: String,
        tool: String,
        raw_label: Option<String>,
    ) {
        let Some(tx) = delta_tx else { return };
        let label = match raw_label {
            Some(l) => self
                .security_gateway
                .sanitize_stream_fragment(&l)
                .await
                .ok(),
            None => None,
        };
        let _ = tx
            .send(AgentOutput {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                event: AgentEvent::ToolStarted {
                    call_id,
                    tool,
                    label,
                },
            })
            .await;
    }

    /// Emit a `ToolCompleted` progress event after a tool call returns.
    /// `raw_summary` passes the leak boundary before leaving the agent; on
    /// a sanitize failure the summary is dropped (empty) rather than risk
    /// a leak. No-op when the turn isn't streaming.
    async fn emit_tool_completed(
        &self,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        session: &Session,
        call_id: String,
        status: ToolStatus,
        raw_summary: String,
        approval: Option<ApprovalDecision>,
    ) {
        let Some(tx) = delta_tx else { return };
        let summary = self
            .security_gateway
            .sanitize_stream_fragment(&raw_summary)
            .await
            .unwrap_or_default();
        let _ = tx
            .send(AgentOutput {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                event: AgentEvent::ToolCompleted {
                    call_id,
                    status,
                    summary,
                    approval,
                },
            })
            .await;
    }

    /// Emit a transient turn-[`AgentEvent::Status`] event (today:
    /// compaction start/end). No sanitization — the variant carries no
    /// free text. No-op when the turn isn't streaming (`delta_tx` is
    /// `None`: cron).
    async fn emit_status(
        &self,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        session: &Session,
        status: StatusPhase,
    ) {
        let Some(tx) = delta_tx else { return };
        let _ = tx
            .send(AgentOutput {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                event: AgentEvent::Status(status),
            })
            .await;
    }

    /// Compress if the budget calls for it. The `chat` closure brackets the
    /// summarizer call in a `Compression` step + `LlmCall` span and records
    /// cost against that span — budget enforcement on the call itself rides
    /// on the wrapped client.
    ///
    /// Reports the compaction phase as `Status(Compacting)` / `Compacted`
    /// when a pass actually runs, so the user sees why the turn paused.
    ///
    /// A failed compaction does **not** end the turn: the transcript is
    /// simply not shortened, and the next iteration (or the next turn) tries
    /// again. But the user is told, once per turn — the conversation is now
    /// running over its compaction threshold, which is worth knowing before
    /// the main call starts refusing oversized requests.
    /// `failure_reported` is that once-per-turn latch, owned by `run_inner`;
    /// it gates the `warn!` line as well, so a long turn does not repeat it.
    async fn compress_if_needed(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        cancel_token: &CancellationToken,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        failure_reported: &mut bool,
    ) -> anyhow::Result<()> {
        let runner = self.build_compression_runner(
            session,
            span_recorder,
            turn_id,
            cancel_token,
            CompressionTrigger::Threshold,
        );
        let model_id = runner.model_info.id.clone();
        // `needs_compression` mirrors `maybe_compress`'s gate, so we only
        // report the phase when a pass will actually run.
        let compacting = self.context_manager.needs_compression(&model_id);
        if compacting {
            self.emit_status(delta_tx, session, StatusPhase::Compacting)
                .await;
        }
        let result = self
            .context_manager
            .maybe_compress(&model_id, |req, marker| async move {
                runner.run(req, marker).await
            })
            .await;
        // The `Compacted` end always follows the `Compacting` start so the
        // status line never dangles — except on a cancel, where the compaction
        // was abandoned and the turn is unwinding behind it.
        if compacting && !cancel_token.is_cancelled() {
            self.emit_status(delta_tx, session, StatusPhase::Compacted)
                .await;
        }
        if let Ok(baybo_context::CompressionOutcome::Failed { reason }) = &result {
            if *failure_reported {
                debug!(
                    session_id = %session.id,
                    reason = %reason,
                    "context compaction failed again this turn"
                );
            } else {
                *failure_reported = true;
                warn!(
                    session_id = %session.id,
                    reason = %reason,
                    "context compaction failed; transcript left unchanged"
                );
                self.emit_compaction_failed(delta_tx, session, reason).await;
            }
        }
        result?;
        Ok(())
    }

    /// Tell the user a compaction failed, as a mid-turn `Warn` notice.
    ///
    /// Live-only (`durable_id: None`): `mid_turn` notices fold into the open
    /// work block, which is the right shape here — the turn keeps running —
    /// but that fold is not keyed by durable id, so persisting a twin would
    /// render twice after a sync. The trace's failed `Compression` step is
    /// the durable record.
    ///
    /// `reason` is already sanitized: [`CompressionRunner`] runs the provider
    /// error through the same `SecurityGateway` as the main LLM path.
    async fn emit_compaction_failed(
        &self,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        session: &Session,
        reason: &str,
    ) {
        let Some(tx) = delta_tx else { return };
        let _ = tx
            .send(AgentOutput {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                event: AgentEvent::Notice {
                    level: baybo_channels::NoticeLevel::Warn,
                    text: compaction_failed_text(reason),
                    mid_turn: true,
                    durable_id: None,
                },
            })
            .await;
    }

    fn build_compression_runner(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        cancel_token: &CancellationToken,
        trigger: CompressionTrigger,
    ) -> CompressionRunner {
        let model_info = self.llm_client.model_info().clone();
        CompressionRunner {
            llm_client: self.llm_client.clone(),
            recorder: Arc::clone(span_recorder),
            security_gateway: Arc::clone(&self.security_gateway),
            turn_id,
            user_id: session.user.id.clone(),
            session_id: session.id.clone(),
            model_info,
            cancel_token: cancel_token.clone(),
            trigger,
        }
    }

    /// Read-only out-of-band progress: summarize the in-flight turn with a
    /// billed LLM call and ship it as a `Notice`. Called only from the
    /// `Continue` arm — after an iteration resolved as a tool round, never on
    /// the final answer. The call runs detached so it never blocks the next
    /// iteration: each time we first DRAIN the previous call (emit its line if
    /// it finished), then SPAWN a fresh one when the gate
    /// (`should_fire_observer`) passes and none is already in flight. No-op
    /// unless the gate passes; throttled to one attempt per
    /// `OBSERVER_MIN_INTERVAL`. At most one call is in flight at a time.
    #[allow(clippy::too_many_arguments)]
    async fn maybe_run_progress_observer(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        cancel_token: &CancellationToken,
        // Bound to the spawned call (not the gate/drain checks) so it aborts on
        // turn end even when the turn itself succeeded.
        observer_cancel: &CancellationToken,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        iterations: usize,
        turn_started: std::time::Instant,
        observer_state: &mut ObserverState,
    ) {
        // Drain the previous detached call first. If it finished, emit its
        // line (the snapshot it summarized is from a prior boundary, but it's
        // still strictly older than any further iteration, so ordering holds).
        // If it's still running, put it back and skip spawning — at-most-one.
        if let Some(handle) = observer_state.in_flight.take() {
            if handle.is_finished() {
                match handle.await {
                    Ok(Ok(text)) if !text.trim().is_empty() => {
                        // Re-check cancel: the call may have raced a preempt/stop.
                        if !cancel_token.is_cancelled() {
                            // Record only what actually reaches the user, so the
                            // next tick dedupes against their real view.
                            observer_state.sent_notices.push(text.clone());
                            // Durably shadow the line as a `progress` control
                            // event so a reload reconstructs it into this turn's
                            // work block (the live frame below is ephemeral).
                            self.persist_progress_narration(session, &text).await;
                            self.emit_progress(delta_tx, session, text).await;
                        }
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => debug!(error = %e, "progress observer call failed; skipping"),
                    Err(e) => debug!(error = %e, "progress observer task join failed; skipping"),
                }
            } else {
                observer_state.in_flight = Some(handle);
                return;
            }
        }

        let now = std::time::Instant::now();
        if cancel_token.is_cancelled()
            || !should_fire_observer(
                delta_tx.is_some(),
                session.trigger.kind() == baybo_model::TriggerKind::User,
                channel_wants_progress(&session.channel),
                iterations,
                turn_started,
                observer_state.last_fired_at,
                now,
            )
        {
            return;
        }

        // Reuse the main call's prefix (cache hit) + a summarize turn that
        // also carries the lines already shown this turn so the model
        // advances instead of repeating. The prior lines + instruction are
        // the appended suffix — `messages_for_llm()` (the cached prefix) is
        // untouched. No tools, so the observer only narrates. Clone the
        // snapshot NOW, synchronously at the boundary, so the detached task
        // owns a coherent frozen copy and never reads live context.
        let mut messages = self.context_manager.messages_for_llm();
        let prompt_msg = ChatMessage::user(vec![ContentBlock::Text(build_observer_prompt(
            &observer_state.sent_notices,
        ))]);
        messages.push(prompt_msg.clone());
        // The cached prefix (`messages_for_llm`) is referenced by ordinal
        // in the span; only the observer prompt rides inline as the
        // suffix. Computed synchronously at the boundary so the detached
        // task owns a coherent marker and never reads live context.
        let input_marker = self
            .context_manager
            .input_marker_with_suffix(vec![prompt_msg])
            .await;
        let request = ChatRequest {
            messages,
            temperature: None,
            tools: Vec::new(),
            reasoning_effort: self.initial_effort.clone(),
        };

        // Throttle on attempt, not just success — a failing or empty call
        // must not re-fire every iteration boundary.
        observer_state.last_fired_at = Some(now);

        // Build the runner from `&self` first; it owns / Arc-clones every
        // field, so the spawned future is `'static + Send` and borrows nothing
        // from `self`. Detached, never `abort()`ed (that would leak a Pending
        // step) — the runner `select!`s on `observer_cancel` instead, closing
        // as Cancelled when the loop trips it on turn end.
        let runner =
            self.build_progress_observer_runner(session, span_recorder, turn_id, observer_cancel);
        observer_state.in_flight = Some(tokio::spawn(async move {
            runner.run(request, input_marker).await
        }));
    }

    fn build_progress_observer_runner(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        turn_id: TurnId,
        cancel_token: &CancellationToken,
    ) -> ProgressObserverRunner {
        ProgressObserverRunner {
            llm_client: self.llm_client.clone(),
            recorder: Arc::clone(span_recorder),
            security_gateway: Arc::clone(&self.security_gateway),
            turn_id,
            user_id: session.user.id.clone(),
            session_id: session.id.clone(),
            model_info: self.llm_client.model_info().clone(),
            cancel_token: cancel_token.clone(),
        }
    }

    /// Emit the observer's one-line summary as a transient
    /// [`AgentEvent::Progress`] — non-terminal, so a work-block client
    /// keeps the turn open around it. The text is already scrubbed by the
    /// runner (`sanitize_llm_response`), so this only routes it. No-op when
    /// the turn isn't streaming.
    async fn emit_progress(
        &self,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        session: &Session,
        text: String,
    ) {
        let Some(tx) = delta_tx else { return };
        let _ = tx
            .send(AgentOutput {
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
                event: AgentEvent::Progress(text),
            })
            .await;
    }

    /// Persist one progress line as a `progress` control event, anchored after
    /// the session's newest ordinal so the reconstruction folds it into the
    /// in-flight turn's work block at the right spot. Best-effort: a missing
    /// session store (test loops) or a write error just skips the durable
    /// shadow — the live frame already reached the user.
    async fn persist_progress_narration(&self, session: &Session, text: &str) {
        let Some(sessions) = self.sessions.as_ref() else {
            return;
        };
        let after_ordinal = match sessions.latest_session_ordinal(&session.id).await {
            Ok(max) => max.unwrap_or(-1),
            Err(e) => {
                debug!(session_id = %session.id, error = %e, "progress: ordinal lookup failed; skipping persist");
                return;
            }
        };
        if let Err(e) = sessions
            .append_control_event(
                &session.id,
                after_ordinal,
                ControlEventKind::Progress,
                text,
                chrono::Utc::now(),
                "",
            )
            .await
        {
            debug!(session_id = %session.id, error = %e, "failed to persist progress narration");
        }
    }

    /// Run an on-demand compression pass and return the notice for the caller
    /// to ship as an `AgentEvent::Notice`.
    /// The variants of `CompressionOutcome` map to specific user-facing
    /// messages so the caller (typically a `/compact` notice) can
    /// distinguish "strategy declined", "no savings", a failed summarizer
    /// call, and a real compress — instead of one generic "nothing to
    /// compress" line.
    /// A fresh turn is minted so the compression step + LLM span land
    /// on a real lifecycle.
    pub async fn compact_now(
        &mut self,
        session: &mut Session,
        turn_lifecycle: &Arc<TurnLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        parent_turn_id: Option<TurnId>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<CompactionNotice> {
        // `/compact` is a user-typed command, but it is not a chat turn: it
        // runs `force_compress` directly and emits a notice, not an assistant
        // reply.
        let turn_input = TurnInput::Compact;
        let spec = TurnSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            input: turn_input,
            parent_turn_id,
        };

        crate::runtime::scope::with_turn(
            turn_lifecycle,
            cancel_token.clone(),
            spec,
            |turn_id| async move {
                let runner = self.build_compression_runner(
                    session,
                    span_recorder,
                    turn_id,
                    &cancel_token,
                    CompressionTrigger::Forced,
                );
                let model_id = runner.model_info.id.clone();
                let outcome = self
                    .context_manager
                    .force_compress(&model_id, |req, marker| async move {
                        runner.run(req, marker).await
                    })
                    .await?;
                let notice = match outcome {
                    baybo_context::CompressionOutcome::Compressed => CompactionNotice::info(
                        "Context compressed.",
                    ),
                    baybo_context::CompressionOutcome::BelowThreshold => CompactionNotice::info(
                        "Context already under the compression threshold; skipped.",
                    ),
                    baybo_context::CompressionOutcome::StrategyDeclined => CompactionNotice::info(
                        "Compression strategy declined: nothing to summarize (conversation too short).",
                    ),
                    baybo_context::CompressionOutcome::NoSavings => CompactionNotice::info(
                        "Compression ran but produced no savings; kept the original.",
                    ),
                    baybo_context::CompressionOutcome::Cancelled => CompactionNotice::info(
                        "Compaction cancelled; the conversation is unchanged.",
                    ),
                    // The one arm the user has to act on — retype `/compact`,
                    // or let the threshold path retry — so it is the one arm
                    // that does not come back as `Info`.
                    baybo_context::CompressionOutcome::Failed { reason } => {
                        warn!(
                            session_id = %session.id,
                            reason = %reason,
                            "/compact failed; transcript left unchanged"
                        );
                        CompactionNotice::warn(&compaction_failed_text(&reason))
                    }
                };
                let output = TurnOutput::Message {
                    content: vec![ContentBlock::Text(notice.text.clone())],
                    // `/compact` is not a user-chat turn — never pushed.
                    ordinal: None,
                };
                Ok((output, notice))
            },
        )
        .await
    }

    /// One-shot conversation-title seed. Called at the **start** of
    /// `run_inner` (right after the system prompt is seeded, before the first
    /// LLM call), so the title pass runs **concurrently with this turn's
    /// answer** rather than after it — the title depends only on the user's
    /// first question, which is already in context, not on the reply. It
    /// `tokio::spawn`s a DETACHED pass that records a
    /// [`StepKind::TitleGeneration`] step + its `LlmCall` span **under this
    /// turn's own turn** (`current_turn_id`) — so cost + trace attribute to the
    /// triggering turn, exactly like [`Self::maybe_run_progress_observer`],
    /// rather than spinning up a separate maintenance turn. It titles the
    /// session, persists it via `SessionManager::set_title`, and notifies the
    /// [`Self::title_sink`] to broadcast it. Fire-and-forget: the turn never
    /// blocks on it.
    ///
    /// The step rides the **turn's** `cancel_token`: `/stop` closes it as
    /// `Cancelled`, a normal turn leaves it untripped so the pass finishes even
    /// if it briefly outlives the reply. Unlike the background-summary pass it
    /// needs no reap-surviving token — the title is cosmetic and self-heals on
    /// a later turn (durable `session.title` stays `None` until one lands).
    ///
    /// **Gate (all must hold):** the turn is `UserChat`; a
    /// [`Self::title_sink`] is wired (the "a live title surface exists" signal
    /// — present in the running gateway, absent in tests / headless, so
    /// titles are only generated where something renders them); the session is
    /// a top-level user session (`TriggerSource::User`, no lineage — cron /
    /// subagent skipped); it has no title yet (`session.title.is_none()`, the
    /// durable once-only guard, self-healing across rehydration); this actor
    /// hasn't already attempted one ([`Self::title_generation`] present, the
    /// per-actor-lifetime guard); and the transcript has a text-bearing first
    /// user question ([`first_user_question`]).
    async fn maybe_generate_title(
        &mut self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        current_turn_id: TurnId,
        is_user_turn: bool,
        cancel_token: &CancellationToken,
    ) {
        if !is_user_turn || self.title_generation.is_some() {
            return;
        }
        if !matches!(session.trigger, baybo_model::TriggerSource::User)
            || session.lineage.is_some()
            || session.title.is_some()
        {
            return;
        }
        let Some(title_sink) = self.title_sink.clone() else {
            return;
        };
        let Some(sessions) = self.sessions.clone() else {
            return;
        };
        let Some(question) = first_user_question(self.context_manager.messages()) else {
            return;
        };

        let session_id = session.id.clone();
        let user_id = session.user.id.clone();
        let llm_client = self.lite_client.clone();
        let security_gateway = self.security_gateway.clone();
        let model_info = self.lite_client.model_info().clone();
        let recorder = Arc::clone(span_recorder);
        let cancel_token = cancel_token.clone();

        let handle = tokio::spawn(async move {
            let runner = crate::runtime::title::TitleRunner {
                llm_client,
                recorder,
                security_gateway,
                turn_id: current_turn_id,
                user_id,
                session_id: session_id.clone(),
                model_info,
                cancel_token,
            };
            match runner.run(question).await {
                Ok(Some(title)) => match sessions.set_title(&session_id, Some(&title)).await {
                    Ok(()) => title_sink.title_updated(&session_id, &title),
                    Err(e) => warn!(
                        session_id = %session_id,
                        error = %e,
                        "failed to persist session title"
                    ),
                },
                Ok(None) => {}
                Err(e) => warn!(error = %e, "title generation pass failed"),
            }
        });
        self.title_generation = Some(handle);
    }
}

/// Cap on the opening message handed to title generation. Naming a
/// conversation never needs more than its first couple of paragraphs, and
/// the message is unbounded user input — a pasted log as the first turn
/// would otherwise be sent verbatim, which a small lite model may not even
/// have the window for.
const TITLE_QUESTION_MAX_CHARS: usize = 2_000;

/// Extract the session's first genuine user question from the transcript:
/// the first `MessageSource::User` row that actually carries text (a
/// media-only opener — an uncaptioned image with no `Text` block — is
/// skipped, so a session that opens with media then a real question still
/// titles from the question). `None` when there is no text-bearing user row.
/// Truncated to [`TITLE_QUESTION_MAX_CHARS`].
fn first_user_question(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|m| matches!(m.source(), baybo_model::MessageSource::User))
        .find_map(|m| {
            let text = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            Some(match text.char_indices().nth(TITLE_QUESTION_MAX_CHARS) {
                Some((cut, _)) => text[..cut].to_string(),
                None => text.to_string(),
            })
        })
}

#[cfg(test)]
mod first_user_question_tests {
    use super::TITLE_QUESTION_MAX_CHARS;
    use super::first_user_question;
    use baybo_model::{ChatMessage, ContentBlock};

    /// The opener is unbounded user input; a pasted log must not ride
    /// into the title prompt verbatim.
    #[test]
    fn a_long_opener_is_truncated() {
        let long = "x".repeat(TITLE_QUESTION_MAX_CHARS * 3);
        let msgs = vec![ChatMessage::user(vec![ContentBlock::Text(long)])];
        let q = first_user_question(&msgs).expect("text-bearing row");
        assert_eq!(q.chars().count(), TITLE_QUESTION_MAX_CHARS);
    }

    /// Truncation slices on a char boundary, not a byte one.
    #[test]
    fn truncation_is_char_boundary_safe() {
        let long = "\u{03b1}\u{03b2}\u{03b3}".repeat(TITLE_QUESTION_MAX_CHARS);
        let msgs = vec![ChatMessage::user(vec![ContentBlock::Text(long)])];
        let q = first_user_question(&msgs).expect("text-bearing row");
        assert_eq!(q.chars().count(), TITLE_QUESTION_MAX_CHARS);
    }

    #[test]
    fn picks_first_genuine_user_row_over_injected_and_assistant() {
        let msgs = vec![
            ChatMessage::system(vec![ContentBlock::Text("system prompt".into())]),
            ChatMessage::user(vec![ContentBlock::Text(
                "How do I reset my password?".into(),
            )]),
            ChatMessage::assistant(vec![ContentBlock::Text("Sure…".into())]),
            ChatMessage::user(vec![ContentBlock::Text("second question".into())]),
        ];
        assert_eq!(
            first_user_question(&msgs).as_deref(),
            Some("How do I reset my password?")
        );
    }

    #[test]
    fn skips_agent_injected_role_user_rows() {
        let msgs = vec![
            ChatMessage::agent_context(vec![ContentBlock::Text("injected context".into())]),
            ChatMessage::user(vec![ContentBlock::Text("the real question".into())]),
        ];
        assert_eq!(
            first_user_question(&msgs).as_deref(),
            Some("the real question")
        );
    }

    #[test]
    fn none_when_no_user_row_or_media_only() {
        let no_user = vec![ChatMessage::system(vec![ContentBlock::Text("s".into())])];
        assert_eq!(first_user_question(&no_user), None);

        let media_only = vec![ChatMessage::user(vec![ContentBlock::Text(String::new())])];
        assert_eq!(first_user_question(&media_only), None);
    }

    #[test]
    fn advances_past_a_media_only_opener_to_the_first_text_question() {
        let msgs = vec![
            ChatMessage::user(vec![ContentBlock::Text(String::new())]),
            ChatMessage::user(vec![ContentBlock::Text(
                "How do I reset my password?".into(),
            )]),
        ];
        assert_eq!(
            first_user_question(&msgs).as_deref(),
            Some("How do I reset my password?")
        );
    }
}

/// Strip leading/trailing whitespace from the LLM response's text fields.
///
/// `LlmResponse::content` is the flat aggregate string; `content_blocks`
/// preserves the structured form (text / thinking / tool_use / etc.). For
/// each text block — and for the aggregate string — leading/trailing
/// whitespace (including stray `\n` runs) is removed. Interior whitespace
/// is preserved so paragraph breaks, code blocks, and list formatting
/// inside the message stay intact.
///
/// Non-text blocks (`Thinking`, `ToolUse`, `ToolResult`, `Image`, etc.)
/// are untouched: their content is structured data the renderer / next
/// turn relies on verbatim.
fn trim_response_text_edges(response: &mut LlmResponse) {
    let trimmed = response.content.trim();
    if trimmed.len() != response.content.len() {
        response.content = trimmed.to_string();
    }
    for block in &mut response.content_blocks {
        if let ContentBlock::Text(t) = block {
            let trimmed = t.trim();
            if trimmed.len() != t.len() {
                *t = trimmed.to_string();
            }
        }
    }
}

#[cfg(test)]
mod task_reminder_throttle_tests {
    use super::{TURNS_BETWEEN_REMINDERS, TURNS_SINCE_WRITE, should_inject_task_reminder};

    #[test]
    fn holds_during_the_start_of_session_grace_window() {
        // Fresh session: counters at 0 ⇒ no reminder until TURNS_SINCE_WRITE.
        for turn in 1..TURNS_SINCE_WRITE {
            assert!(
                !should_inject_task_reminder(turn, 0, 0),
                "turn {turn} is inside the grace window"
            );
        }
        assert!(should_inject_task_reminder(TURNS_SINCE_WRITE, 0, 0));
    }

    #[test]
    fn recent_task_management_suppresses_the_reminder() {
        // Managed at turn 8: the write gate isn't met until TURNS_SINCE_WRITE
        // turns have passed since, no matter how late in the session.
        let last_tm = 8;
        assert!(!should_inject_task_reminder(
            last_tm + TURNS_SINCE_WRITE - 1,
            last_tm,
            0
        ));
        assert!(should_inject_task_reminder(
            last_tm + TURNS_SINCE_WRITE,
            last_tm,
            0
        ));
    }

    #[test]
    fn reminders_are_spaced_by_turns_between_reminders() {
        // Idle on tasks (last_tm = 0), reminded at turn 10: the next reminder
        // waits TURNS_BETWEEN_REMINDERS even though the write gate stays open.
        let last_rem = 10;
        assert!(!should_inject_task_reminder(
            last_rem + TURNS_BETWEEN_REMINDERS - 1,
            0,
            last_rem
        ));
        assert!(should_inject_task_reminder(
            last_rem + TURNS_BETWEEN_REMINDERS,
            0,
            last_rem
        ));
    }

    #[test]
    fn both_gates_must_hold() {
        // Write gate open but reminded recently → suppressed.
        assert!(!should_inject_task_reminder(100, 0, 95));
        // Reminder gate open but managed recently → suppressed.
        assert!(!should_inject_task_reminder(100, 95, 0));
        // Both open → inject.
        assert!(should_inject_task_reminder(100, 0, 0));
    }
}

#[cfg(test)]
mod notifier_bridge_tests {
    use super::*;
    use baybo_channels::{AgentEvent, AgentOutput};
    use baybo_tools::{NoticeLevel as ToolsNoticeLevel, SessionNotifier};

    fn mk_notifier() -> (DeltaTxNotifier, tokio::sync::mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let n = DeltaTxNotifier {
            tx,
            session_id: "s".into(),
            user_id: "u".into(),
            channel: baybo_model::ChannelType::tui(),
        };
        (n, rx)
    }

    #[test]
    fn warn_forwards_as_agent_output_warn() {
        let (n, mut rx) = mk_notifier();
        n.emit(ToolsNoticeLevel::Warn, "summary", "detail");
        let out = rx.try_recv().expect("notice should be queued");
        match out {
            AgentOutput {
                event:
                    AgentEvent::Notice {
                        level,
                        text,
                        mid_turn,
                        durable_id,
                    },
                session_id,
                user_id,
                ..
            } => {
                assert_eq!(level, baybo_channels::NoticeLevel::Warn);
                assert_eq!(text, "summary: detail");
                assert_eq!(session_id, "s");
                assert_eq!(user_id, "u");
                assert!(mid_turn, "SessionNotifier asides must be mid_turn");
                assert!(durable_id.is_none(), "asides are live-only");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn error_forwards_as_agent_output_error() {
        let (n, mut rx) = mk_notifier();
        n.emit(ToolsNoticeLevel::Error, "blocked", "rationale");
        match rx.try_recv().unwrap() {
            AgentOutput {
                event: AgentEvent::Notice { level, text, .. },
                ..
            } => {
                assert_eq!(level, baybo_channels::NoticeLevel::Error);
                assert_eq!(text, "blocked: rationale");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn empty_detail_collapses_text() {
        let (n, mut rx) = mk_notifier();
        n.emit(ToolsNoticeLevel::Warn, "headline", "");
        match rx.try_recv().unwrap() {
            AgentOutput {
                event: AgentEvent::Notice { text, .. },
                ..
            } => assert_eq!(text, "headline"),
            _ => panic!(),
        }
    }

    #[test]
    fn full_channel_drops_silently() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let n = DeltaTxNotifier {
            tx,
            session_id: "s".into(),
            user_id: "u".into(),
            channel: baybo_model::ChannelType::tui(),
        };
        n.emit(ToolsNoticeLevel::Warn, "first", "");
        // Second emit should not block or panic — try_send drops it.
        n.emit(ToolsNoticeLevel::Warn, "second", "");
        // Only the first one is in the queue.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}

#[cfg(test)]
mod stream_buffer_tests {
    use super::safe_flush_boundary;

    #[test]
    fn empty_flushes_nothing() {
        assert_eq!(safe_flush_boundary(""), 0);
    }

    #[test]
    fn plain_text_flushes_all() {
        let s = "hello world";
        assert_eq!(safe_flush_boundary(s), s.len());
    }

    #[test]
    fn trailing_open_bracket_is_withheld() {
        let s = "hello [";
        assert_eq!(safe_flush_boundary(s), s.find('[').unwrap());
    }

    #[test]
    fn trailing_partial_placeholder_is_withheld() {
        let s = "abc [{REDACTED_SECRET_deadbee";
        assert_eq!(safe_flush_boundary(s), s.find('[').unwrap());
    }

    #[test]
    fn complete_placeholder_flushes_all() {
        let s = "abc [{REDACTED_SECRET_deadbeefdeadbeefdeadbeef}] def";
        assert_eq!(safe_flush_boundary(s), s.len());
    }

    #[test]
    fn high_water_forces_flush() {
        let s: String = "a".repeat(200) + "[";
        assert_eq!(safe_flush_boundary(&s), s.len());
    }
}

#[cfg(test)]
mod skip_leading_whitespace_tests {
    use super::skip_leading_whitespace;

    #[test]
    fn pure_whitespace_first_chunk_is_skipped() {
        let mut chunk = String::from("\n\n\n");
        let mut stripped = false;
        let keep = skip_leading_whitespace(&mut chunk, &mut stripped);
        assert!(!keep);
        assert!(chunk.is_empty());
        assert!(!stripped, "no real content yet, flag must stay false");
    }

    #[test]
    fn mixed_first_chunk_strips_only_leading() {
        let mut chunk = String::from("\n\n  Hello\nworld");
        let mut stripped = false;
        let keep = skip_leading_whitespace(&mut chunk, &mut stripped);
        assert!(keep);
        assert_eq!(chunk, "Hello\nworld");
        assert!(stripped);
    }

    #[test]
    fn passthrough_after_first_real_content() {
        let mut stripped = true;
        let mut chunk = String::from("\n  next paragraph");
        let keep = skip_leading_whitespace(&mut chunk, &mut stripped);
        assert!(keep);
        assert_eq!(chunk, "\n  next paragraph", "interior whitespace preserved");
    }

    #[test]
    fn passthrough_empty_chunk_returns_false() {
        let mut stripped = true;
        let mut chunk = String::new();
        assert!(!skip_leading_whitespace(&mut chunk, &mut stripped));
    }

    #[test]
    fn sequential_whitespace_chunks_keep_flag_false() {
        let mut stripped = false;
        let mut a = String::from("  ");
        assert!(!skip_leading_whitespace(&mut a, &mut stripped));
        let mut b = String::from("\n");
        assert!(!skip_leading_whitespace(&mut b, &mut stripped));
        let mut c = String::from(" Hello");
        assert!(skip_leading_whitespace(&mut c, &mut stripped));
        assert_eq!(c, "Hello");
        assert!(stripped);
    }
}

#[cfg(test)]
mod trim_response_text_edges_tests {
    use super::trim_response_text_edges;
    use baybo_llm::{LlmResponse, TokenUsage};
    use baybo_model::ContentBlock;

    fn resp(content: &str, blocks: Vec<ContentBlock>) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            content_blocks: blocks,
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            thinking: None,
        }
    }

    #[test]
    fn strips_leading_newlines_on_aggregate_content() {
        let mut r = resp("\n\n\nHello world", Vec::new());
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, "Hello world");
    }

    #[test]
    fn strips_both_edges_keeps_interior() {
        let mut r = resp("\n  Title\n\nBody\n  ", Vec::new());
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, "Title\n\nBody");
    }

    #[test]
    fn strips_each_text_block() {
        let mut r = resp(
            "doesn't matter",
            vec![
                ContentBlock::Text("\n\nfirst paragraph".into()),
                ContentBlock::Text("second paragraph\n\n".into()),
            ],
        );
        trim_response_text_edges(&mut r);
        match &r.content_blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, "first paragraph"),
            other => panic!("expected text, got {other:?}"),
        }
        match &r.content_blocks[1] {
            ContentBlock::Text(t) => assert_eq!(t, "second paragraph"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn leaves_non_text_blocks_alone() {
        let tool_use = ContentBlock::ToolUse {
            id: "id1".into(),
            name: "Foo".into(),
            input: serde_json::json!({"k": "v"}),
            signature: None,
        };
        let mut r = resp("ignored", vec![tool_use.clone()]);
        trim_response_text_edges(&mut r);
        assert_eq!(r.content_blocks[0], tool_use);
    }

    #[test]
    fn no_op_when_already_clean() {
        let mut r = resp("Hello", vec![ContentBlock::Text("Already trimmed".into())]);
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, "Hello");
        match &r.content_blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, "Already trimmed"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn preserves_interior_code_block_indentation() {
        let body = "Heading\n\n    indented code line\n    second line";
        let mut r = resp(
            &format!("\n\n{body}\n\n"),
            vec![ContentBlock::Text(format!("\n{body}\n"))],
        );
        trim_response_text_edges(&mut r);
        assert_eq!(r.content, body);
        match &r.content_blocks[0] {
            ContentBlock::Text(t) => assert_eq!(t, body),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod session_end_gate_tests {
    //! `should_fire_session_end` decides whether `Memory::on_session_end`
    //! runs when an actor processes `ActorStop`. Subagent actors also stop,
    //! but their teardown is not a user-session ending — firing the hook for
    //! them would write garbage memory. Also covers the shared `is_subagent`
    //! predicate that gates the background-summary pass (subagents skip it).
    use super::{is_subagent, should_fire_session_end};
    use baybo_model::{
        ChannelType, Lineage, LineageKind, Session, SessionId, SessionState, TriggerSource, TurnId,
        User,
    };
    use chrono::Utc;

    fn session_with(trigger: TriggerSource, lineage: Option<Lineage>) -> Session {
        let now = Utc::now();
        let id = SessionId::from("sess-gate");
        Session {
            id: id.clone(),
            user: User {
                id: "user-gate".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: id,
            trigger,
            lineage,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    #[test]
    fn fires_for_root_user_session() {
        assert!(should_fire_session_end(&session_with(
            TriggerSource::User,
            None
        )));
    }

    #[test]
    fn fires_for_root_cron_session() {
        let s = session_with(
            TriggerSource::Cron {
                cron_job_id: "c-1".into(),
                origin_session_id: None,
                conversation: true,
                job_title: None,
            },
            None,
        );
        assert!(should_fire_session_end(&s));
    }

    #[test]
    fn skips_subagent_session() {
        let lineage = Lineage {
            parent_session_id: SessionId::from("parent"),
            parent_turn_id: TurnId::new(),
            parent_span_id: None,
            kind: LineageKind::Subagent,
        };
        // Subagents inherit their parent's trigger, so the User flag alone
        // shouldn't unlock the hook for them.
        assert!(!should_fire_session_end(&session_with(
            TriggerSource::User,
            Some(lineage),
        )));
    }

    #[test]
    fn is_subagent_true_only_for_lineage_subagent() {
        assert!(!is_subagent(&session_with(TriggerSource::User, None)));
        let lineage = Lineage {
            parent_session_id: SessionId::from("parent"),
            parent_turn_id: TurnId::new(),
            parent_span_id: None,
            kind: LineageKind::Subagent,
        };
        assert!(is_subagent(&session_with(
            TriggerSource::User,
            Some(lineage)
        )));
    }
}

#[cfg(test)]
mod tool_completion_summary_tests {
    use super::*;

    fn executed(
        output: anyhow::Result<ToolOutput>,
        approval: Option<ApprovalDecision>,
    ) -> ExecutedTool {
        ExecutedTool { output, approval }
    }

    #[test]
    fn a_recorded_denial_reads_denied_even_though_the_error_is_untyped() {
        // The regression this pins: a tool that prompts MID-CALL folds the
        // refusal into its own error, which reaches us as a plain `anyhow`
        // (the executor sanitizes and re-wraps it, losing the type). Keying
        // off the error type alone reported that as a crash — an "error" step
        // with no verdict — while the user had explicitly denied it.
        let (status, summary) = tool_completion_summary(&executed(
            Err(anyhow::anyhow!("skill 'x' requires env-var approval")),
            Some(ApprovalDecision::Deny),
        ));
        assert_eq!(status, ToolStatus::Denied);
        assert_eq!(summary, "denied");
    }

    #[test]
    fn an_approved_call_that_then_fails_still_reads_as_an_error() {
        let (status, _) = tool_completion_summary(&executed(
            Err(anyhow::anyhow!("boom")),
            Some(ApprovalDecision::ApproveAlways),
        ));
        assert_eq!(status, ToolStatus::Error, "the approval isn't the outcome");
    }

    #[test]
    fn an_ungated_call_is_unaffected() {
        let (status, summary) =
            tool_completion_summary(&executed(Ok(ToolOutput::Text("3 files".into())), None));
        assert_eq!(status, ToolStatus::Ok);
        assert_eq!(summary, "3 files");
    }
}
