use std::sync::Arc;

use aura_channels::{
    AgentEvent, AgentOutput, COMPACT_COMMAND, OutgoingMessage, ToolStatus, TurnStatus,
};
use aura_context::ContextManager;
use aura_job::{JobInput, JobLifecycle, JobOutput, JobShape};
use aura_llm::{
    Attribution, BillableLlm, BoundBilledLlm, ChatRequest, LlmResponse, StreamEvent, TokenUsage,
    ToolDefinitionForLlm,
};
use aura_memory::{Memory, MemoryContext};
use aura_model::{
    ChatMessage, ContentBlock, JobId, LlmEntryName, MessageSource, Role, SessionId, ThinkingContent,
};
use futures::StreamExt;
use tokio::sync::mpsc;

use aura_model::{LineageKind, Session, TriggerSource};
use aura_tools::{ToolConcurrency, ToolOutput, ToolRegistry};
use aura_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanRecorder, StepHandle, StepKind,
};
use tracing::{debug, error, info, warn};

use crate::runtime::compression::CompressionRunner;
use crate::runtime::error_recovery::ErrorHandler;
use crate::runtime::progress_observer::{
    ObserverState, ProgressObserverRunner, build_observer_prompt, channel_wants_progress,
    should_fire_observer,
};
use crate::runtime::scope::JobSpec;

use crate::runtime::tool_executor::ToolExecutor;
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

/// Upper bound on how many [`ToolConcurrency::Concurrent`] tool calls
/// run at once within a single LLM response. A
/// [`ToolConcurrency::Exclusive`] call (any tool that mutates state)
/// acquires *all* of these permits, so it runs alone — it waits for
/// in-flight pool calls to drain and blocks any other pool call until it
/// returns. A [`ToolConcurrency::Independent`] call (`spawn_subagent`)
/// acquires no permit and self-bounds out-of-band, so the pool never
/// throttles subagent fan-out. Like the per-tool timeout ceiling, the
/// cap lives in code rather than `aura.json`.
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

/// Derive the `(status, summary)` for a finished tool call's
/// `ToolCompleted` progress event from its result. Presentation-only and
/// content-light; the summary still passes the leak boundary before it is
/// emitted. Mirrors the result match the loop runs for the LLM-facing
/// `tool_result` text.
fn tool_completion_summary(result: &anyhow::Result<ToolOutput>) -> (ToolStatus, String) {
    match result {
        Ok(ToolOutput::Text(s)) => (ToolStatus::Ok, summarize_text(s)),
        Ok(ToolOutput::Json(_)) => (ToolStatus::Ok, "ok".to_string()),
        Ok(ToolOutput::WithAttachments { attachments, .. }) => (
            ToolStatus::Ok,
            format!("{} attachment(s)", attachments.len()),
        ),
        Ok(ToolOutput::MultiModalText { llm_images, .. }) => {
            (ToolStatus::Ok, format!("{} image(s)", llm_images.len()))
        }
        Ok(ToolOutput::Error(msg)) => (ToolStatus::Error, truncate_summary(msg)),
        Err(e) => {
            if let Some(aura_tools::ToolError::Denied { .. }) =
                e.downcast_ref::<aura_tools::ToolError>()
            {
                (ToolStatus::Denied, "denied".to_string())
            } else {
                (ToolStatus::Error, truncate_summary(&e.to_string()))
            }
        }
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
/// ([`aura_context::prompts::cancelled_turn`]) appended so the model knows the
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
        Some(ContentBlock::Text(t)) => t.push_str(aura_context::prompts::cancelled_turn::SUFFIX),
        _ => blocks.push(ContentBlock::Text(
            aura_context::prompts::cancelled_turn::marker_block_text(),
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
    session_id: aura_model::SessionId,
    user_id: String,
    channel: aura_model::ChannelType,
}

impl aura_tools::SessionNotifier for DeltaTxNotifier {
    fn emit(&self, level: aura_tools::NoticeLevel, summary: &str, detail: &str) {
        let level = match level {
            aura_tools::NoticeLevel::Info => aura_channels::NoticeLevel::Info,
            aura_tools::NoticeLevel::Warn => aura_channels::NoticeLevel::Warn,
            aura_tools::NoticeLevel::Error => aura_channels::NoticeLevel::Error,
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
            event: AgentEvent::Notice { level, text },
        });
    }

    fn emit_attachment(&self, blocks: &[ContentBlock]) {
        if blocks.is_empty() {
            return;
        }
        // Media carries no free text, so no leak boundary applies. `try_send`
        // (like `emit`) drops on a full channel rather than blocking the
        // sync tool path.
        let _ = self.tx.try_send(AgentOutput {
            session_id: self.session_id.clone(),
            user_id: self.user_id.clone(),
            channel: self.channel.clone(),
            event: AgentEvent::Attachment(blocks.to_vec()),
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
    /// assistant turn (text / thinking); tool media was delivered live as it
    /// was produced, never bundled here.
    Final { outgoing: OutgoingMessage },
    /// LLM emitted tool calls; loop continues. `task_mutated` is `true`
    /// when one of this iteration's tool calls changed the planning
    /// checklist, so the caller refreshes the reminder before the next
    /// LLM call.
    Continue { task_mutated: bool },
}

/// Captured inputs for the deferred `Memory::on_job_complete` write. Built
/// inside `with_job`'s body at the Final-iteration boundary and returned up
/// so `run()` can fire `spawn_job_complete_write` **after** `with_job`
/// commits the job — otherwise a cancel-race in `with_job`'s post-body
/// window could let a memorized turn outlive a `Cancelled` job row.
struct PendingMemoryWrite {
    user_input: Vec<ContentBlock>,
    final_output: Vec<ContentBlock>,
}

/// Source of mid-turn user messages ("interjections") that arrived while the
/// loop was running. Consulted at each tool boundary (after a tool batch, before
/// the next LLM call) — never mid-call, so injection stays non-preemptive.
/// Implemented by the actor over its mailbox (draining the leading run of
/// non-slash `UserInput`s); a fake stands in for it in tests. Returns each
/// injectable message's content in arrival order, or empty when nothing is
/// queued. See `docs/mid-turn-user-interjection.md`.
///
/// `Send` supertrait so the `&mut dyn InterjectionSource` the loop holds across
/// `.await` points keeps the agent task `Send`.
pub trait InterjectionSource: Send {
    fn drain_injectable(&mut self) -> Vec<Vec<ContentBlock>>;
    /// Drop any queued injectable messages without running them. Used when a
    /// turn is `/stop`-cancelled so client-fired interjections still sitting in
    /// the mailbox don't run as follow-up turns once the actor resumes its loop.
    fn discard_pending(&mut self) {
        let _ = self.drain_injectable();
    }
}

/// Core conversation loop: LLM call -> parse -> Tool/Skill dispatch -> repeat.
/// The recall query for a job, or `None` for job kinds that don't recall.
/// Memory recall/write run only for `UserChat` and `Cron` jobs — `System`,
/// `Spawned` (subagent), and `SubagentNotification` have no direct user input
/// and would pollute or double-write. The exhaustive match forces a
/// classification when a new `JobInput` variant is added.
fn memory_recall_query(input: &JobInput) -> Option<Vec<ContentBlock>> {
    match input {
        JobInput::UserChat { content } => Some(content.clone()),
        JobInput::Cron { action_payload } => Some(cron_prompt_blocks(action_payload)),
        JobInput::System { .. }
        | JobInput::Spawned { .. }
        | JobInput::SubagentNotification { .. } => None,
    }
}

/// Best-effort extraction of a cron fire's prompt text for the recall query.
/// The cron router writes `action_payload` as `{cron_job_id, prompt}` (an
/// opaque trace blob — see `aura_job::JobInput::Cron`); a missing or non-string
/// `prompt` yields an empty query, so recall degrades to a no-op rather than
/// coupling hard to that shape.
fn cron_prompt_blocks(action_payload: &serde_json::Value) -> Vec<ContentBlock> {
    match action_payload.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => vec![ContentBlock::Text(p.to_string())],
        _ => Vec::new(),
    }
}

/// Whether the `on_session_end` memory hook should fire for this session.
/// The session-level analogue of [`memory_recall_query`]: only sessions a person
/// would call "theirs" — root `User`/`Cron` sessions, not subagents.
/// Subagent actors send `ActorStop` when they finish, but their shutdown
/// is not a user-session ending. Exhaustive arms force a classification
/// when a new `TriggerSource` / `LineageKind` variant is added.
fn should_fire_session_end(session: &Session) -> bool {
    let user_trigger = match &session.trigger {
        TriggerSource::User | TriggerSource::Cron { .. } => true,
    };
    let user_lineage = match &session.lineage {
        None => true,
        Some(l) => match &l.kind {
            LineageKind::Subagent => false,
        },
    };
    user_trigger && user_lineage
}

pub struct AgentLoop {
    /// Currently-active client, re-resolved from `llm_pool` at the
    /// start of each turn ([`Self::refresh_active_llm`]) so a config
    /// hot-reload takes effect on the next message.
    llm_client: Arc<BillableLlm>,
    /// Hot-swappable pool handle this loop re-resolves against per turn.
    llm_pool: crate::runtime::llm_pool::LlmPoolHandle,
    /// The pin this loop resolves: `None` ⇒ pool default (user / cron
    /// actors); `Some` ⇒ a subagent's pinned entry name.
    initial_llm: Option<LlmEntryName>,
    tool_registry: Arc<ToolRegistry>,
    tool_executor: Arc<ToolExecutor>,
    context_manager: ContextManager,
    max_iterations: usize,
    security_gateway: Arc<SecurityGateway>,
    error_handler: ErrorHandler,
    /// Resolved workspace paths. Today only the background-summary
    /// pass reads it (to write `summary.md`); other future system
    /// work may want it too. `None` in tests that don't exercise such
    /// passes.
    workspace_paths: Option<Arc<aura_workspace::WorkspacePaths>>,
    /// Cross-session manager — used by passes that operate across
    /// sessions (today: background summary, for transcript loads and
    /// summary metadata writes). Distinct from the `SessionManager`
    /// plumbed inside `ContextManager` because that one is
    /// per-session-bound.
    sessions: Option<Arc<crate::SessionManager>>,
    /// Pluggable long-term memory. `None` disables every memory hook (recall,
    /// `on_job_complete`) — the runtime wires `None` until a real
    /// implementation is registered.
    memory: Option<Arc<dyn Memory>>,
    /// Durable per-session planning checklist (`Task*`). The loop
    /// loads it each turn — and after any checklist-mutating tool call — to
    /// refresh the transient reminder the model sees via `ContextManager`.
    /// Always present in production (sourced from the `Store` bundle).
    task_store: Arc<dyn aura_store::TaskStore>,
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
    /// At-most-one handle for the in-actor background-summary pass. The
    /// trigger gate ([`Self::maybe_run_background_compression`]) checks
    /// it before spawning: a present, not-yet-finished handle means a
    /// pass is already running for this session, so a second is skipped.
    /// Detached (its own fresh `CancellationToken`, NOT derived from the
    /// surrounding actor's token) so the idle reaper cancelling that
    /// token can't kill an in-flight pass — mirrors
    /// [`Self::spawn_session_end_write`].
    bg_compression: Option<tokio::task::JoinHandle<()>>,
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
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_executor: Arc<ToolExecutor>,
    pub context_manager: ContextManager,
    pub max_iterations: usize,
    pub security_gateway: Arc<SecurityGateway>,
    /// Workspace paths. Used by the background-summary pass to write
    /// on-disk `summary.md`.
    pub workspace_paths: Option<Arc<aura_workspace::WorkspacePaths>>,
    /// Cross-session manager. Used by the background-summary pass for
    /// transcript loads + summary metadata writes.
    pub sessions: Option<Arc<crate::SessionManager>>,
    /// Pluggable long-term memory handle — one registered implementation, or
    /// `None` to disable the memory hooks (recall / `on_job_complete`).
    pub memory: Option<Arc<dyn Memory>>,
    /// Durable per-session planning-checklist store backing the `Task*` tools
    /// and the per-turn reminder.
    pub task_store: Arc<dyn aura_store::TaskStore>,
}

/// Task-reminder throttle (mirrors Claude Code's `TODO_REMINDER_CONFIG`): the
/// model-facing reminder is injected only once the model has gone
/// `TURNS_SINCE_WRITE` turns without managing tasks AND it has been at least
/// `TURNS_BETWEEN_REMINDERS` turns since the last reminder — so it nudges
/// periodically instead of riding every request. The web `TaskList` surface is
/// **not** throttled (it tracks the live list).
const TURNS_SINCE_WRITE: u64 = 10;
const TURNS_BETWEEN_REMINDERS: u64 = 10;

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
            tool_registry,
            tool_executor,
            context_manager,
            max_iterations,
            security_gateway,
            workspace_paths,
            sessions,
            memory,
            task_store,
        } = config;
        let (llm_client, _effective_name) = llm_pool.read().resolve(initial_llm.as_ref());
        let mut context_manager = context_manager;
        context_manager.set_active_model_context_window(llm_client.model_info().context_window);

        Self {
            llm_client,
            llm_pool,
            initial_llm,
            tool_registry,
            tool_executor,
            context_manager,
            max_iterations,
            security_gateway,
            error_handler: ErrorHandler::default(),
            workspace_paths,
            sessions,
            memory,
            task_store,
            turn_counter: 0,
            last_task_management_turn: 0,
            last_reminder_turn: 0,
            bg_compression: None,
        }
    }

    /// Delegate to `ContextManager::restore_from_store` — the manager
    /// is bound to its session at construction time and owns the
    /// load path. Kept on `AgentLoop` so `AgentActor::run` doesn't
    /// have to reach inside the loop's private state.
    pub async fn restore_transcript_from_store(&mut self) {
        self.context_manager.restore_from_store().await;
    }

    /// Snapshot the in-memory transcript so a fallible turn can be rolled
    /// back. Used by the subagent-notification retry path: that turn's
    /// synthetic prompt is appended in-memory only (not persisted), so on
    /// failure the actor restores this snapshot to drop the row before the
    /// next retry rebuilds it — otherwise the live context would stack a copy
    /// per attempt.
    pub fn context_snapshot(&self) -> Vec<ChatMessage> {
        self.context_manager.messages().to_vec()
    }

    /// Restore a transcript snapshot taken by [`Self::context_snapshot`].
    pub fn restore_context(&mut self, messages: Vec<ChatMessage>) {
        self.context_manager.restore_messages(messages);
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

    /// Seed the system prompt (+ skill reminder) if the transcript doesn't
    /// already lead with it. Idempotent. Exposed so the subagent-notification
    /// path can establish the (persisted) system row *before* snapshotting the
    /// context for rollback — otherwise a rollback on a fresh session would
    /// drop the just-persisted system row in-memory and the next retry would
    /// re-seed and re-persist it.
    pub async fn ensure_system_prompt_seeded(&mut self) {
        self.context_manager.ensure_seeded().await;
    }

    /// Run the main conversation loop for a single user message.
    ///
    /// When `delta_tx` is `Some`, each text chunk emitted by the LLM is
    /// forwarded as `AgentEvent::AnswerDelta` so adapters that support partial
    /// rendering (e.g. the TUI) can show incremental output. The final
    /// `OutgoingMessage` returned here should still be dispatched by the
    /// caller as `AgentEvent::Message` so non-streaming adapters receive
    /// the canonical response.
    // `job_input` records why this job exists (provenance: which trigger
    // kicked it off — User / Cron / System / Spawned), used for the JobSpec.
    // The turn's triggering message is appended to the transcript by the
    // actor *before* this runs (via `append_user_message` / `append_cron_fire`
    // / `append_subagent_notification`), so the loop iterates the current
    // context rather than appending here.
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
        let (client, _name) = pool.resolve(self.initial_llm.as_ref());
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

    /// Re-pin which `aura.json` entry this loop resolves against and
    /// apply it now (swaps the client + context window via
    /// [`Self::refresh_active_llm`]) so the next turn runs on the new
    /// model. `None` reverts to the pool default. Drives the chat
    /// per-session model switch ([`crate::actor::AgentMessage::SetModel`]);
    /// the actor also persists the pin to `session.state.last_llm` so it
    /// survives eviction.
    pub fn set_initial_llm(&mut self, llm: Option<LlmEntryName>) {
        self.initial_llm = llm;
        self.refresh_active_llm();
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &mut self,
        session: &mut Session,
        job_input: JobInput,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        parent_job_id: Option<JobId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        cancel_token: CancellationToken,
        interjections: Option<&mut dyn InterjectionSource>,
    ) -> anyhow::Result<OutgoingMessage> {
        self.refresh_active_llm();
        // Memory recall query (and write eligibility) for this job — `None`
        // for kinds that don't participate (System / Spawned / notification).
        let memory_query = memory_recall_query(&job_input);
        let spec = JobSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            shape: JobShape::Turn,
            input: job_input,
            parent_job_id,
        };
        // Capture what the post-`with_job` memory spawn needs from `self`
        // and `session` before the closure takes `&mut self` + `&mut session`
        // by move — once `with_job`'s body runs we can no longer touch either
        // directly out here.
        let memory_handle = self.memory.clone();
        let memory_user_id = session.user.id.clone();
        let memory_session_id = session.id.clone();
        // The `on_job_complete` spawn is intentionally OUTSIDE `with_job`'s
        // body: `with_job`'s post-body window can still mark the job
        // `Cancelled` (cancel-race case 3 in `scope.rs`), in which case the
        // body's Ok is suppressed and `with_job` returns Err — so a spawn
        // launched inside the body would persist memory for a turn the
        // runtime later treats as cancelled. Carry `PendingMemoryWrite` up
        // through `with_job`'s `T` and fire only once it has returned Ok.
        let (outgoing, pending_write) = crate::runtime::scope::with_job(
            job_lifecycle,
            cancel_token.clone(),
            spec,
            |job_id| async move {
                let (outgoing, pending) = self
                    .run_inner(
                        session,
                        job_lifecycle,
                        span_recorder,
                        job_id,
                        delta_tx,
                        cancel_token,
                        interjections,
                        memory_query,
                    )
                    .await?;
                let output = JobOutput::Message {
                    content: outgoing.content.clone(),
                };
                let pending_with_id = pending.map(|p| (job_id, p));
                Ok((output, (outgoing, pending_with_id)))
            },
        )
        .await?;
        // Past the cancel-race window — `with_job` returned Ok, so the job
        // row is `Complete`. Safe to bill the memory write against it.
        if let Some((job_id, write)) = pending_write {
            Self::spawn_job_complete_write(
                memory_handle,
                memory_user_id,
                memory_session_id,
                job_id,
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
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        cancel_token: CancellationToken,
        mut interjections: Option<&mut dyn InterjectionSource>,
        memory_query: Option<Vec<ContentBlock>>,
    ) -> anyhow::Result<(OutgoingMessage, Option<PendingMemoryWrite>)> {
        self.context_manager.ensure_seeded().await;

        // Tool-authored notices (`AgentEvent::Notice`) ride the job-wide
        // delta_tx directly, not the per-iteration `iter_delta_tx`: they
        // are a distinct output variant from the LLM's streamed `AnswerDelta`
        // and must reach the channel on every iteration, independent of
        // any per-iteration streaming decision.
        let notifier: Option<Arc<dyn aura_tools::SessionNotifier>> = delta_tx.as_ref().map(|tx| {
            Arc::new(DeltaTxNotifier {
                tx: tx.clone(),
                session_id: session.id.clone(),
                user_id: session.user.id.clone(),
                channel: session.channel.clone(),
            }) as Arc<dyn aura_tools::SessionNotifier>
        });

        // Expand an explicit `/command` skill invocation before the loop:
        // context reads the matching skill's body and appends it (persisted +
        // JSONL-logged) as a hidden agent-context row for the loop to act on.
        self.context_manager.expand_slash_command().await;

        // Recall relevant long-term memories for the triggering input and
        // inject them (framed) before the first LLM call. No-op without a
        // memory impl or for ineligible job kinds (`memory_query` is `None`).
        if let Some(query) = memory_query.as_deref() {
            self.recall_and_inject(query, session, span_recorder, job_id, &cancel_token)
                .await;
        }
        // Accumulates this job's user-authored input (initial prompt + any
        // mid-turn interjections) for the `on_job_complete` write at turn end.
        let mut job_user_input: Vec<ContentBlock> = memory_query.clone().unwrap_or_default();

        // Iterative LLM loop
        let mut iterations = 0;
        let turn_started = std::time::Instant::now();
        let mut observer_state = ObserverState::default();
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
            // this, a `cancel(job_id, ...)` admin call (which trips the
            // registered token before flipping the row) lets the loop
            // finish whatever it's doing and run another LLM call /
            // compress / tool-call before observing the cancel. Tools
            // and the LLM still get the token via their own paths;
            // this catches the orchestration-layer wait windows.
            if cancel_token.is_cancelled() {
                warn!(job_id = %job_id, iterations, "cancel observed at iteration boundary; aborting loop");
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
                // and fold it into this job's input for the end-of-turn write.
                if memory_query.is_some() {
                    for content in &drained {
                        self.recall_and_inject(
                            content,
                            session,
                            span_recorder,
                            job_id,
                            &cancel_token,
                        )
                        .await;
                        job_user_input.extend(content.iter().cloned());
                    }
                }
                // Iteration-boundary summary-refresh check.
                self.maybe_run_background_compression(
                    session,
                    job_lifecycle,
                    span_recorder,
                    job_id,
                    /* job_done */ false,
                )
                .await;
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
                job_id,
                &cancel_token,
                delta_tx.as_ref(),
            )
            .await?;

            // Here (post-compression, between iterations) the context
            // snapshot the observer reads is coherent — no dangling tool_use.
            self.maybe_run_progress_observer(
                session,
                span_recorder,
                job_id,
                &cancel_token,
                delta_tx.as_ref(),
                iterations,
                turn_started,
                &mut observer_state,
            )
            .await;

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
                job_id,
                StepKind::LlmIteration,
                Some((&cancel_token, aura_job::CancelReason::ParentCancelled)),
                |step| {
                    let fut = self.run_iteration(
                        session,
                        span_recorder,
                        step,
                        job_id,
                        iterations,
                        iter_delta_tx,
                        notifier.clone(),
                        &cancel_token,
                    );
                    async move { Ok((LifecycleOutcome::Ok, fut.await?)) }
                },
            )
            .await?;

            match outcome {
                IterationOutcome::Final { outgoing } => {
                    // End-of-job summary-refresh check. The activity
                    // disjunct is satisfied by `job_done = true`;
                    // the tokens / diff conjuncts still apply.
                    self.maybe_run_background_compression(
                        session,
                        job_lifecycle,
                        span_recorder,
                        job_id,
                        /* job_done */ true,
                    )
                    .await;
                    // Capture the memory write inputs and return them up to
                    // `run()` — the actual `spawn_job_complete_write` fires
                    // **after** `with_job` accepts the job, so a cancel-race
                    // in `with_job`'s post-body window can't memorize a
                    // cancelled turn.
                    let pending = memory_query.is_some().then(|| PendingMemoryWrite {
                        user_input: std::mem::take(&mut job_user_input),
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
                }
            }
        }

        // If we exhausted iterations, return what we have. Any media the
        // tools produced was already delivered live as it was produced, so
        // there's nothing to append here.
        let content = vec![ContentBlock::Text(
            "I've reached the maximum number of processing steps. Please try again with a simpler request.".to_string(),
        )];
        // Max-iterations fallback. No assistant row was persisted at
        // the loop end — the early-return path inside `run_iteration`
        // is the only one that calls `append_context_message`, so
        // there's no ordinal to stamp here. `on_job_complete` is also
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
        job_id: JobId,
        iterations: usize,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        notifier: Option<Arc<dyn aura_tools::SessionNotifier>>,
        cancel_token: &CancellationToken,
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
            let response_blocks = if response.content_blocks.is_empty() {
                vec![ContentBlock::Text(response.content.clone())]
            } else {
                response.content_blocks.clone()
            };

            let final_text = aura_llm::multimodal::extract_text(&response_blocks);

            info!(
                iterations,
                content_len = final_text.len(),
                "conversation loop complete"
            );

            // The reply blocks are both the channel-bound content and the
            // persisted assistant row (tool media was delivered live, never
            // bundled here). Capture the persisted ordinal so the channel
            // adapter can stamp the live `Frame::Message`.
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
        // (streaming turns only; cron / subagent pass `delta_tx = None`).
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
        let user_for_calls = session.user.clone();
        // Gate (Copy, captured per closure): only a user-facing session may
        // background a slow command — keeps cron / nested-subagent bash on
        // kill-on-timeout. Mirrors the subagent-conversion gate.
        let background_eligible = session.supports_background_jobs();
        let recorder_for_calls = Arc::clone(span_recorder);
        let step_for_calls = step.clone();
        let notifier_for_calls = notifier.clone();
        let llm_for_calls = Arc::clone(&self.llm_client);
        let registry_for_calls = Arc::clone(&self.tool_registry);
        let concurrency_limiter = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TOOL_CALLS));
        let exec_futures = response.tool_calls.iter().map(|tc| {
            let executor = Arc::clone(&executor);
            let session_id = session_id_for_calls.clone();
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
                        &user,
                        &approved,
                        &recorder,
                        &step,
                        triggering_llm_span,
                        tool_use_id,
                        None,
                        Some(job_id),
                        cancel,
                        notifier,
                        Some(&bind_source),
                        background_eligible,
                    )
                    .await
            }
        });
        let tool_results = futures::future::join_all(exec_futures).await;

        // Sequential post-processing: append results in `tool_calls`
        // order so context state stays byte-stable across calls.
        for (tool_call, tool_result) in response.tool_calls.iter().zip(tool_results) {
            let (status, raw_summary) = tool_completion_summary(&tool_result);
            self.emit_tool_completed(delta_tx, session, tool_call.id.clone(), status, raw_summary)
                .await;

            // Count a grouped subagent spawn into its barrier cohort so the
            // turn-end seal knows the member total. Only a *successful
            // dispatch* counts — a router-side failure (unregistered backend,
            // closed channel, …) comes back as `Ok("[subagent failed: …]")`
            // with no escorted result to ever arrive, so `is_ok()` is too
            // broad: counting it would stall the cohort until its group
            // timeout. A real dispatch returns the ack with its `bg-…` handle.
            let dispatched = matches!(
                &tool_result,
                Ok(ToolOutput::Text(t)) if t.starts_with(aura_model::BACKGROUND_DISPATCH_ACK_PREFIX)
            );
            if tool_call.name == aura_model::SPAWN_SUBAGENT_TOOL_NAME
                && dispatched
                && let Some(group) = tool_call
                    .arguments
                    .get("group")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            {
                // Namespace the cohort by this turn's `job_id` so reusing a
                // group name in a later turn opens a fresh cohort rather than
                // extending the prior (still-sealed, still-draining) one. The
                // spawner stamps the escorted member's `group` through the
                // same helper, so routing back into the cohort agrees.
                session
                    .state
                    .background_groups
                    .entry(aura_model::GroupState::cohort_key(job_id, group))
                    .or_default()
                    .expected += 1;
            }

            let raw_result_text = match &tool_result {
                Ok(ToolOutput::Text(s)) => s.clone(),
                Ok(ToolOutput::Json(v)) => {
                    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
                }
                // A tool that delivers media to the user does so itself via
                // `ctx.notifier.emit_attachment` (e.g. `SendFile`); the loop
                // forwards only the text result to the LLM.
                Ok(ToolOutput::WithAttachments { text, .. })
                | Ok(ToolOutput::MultiModalText { text, .. }) => text.clone(),
                Ok(ToolOutput::Error(msg)) => format!("Error: {msg}"),
                Err(e) => {
                    if let Some(denied) = e.downcast_ref::<aura_tools::ToolError>()
                        && matches!(denied, aura_tools::ToolError::Denied { .. })
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
            // injection detector in `aura-security` while the cap + spill +
            // envelope framing live in `aura-context`; the loop bridges the
            // two by feeding the scan's rule names into the wrapper.
            let capped = self.context_manager.cap_tool_output(raw_result_text).await;
            let warnings = self.security_gateway.detect_injection(&capped);
            let warning_rules: Vec<&str> = warnings.iter().map(|w| w.rule_name.as_str()).collect();
            let wrapped = aura_context::prompts::tool_output::wrap_tool_output(
                &tool_call.name,
                &capped,
                &warning_rules,
            );

            // Append tool result to context with the tool_use_id so the
            // LLM can correlate results with their originating calls.
            let tool_msg = ChatMessage::tool_result(tool_call.id.clone(), wrapped);
            self.context_manager.append(&tool_msg).await;
        }

        // Flush accumulated approvals back into session state.
        session.state.approved_resources = approved.lock().clone();

        let task_mutated = response
            .tool_calls
            .iter()
            .any(|tc| aura_model::TASK_MUTATING_TOOL_NAMES.contains(&tc.name.as_str()));
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
    ) -> anyhow::Result<(LlmResponse, aura_model::SpanId)> {
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
                    if cancel_token.is_cancelled() || !self.error_handler.should_retry(attempt, &e)
                    {
                        return Err(e);
                    }
                    let backoff = self.error_handler.backoff_duration(attempt);
                    warn!(
                        attempt = attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e,
                        "retrying LLM call after transient error"
                    );
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
    ) -> anyhow::Result<(LlmResponse, aura_model::SpanId)> {
        let model_info = self.llm_client.model_info();

        let tool_defs: Vec<ToolDefinitionForLlm> = self
            .tool_registry
            .tool_definitions()
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
        };

        let input_messages = self.context_manager.build_call_input_marker().await;

        let cancel = cancel_token.clone();
        crate::runtime::scope::with_llm_span(
            span_recorder.as_ref(),
            step,
            step.job_id,
            LlmCallBegin {
                model_id: model_info.id.clone(),
                provider: model_info.provider.clone(),
                provider_config_hash: String::new(),
                input_messages,
                temperature: None,
            },
            Some((cancel_token, aura_job::CancelReason::ParentCancelled)),
            |span| async move {
                // Bind this call to its `LlmCall` span so the spend lands
                // on the right span. `BoundBilledLlm` does gate → call →
                // record internally — no manual `record_call` afterward.
                let bound = self.llm_client.bind(Attribution {
                    user_id: session.user.id.clone(),
                    session_id: session.id.clone(),
                    job_id: step.job_id,
                    span_id: span.span_id,
                    reason: aura_llm::CallReason::Chat,
                });
                // Run the provider call. The streaming path is cancel-aware
                // internally — it stops consuming and returns whatever it
                // streamed so far. The atomic non-streaming call is raced
                // against the token so a `/stop` (or the idle reaper) aborts
                // the in-flight request by dropping it (a streaming
                // `RecordingStream` still bills its partial usage on drop).
                let (partial_usage, llm_result): (TokenUsage, aura_llm::Result<LlmResponse>) =
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
                                .map(|tc| aura_trace::LlmToolCallRecord {
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
                        let trace_tool_calls: Vec<aura_trace::LlmToolCallRecord> = response
                            .tool_calls
                            .iter()
                            .map(|tc| aura_trace::LlmToolCallRecord {
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
        for content in &drained {
            // Budgeted at the framed wire size; see `append_user_interjection`.
            self.context_manager
                .append_user_interjection(content.clone())
                .await;
        }
        info!(
            interjections = count,
            "injected mid-turn user interjection(s) before the next LLM call"
        );
        drained
    }

    /// Recall memories relevant to `query` and inject each as a framed
    /// [`aura_model::MessageSource::RecalledMemory`] row before the next LLM
    /// call. No-op when no memory is wired. Recall failure is logged and
    /// swallowed — it must never fail the turn. The impl bills its own
    /// embedding/LLM work against the minted [`Attribution`]; a `MemoryRecall`
    /// trace step marks the operation.
    async fn recall_and_inject(
        &mut self,
        query: &[ContentBlock],
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
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
        let query = query.to_vec();
        let recorder = Arc::clone(span_recorder);
        let recalled = crate::runtime::scope::with_step(
            span_recorder.as_ref(),
            job_id,
            StepKind::MemoryRecall,
            Some((cancel_token, aura_job::CancelReason::ParentCancelled)),
            move |step| async move {
                let ctx = MemoryContext::new(user_id, session_id, job_id, recorder, step);
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
        // de-duplication is the impl's job, but mem0 / openviking don't do
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
            // Synthetic JobId — `on_session_end` isn't tied to a user job; this
            // id only exists so the trace step + any billed sub-call records
            // share one key. Mirrors how compression mints its own ids for
            // maintenance work.
            let job_id = JobId::new();
            let ctx_recorder = Arc::clone(&recorder);
            let result = crate::runtime::scope::with_step(
                recorder.as_ref(),
                job_id,
                StepKind::MemoryWrite,
                None,
                move |step| async move {
                    let ctx = MemoryContext::new(user_id, session_id, job_id, ctx_recorder, step);
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

    /// Fire-and-forget the [`Memory::on_job_complete`] write for a finished
    /// exchange. Detached so the actor returns the answer without waiting on
    /// the memory write; the impl bills its work against the minted
    /// [`Attribution`] under a `MemoryWrite` trace step. No-op when no memory
    /// is wired (`memory == None`).
    ///
    /// Free-standing (no `&self`) and takes owned `user_id` / `session_id`
    /// so `run()` can call it AFTER `with_job` returns — the closure that
    /// drives the iteration loop moves `&mut self` + `&mut session` into
    /// `with_job`'s body, so the borrow checker won't let us touch either
    /// from `run()` afterwards. Pre-extract `self.memory.clone()` and the
    /// two ids before the closure, then call this with the owned values.
    fn spawn_job_complete_write(
        memory: Option<Arc<dyn Memory>>,
        user_id: String,
        session_id: SessionId,
        job_id: JobId,
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
            let result = crate::runtime::scope::with_step(
                recorder.as_ref(),
                job_id,
                StepKind::MemoryWrite,
                None,
                move |step| async move {
                    let ctx = MemoryContext::new(user_id, session_id, job_id, ctx_recorder, step);
                    match memory
                        .on_job_complete(&ctx, &user_input, &final_output)
                        .await
                    {
                        Ok(()) => Ok((LifecycleOutcome::Ok, ())),
                        Err(e) => Err(anyhow::Error::new(e)),
                    }
                },
            )
            .await;
            if let Err(e) = result {
                warn!(error = %e, "memory on_job_complete write failed");
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
        // A coalesced burst can be the first thing a fresh session ever
        // appends; seed the system prompt first so it never lands *after*
        // user content. `ensure_seeded` keys off `messages[0]`, so a leading
        // user row would otherwise make every later turn re-seed.
        self.context_manager.ensure_seeded().await;
        let msg = ChatMessage::user(content);
        self.context_manager.append(&msg).await;
        Ok(())
    }

    /// Append a cron fire's framed prompt as a persisted `Cron`-source row
    /// ahead of the turn. The framing ([`aura_context::prompts::cron`]) makes
    /// the model treat the fire as a task to perform now rather than a live
    /// user message; `MessageSource::Cron` lets the operator inbox find the
    /// row. Seeds the system prompt first so a fresh cron session never lands
    /// the fire ahead of `messages[0]`.
    pub async fn append_cron_fire(&mut self, job_id: &str, prompt: &str) -> anyhow::Result<()> {
        self.context_manager.ensure_seeded().await;
        let framed = aura_context::prompts::cron::frame_cron_prompt(job_id, prompt);
        let msg = ChatMessage::cron_fire(vec![ContentBlock::Text(framed)]);
        self.context_manager.append(&msg).await;
        Ok(())
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

    /// Append the synthetic `SubagentNotification` prompt **in-memory only**.
    /// It is rebuilt from the durable `pending_background_results` buffer on
    /// every retry, so persisting per-attempt would stack duplicate hidden
    /// rows under the infinite-backoff retry. The caller seeds the system
    /// prompt and snapshots the transcript *before* this so a failed turn can
    /// roll the row back; `content` is built via
    /// [`aura_context::prompts::subagent::build_notification_content`].
    pub fn append_subagent_notification(&mut self, content: Vec<ContentBlock>) {
        // Unlike the other append_* helpers this does NOT seed the system
        // prompt: the caller must have seeded (and snapshotted) *before* this,
        // so a failed turn's rollback can't drop the system row. Self-seeding
        // here would append the system row to the tail, after the notification.
        let seeded = self
            .context_manager
            .messages()
            .first()
            .is_some_and(|m| m.role == Role::System);
        debug_assert!(
            seeded,
            "append_subagent_notification requires the system prompt already seeded \
             (call ensure_system_prompt_seeded before snapshotting)"
        );
        if !seeded {
            // Unreachable given the sole caller, but in release (debug_assert
            // compiled out) don't push the notification ahead of a not-yet-seeded
            // system row: that would leave messages[0] off the system prompt and
            // break the prompt-cache prefix. Drop the row and log loudly instead.
            error!(
                "append_subagent_notification called before the system prompt was seeded; \
                 dropping the in-memory notification to keep the transcript prefix intact"
            );
            return;
        }
        let msg = ChatMessage::agent_context(content);
        self.context_manager.append_in_memory(&msg);
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
    ) -> (TokenUsage, aura_llm::Result<LlmResponse>) {
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
    /// isn't streaming (`delta_tx` is `None`: cron / subagent).
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
                },
            })
            .await;
    }

    /// Emit a transient turn-[`AgentEvent::Status`] event (today:
    /// compaction start/end). No sanitization — the variant carries no
    /// free text. No-op when the turn isn't streaming (`delta_tx` is
    /// `None`: cron / subagent).
    async fn emit_status(
        &self,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
        session: &Session,
        status: TurnStatus,
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

    /// Compress if the budget calls for it. The `chat` closure is
    /// invoked only when the strategy returns `NeedsLlmCall`; pure
    /// strategies (Truncate, Summarize fallback) skip it entirely. The
    /// closure brackets the real LLM call in a `Compression` step +
    /// `LlmCall` span and records cost against that span — budget
    /// enforcement on the call itself rides on the wrapped client.
    ///
    /// Reports the compaction phase as `Status(Compacting)` / `Compacted`
    /// when a pass actually runs, so the user sees why the turn paused.
    async fn compress_if_needed(
        &mut self,
        session: &mut Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
        delta_tx: Option<&mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<()> {
        let runner = self.build_compression_runner(session, span_recorder, job_id, cancel_token);
        let model_id = runner.model_info.id.clone();
        // `needs_compression` mirrors `maybe_compress`'s gate, so we only
        // report the phase when a pass will actually run; the `Compacted`
        // end always follows the `Compacting` start (emitted even on a
        // compress error) so the status line never dangles.
        let compacting = self.context_manager.needs_compression(&model_id);
        if compacting {
            self.emit_status(delta_tx, session, TurnStatus::Compacting)
                .await;
        }
        let result = self
            .context_manager
            .maybe_compress(&model_id, |req, marker| async move {
                runner.run(req, marker).await.map(|run| run.response)
            })
            .await;
        if compacting {
            self.emit_status(delta_tx, session, TurnStatus::Compacted)
                .await;
        }
        result?;
        Ok(())
    }

    fn build_compression_runner(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
    ) -> CompressionRunner {
        let model_info = self.llm_client.model_info().clone();
        CompressionRunner {
            llm_client: self.llm_client.clone(),
            recorder: Arc::clone(span_recorder),
            security_gateway: Arc::clone(&self.security_gateway),
            job_id,
            user_id: session.user.id.clone(),
            session_id: session.id.clone(),
            model_info,
            cancel_token: cancel_token.clone(),
        }
    }

    /// Read-only out-of-band progress: summarize the in-flight turn with a
    /// billed LLM call and ship it as a `Notice`. The call runs detached so
    /// it never blocks the next iteration: at each boundary we first DRAIN
    /// the previous call (emit its line if it finished), then SPAWN a fresh
    /// one when the gate (`should_fire_observer`) passes and none is already
    /// in flight. No-op unless the gate passes; throttled to one attempt per
    /// `OBSERVER_MIN_INTERVAL`. At most one call is in flight at a time.
    #[allow(clippy::too_many_arguments)]
    async fn maybe_run_progress_observer(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
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
                session.trigger.kind() == aura_model::TriggerKind::User,
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
        };

        // Throttle on attempt, not just success — a failing or empty call
        // must not re-fire every iteration boundary.
        observer_state.last_fired_at = Some(now);

        // Build the runner from `&self` first; it owns / Arc-clones every
        // field, so the spawned future is `'static + Send` and borrows
        // nothing from `self`. We detach on drop and never abort: aborting
        // mid-`with_step` would leave a Pending step until boot recovery,
        // whereas the runner already threads `cancel_token` through the step
        // so a real stop closes it as Cancelled cleanly.
        let runner =
            self.build_progress_observer_runner(session, span_recorder, job_id, cancel_token);
        observer_state.in_flight = Some(tokio::spawn(async move {
            runner.run(request, input_marker).await
        }));
    }

    fn build_progress_observer_runner(
        &self,
        session: &Session,
        span_recorder: &Arc<SpanRecorder>,
        job_id: JobId,
        cancel_token: &CancellationToken,
    ) -> ProgressObserverRunner {
        ProgressObserverRunner {
            llm_client: self.llm_client.clone(),
            recorder: Arc::clone(span_recorder),
            security_gateway: Arc::clone(&self.security_gateway),
            job_id,
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

    /// Run an on-demand compression pass and return the confirmation
    /// text for the caller to ship as an `AgentEvent::Notice`.
    /// The variants of `CompressionOutcome` map to specific user-facing
    /// messages so the caller (typically a `/compact` notice) can
    /// distinguish "strategy declined", "no savings", and a real
    /// compress — instead of one generic "nothing to compress" line.
    /// A fresh job is minted so the compression step + LLM span land
    /// on a real lifecycle.
    pub async fn compact_now(
        &mut self,
        session: &mut Session,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        parent_job_id: Option<JobId>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<String> {
        // `/compact` is a user-typed command, so the input is a UserChat
        // payload regardless of the session's root trigger; the trigger
        // is recorded separately as the job's origin. It runs
        // `force_compress`, not the agent loop, so its shape is
        // `Maintenance` (like background compression) despite the
        // turn-shaped input.
        let job_input = JobInput::UserChat {
            content: vec![ContentBlock::Text(COMPACT_COMMAND.to_string())],
        };
        let spec = JobSpec {
            session_id: session.id.clone(),
            origin: session.trigger.kind(),
            shape: JobShape::Maintenance,
            input: job_input,
            parent_job_id,
        };

        crate::runtime::scope::with_job(
            job_lifecycle,
            cancel_token.clone(),
            spec,
            |job_id| async move {
                let runner =
                    self.build_compression_runner(session, span_recorder, job_id, &cancel_token);
                let model_id = runner.model_info.id.clone();
                let outcome = self
                    .context_manager
                    .force_compress(&model_id, |req, marker| async move {
                        runner.run(req, marker).await.map(|run| run.response)
                    })
                    .await?;
                let text = match outcome {
                    aura_context::CompressionOutcome::Compressed => {
                        "Context compressed.".to_string()
                    }
                    aura_context::CompressionOutcome::BelowThreshold => {
                        "Context already under the compression threshold; skipped.".to_string()
                    }
                    aura_context::CompressionOutcome::StrategyDeclined => {
                        "Compression strategy declined: nothing to summarize (conversation too short).".to_string()
                    }
                    aura_context::CompressionOutcome::NoSavings => {
                        "Compression ran but produced no savings; kept the original.".to_string()
                    }
                };
                let output = JobOutput::Message {
                    content: vec![ContentBlock::Text(text.clone())],
                };
                Ok((output, text))
            },
        )
        .await
    }

    /// Parent-side trigger gate + in-actor detached background-summary
    /// spawn. Fires at iteration boundaries and on terminal-state
    /// commit. When tokens and activity have crossed their thresholds
    /// (see [`ContextManager::maybe_request_background_summary`]) it
    /// `tokio::spawn`s a DETACHED background-summary pass attributed to
    /// **this** (parent) session. Fire-and-forget: the user's turn never
    /// blocks on it.
    ///
    /// `job_done = true` is passed at end-of-job (where the activity
    /// disjunct is trivially satisfied); `false` at iteration boundaries
    /// (where it relies on `tool_calls_since_anchor` exceeding the
    /// threshold).
    ///
    /// **At-most-one** is enforced in-memory by [`Self::bg_compression`]:
    /// if a pass is already running (handle present and not finished) we
    /// skip rather than spawn a second. No durable in-flight flag.
    ///
    /// **Detached cancel token.** The pass gets a fresh
    /// [`CancellationToken::new`] — NOT derived from the surrounding
    /// actor's token — so the idle reaper cancelling that token can't
    /// tear down an in-flight pass. Mirrors
    /// [`Self::spawn_session_end_write`].
    ///
    /// **Anchor-cursor sync** lives in
    /// `maybe_request_background_summary`: it reads
    /// `session_summaries.cursor` and `sync_anchor_to_cursor`s the
    /// in-memory anchor forward *before* measuring the anchor-relative
    /// thresholds, so a session that crossed 50% once doesn't re-fire on
    /// every later job.
    async fn maybe_run_background_compression(
        &mut self,
        session: &Session,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        current_job_id: JobId,
        job_done: bool,
    ) {
        // At-most-one: a still-running pass blocks a second.
        if let Some(handle) = self.bg_compression.as_ref()
            && !handle.is_finished()
        {
            return;
        }

        let Some(payload) = self
            .context_manager
            .maybe_request_background_summary(job_done)
            .await
        else {
            return;
        };

        // The pass writes on-disk `summary.md` + cross-session metadata,
        // so both deps are required. Unwired only in test harnesses that
        // don't exercise the pass — skip silently there.
        let (Some(workspace_paths), Some(sessions)) =
            (self.workspace_paths.clone(), self.sessions.clone())
        else {
            return;
        };

        // Pre-extract everything the 'static task needs — the spawned
        // future cannot borrow `&self` / `&session`. The pass bills +
        // traces against this session.
        let session_id = session.id.clone();
        let origin = session.trigger.kind();
        let user_id = session.user.id.clone();
        let llm_client = self.llm_client.clone();
        let security_gateway = self.security_gateway.clone();
        let tokenizer = Arc::clone(self.context_manager.tokenizer());
        let model_info = self.llm_client.model_info().clone();
        let recorder = Arc::clone(span_recorder);
        let job_lifecycle = Arc::clone(job_lifecycle);

        // Fresh, never-cancelled token — NOT a child of the actor's
        // token. The idle reaper cancels the actor token; deriving from
        // it would let a reap mid-pass tear the summary down. Mirrors
        // `spawn_session_end_write`.
        let cancel_token = CancellationToken::new();

        let handle = tokio::spawn(async move {
            // Clone the session id for the runner before the spec moves
            // it into `session_id`.
            let runner_session_id = session_id.clone();
            let spec = JobSpec {
                session_id,
                // Runs inside the triggering (User / Cron) session, so it
                // records that session's trigger as its origin.
                origin,
                // A compression pass, not an agent-loop turn.
                shape: JobShape::Maintenance,
                input: aura_job::JobInput::System {
                    payload: payload.clone(),
                },
                // Parent the maintenance job under the triggering turn's job.
                parent_job_id: Some(current_job_id),
            };
            let result = crate::runtime::scope::with_job(
                &job_lifecycle,
                cancel_token.clone(),
                spec,
                move |job_id| async move {
                    let runner = crate::runtime::compression::BackgroundCompressionRunner {
                        llm_client,
                        security_gateway,
                        sessions,
                        workspace_paths,
                        tokenizer,
                        recorder,
                        model_info,
                        session_id: runner_session_id,
                        user_id,
                        job_id,
                        cancel_token,
                    };
                    let outcome = runner.run(payload).await?;
                    let value = serde_json::to_value(&outcome)?;
                    let output = aura_job::JobOutput::Structured { value };
                    Ok((output, outcome))
                },
            )
            .await;
            if let Err(e) = result {
                warn!(error = %e, "background summary pass failed");
            }
        });
        self.bg_compression = Some(handle);
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
    use aura_channels::{AgentEvent, AgentOutput};
    use aura_tools::{NoticeLevel as ToolsNoticeLevel, SessionNotifier};

    fn mk_notifier() -> (DeltaTxNotifier, tokio::sync::mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let n = DeltaTxNotifier {
            tx,
            session_id: "s".into(),
            user_id: "u".into(),
            channel: aura_model::ChannelType::tui(),
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
                event: AgentEvent::Notice { level, text },
                session_id,
                user_id,
                ..
            } => {
                assert_eq!(level, aura_channels::NoticeLevel::Warn);
                assert_eq!(text, "summary: detail");
                assert_eq!(session_id, "s");
                assert_eq!(user_id, "u");
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
                event: AgentEvent::Notice { level, text },
                ..
            } => {
                assert_eq!(level, aura_channels::NoticeLevel::Error);
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
            channel: aura_model::ChannelType::tui(),
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
    use aura_llm::{LlmResponse, TokenUsage};
    use aura_model::ContentBlock;

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
    //! runs when an actor processes `ActorStop`. Subagent actors and
    //! `System`-triggered (background compression) sessions also stop,
    //! but their teardown is not a user-session ending — firing the hook
    //! for them would write garbage memory.
    use super::should_fire_session_end;
    use aura_model::{
        ChannelType, JobId, Lineage, LineageKind, Session, SessionId, SessionState, TriggerSource,
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
            folder_id: None,
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
            },
            None,
        );
        assert!(should_fire_session_end(&s));
    }

    #[test]
    fn skips_subagent_session() {
        let lineage = Lineage {
            parent_session_id: SessionId::from("parent"),
            parent_job_id: JobId::new(),
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
}

#[cfg(test)]
mod bg_compression_at_most_one_tests {
    //! Focused coverage of the at-most-one gate in
    //! [`super::AgentLoop::maybe_run_background_compression`]:
    //!
    //! ```ignore
    //! if let Some(handle) = self.bg_compression.as_ref()
    //!     && !handle.is_finished() { return; }   // skip — pass already running
    //! ```
    //!
    //! Wiring a full `AgentLoop` (LLM pool, tool registry/executor,
    //! `ContextManager`, span recorder, …) into a unit test is
    //! disproportionately heavy, so this asserts the load-bearing
    //! predicate directly against real `tokio::task::JoinHandle`s — a
    //! present, not-yet-finished handle blocks; a finished or absent one
    //! lets the spawn through. The end-to-end "no second maintenance
    //! session" behavior is covered in
    //! `integration-tests/tests/background_compression_e2e.rs`.

    use tokio::sync::oneshot;

    /// Mirror of the gate: returns `true` when a NEW pass may be spawned.
    fn may_spawn(handle: &Option<tokio::task::JoinHandle<()>>) -> bool {
        !matches!(handle.as_ref(), Some(h) if !h.is_finished())
    }

    #[tokio::test]
    async fn none_handle_allows_spawn() {
        let handle: Option<tokio::task::JoinHandle<()>> = None;
        assert!(may_spawn(&handle), "no in-flight pass ⇒ spawn allowed");
    }

    #[tokio::test]
    async fn unfinished_handle_blocks_second_spawn() {
        // A task parked on a oneshot stays unfinished until we release it.
        let (tx, rx) = oneshot::channel::<()>();
        let handle = Some(tokio::spawn(async move {
            let _ = rx.await;
        }));
        assert!(
            !may_spawn(&handle),
            "an unfinished in-flight pass must block a second spawn"
        );
        // Release so the parked task can complete.
        let _ = tx.send(());
    }

    #[tokio::test]
    async fn finished_handle_allows_spawn() {
        // Spawn a trivial task and await it so the handle is observably
        // finished, then re-check via a fresh `is_finished()` read. We
        // keep the awaited handle (await on `&mut`) so `may_spawn` can
        // inspect the same, now-finished, handle.
        let mut h = tokio::spawn(async {});
        (&mut h).await.unwrap();
        let handle = Some(h);
        assert!(
            may_spawn(&handle),
            "a finished pass must not block the next spawn"
        );
    }
}
