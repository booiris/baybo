use std::sync::Arc;

use aura_channels::{
    AgentEvent, AgentOutput, COMPACT_COMMAND, OutgoingMessage, ToolStatus, TurnStatus,
};
use aura_context::ContextManager;
use aura_job::{JobInput, JobLifecycle, JobOutput};
use aura_llm::{
    Attribution, BillableLlm, BoundBilledLlm, ChatRequest, LlmResponse, StreamEvent, TokenUsage,
    ToolDefinitionForLlm,
};
use aura_memory::{Memory, MemoryContext};
use aura_model::{
    ChatMessage, ContentBlock, JobId, LlmEntryName, MessageSource, Role, SessionId,
    SystemSpawnRequest, ThinkingContent,
};
use futures::StreamExt;
use tokio::sync::mpsc;

use aura_model::{LineageKind, Session, TriggerSource};
use aura_tools::{ToolOutput, ToolRegistry};
use aura_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanRecorder, StepHandle, StepKind,
};
use tracing::{debug, error, info, warn};

use crate::runtime::compression::CompressionRunner;
use crate::runtime::error_recovery::ErrorHandler;
use crate::runtime::progress_observer::{
    PROGRESS_OBSERVER_PROMPT, ProgressObserverRunner, should_fire_observer,
};
use crate::runtime::scope::JobSpec;
use aura_context::{LlmCallOutcome, LlmResponseMeta};

use crate::runtime::tool_executor::ToolExecutor;
use crate::security::SecurityGateway;
use tokio_util::sync::CancellationToken;

/// The maximum amount of text we'll hold in the streaming buffer waiting
/// for a placeholder to complete. If a chunk ends with an open `[{` but no
/// closing `}]` arrives within this many bytes, we flush anyway — no real
/// placeholder is this long, so holding further would be a DoS vector.
const STREAM_BUFFER_HIGH_WATER: usize = 128;

/// Cap on attachments carried into the final `OutgoingMessage`. Tools
/// like `browser_screenshot` and `send_local_file` push into a
/// per-turn `accumulated_attachments` vec; without a cap a chatty
/// agent (multi-iteration browse, page-each screenshot, …) would
/// dump every blob produced over the loop into one channel message.
/// 16 is well above any plausible "user asked for N artifacts" case
/// and small enough that a runaway loop doesn't drown the channel.
/// FIFO eviction: keep the newest, drop the oldest — older
/// screenshots from earlier iterations are typically stale anyway
/// (page state has moved on).
const MAX_ATTACHMENTS_PER_TURN: usize = 16;

/// Max characters in a `ToolCompleted` progress summary before it is
/// truncated with an ellipsis. Presentation-only — the full result still
/// reaches the LLM (capped separately) and the trace.
const TOOL_SUMMARY_MAX: usize = 80;

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

/// Append `items` into `dst` while keeping `dst.len() <=
/// MAX_ATTACHMENTS_PER_TURN` via FIFO eviction. Used for the
/// per-turn `accumulated_attachments` vec so a runaway loop can't
/// drown the final `OutgoingMessage` in stale screenshots.
fn push_bounded<I: IntoIterator<Item = ContentBlock>>(dst: &mut Vec<ContentBlock>, items: I) {
    for item in items {
        if dst.len() >= MAX_ATTACHMENTS_PER_TURN {
            // Stable order: channels render attachments in arrival
            // order, so FIFO eviction keeps the surviving set
            // chronologically coherent.
            dst.remove(0);
        }
        dst.push(item);
    }
}

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
}

/// What one `LlmIteration` step's body produced. The terminal-vs-loop
/// distinction lives here (rather than the body short-circuiting via
/// `?`) so the `with_step` wrapper sees a clean `Ok(...)` either way
/// and closes the step before the parent loop runs the next thing.
enum IterationOutcome {
    /// Final assistant response — caller returns this from `run_inner`.
    /// `outgoing` is the channel-bound message (may include intermediate
    /// `ToolUse` blocks + channel-only attachments concatenated after the
    /// reply); `assistant_reply` is the actual persisted assistant turn
    /// (text / thinking blocks only — what `ChatMessage::assistant(...)`
    /// was constructed with). The split exists so `Memory::on_job_complete`
    /// can see the assistant's last turn per its trait contract instead of
    /// the channel-augmented view.
    Final {
        outgoing: OutgoingMessage,
        assistant_reply: Vec<ContentBlock>,
    },
    /// LLM emitted tool calls; loop continues.
    Continue,
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
/// would call "theirs" — root `User`/`Cron` sessions and `UserFork` branches.
/// Subagent, `SystemMaintenance`, and `System`-triggered (background
/// compression) actors all send `ActorStop` when they finish, but their
/// shutdown is not a user-session ending. Exhaustive arms force a
/// classification when a new `TriggerSource` / `LineageKind` variant is added.
fn should_fire_session_end(session: &Session) -> bool {
    let user_trigger = match &session.trigger {
        TriggerSource::User | TriggerSource::Cron { .. } => true,
        TriggerSource::System { .. } => false,
    };
    let user_lineage = match &session.lineage {
        None => true,
        Some(l) => match &l.kind {
            LineageKind::UserFork { .. } => true,
            LineageKind::Subagent | LineageKind::SystemMaintenance => false,
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
    /// Lifetime token for the surrounding `AgentActor`. The spawner
    /// factory derives the token once and threads it into both this
    /// loop and the actor. The summary-refresh trigger gate clones it
    /// into outgoing `SystemSpawnRequest`s so cancellation cascades
    /// from the parent actor into its maintenance children
    /// automatically.
    actor_token: CancellationToken,
    /// Sender half of the generic system-spawn channel. The router
    /// consumes the receiving half. Today only the summary-refresh
    /// trigger gate emits on it; future system tasks (history review,
    /// memory consolidation, ...) will share the same channel via
    /// other `SystemSpawnRequest` variants. `None` disables every
    /// system-trigger gate that gates on it.
    system_spawn_tx: Option<mpsc::Sender<SystemSpawnRequest>>,
    /// Resolved workspace paths. Today only the summary-refresh
    /// maintenance handler reads it (to write `summary.md`); other
    /// future system handlers may want it too. `None` in tests that
    /// don't exercise such handlers.
    workspace_paths: Option<Arc<aura_workspace::WorkspacePaths>>,
    /// Cross-session manager — used by handlers that operate across
    /// sessions (today: background summary, for transcript loads,
    /// in-flight maintenance lookups, summary metadata writes).
    /// Distinct from the `SessionManager` plumbed inside
    /// `ContextManager` because that one is per-session-bound.
    sessions: Option<Arc<crate::SessionManager>>,
    /// Pluggable long-term memory. `None` disables every memory hook (recall,
    /// `on_job_complete`) — the runtime wires `None` until a real
    /// implementation is registered.
    memory: Option<Arc<dyn Memory>>,
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
    /// Lifetime token for the surrounding `AgentActor`. The spawner
    /// factory derives the actor token once and threads the same
    /// handle into both this loop and the actor.
    pub actor_token: CancellationToken,
    /// Generic system-spawn channel sender (any
    /// `SystemSpawnRequest` variant — today background summary).
    pub system_spawn_tx: Option<mpsc::Sender<SystemSpawnRequest>>,
    /// Workspace paths. Used by system handlers that touch on-disk
    /// state.
    pub workspace_paths: Option<Arc<aura_workspace::WorkspacePaths>>,
    /// Cross-session manager. Used by system handlers that operate
    /// across sessions.
    pub sessions: Option<Arc<crate::SessionManager>>,
    /// Pluggable long-term memory handle — one registered implementation, or
    /// `None` to disable the memory hooks (recall / `on_job_complete`).
    pub memory: Option<Arc<dyn Memory>>,
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
            actor_token,
            system_spawn_tx,
            workspace_paths,
            sessions,
            memory,
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
            actor_token,
            system_spawn_tx,
            workspace_paths,
            sessions,
            memory,
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
            session_trigger_kind: session.trigger.kind(),
            input: job_input,
            effective_soul_version: session.bound_soul_version.clone(),
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
        let _ = job_lifecycle;
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
        let mut last_observer_at: Option<std::time::Instant> = None;
        // Tool invocations issued during intermediate iterations. Channels
        // only receive the terminal `OutgoingMessage`, so without this the
        // TUI (which renders `ContentBlock::ToolUse`) would never see tool
        // activity — breaking any channel-side behavior keyed on the tool
        // call (e.g. the TUI cron-recurring hint).
        let mut accumulated_tool_uses: Vec<ContentBlock> = Vec::new();
        // Side-channel attachments emitted by tools (e.g. send_local_file).
        // Hoisted into the final OutgoingMessage so the channel sidecar
        // delivers them out-of-band; never echoed back to the LLM.
        let mut accumulated_attachments: Vec<ContentBlock> = Vec::new();
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
                self.maybe_spawn_background_compression(job_id, /* job_done */ false)
                    .await;
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
                &mut last_observer_at,
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
                        &mut accumulated_tool_uses,
                        &mut accumulated_attachments,
                    );
                    async move { Ok((LifecycleOutcome::Ok, fut.await?)) }
                },
            )
            .await?;

            match outcome {
                IterationOutcome::Final {
                    outgoing,
                    assistant_reply,
                } => {
                    // End-of-job summary-refresh check. The activity
                    // disjunct is satisfied by `job_done = true`;
                    // the tokens / diff conjuncts still apply.
                    self.maybe_spawn_background_compression(job_id, /* job_done */ true)
                        .await;
                    // Capture the memory write inputs and return them up to
                    // `run()` — the actual `spawn_job_complete_write` fires
                    // **after** `with_job` accepts the job, so a cancel-race
                    // in `with_job`'s post-body window can't memorize a
                    // cancelled turn. `assistant_reply` (not `outgoing.content`)
                    // is what gets persisted as the assistant row, matching
                    // the trait contract.
                    let pending = memory_query.is_some().then(|| PendingMemoryWrite {
                        user_input: std::mem::take(&mut job_user_input),
                        final_output: assistant_reply,
                    });
                    return Ok((outgoing, pending));
                }
                IterationOutcome::Continue => {
                    // Continue to the next LLM iteration; `run_iteration`
                    // has already appended each tool's result to the
                    // context.
                }
            }
        }

        // If we exhausted iterations, return what we have. Tail-append any
        // attachments the tools produced so the user still receives the
        // file even when the agent ran out of reasoning budget.
        let mut content = vec![ContentBlock::Text(
            "I've reached the maximum number of processing steps. Please try again with a simpler request.".to_string(),
        )];
        content.extend(std::mem::take(&mut accumulated_attachments));
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
    /// the caller). Calls the LLM, executes any non-subagent tool
    /// calls, appends their results to context. `spawn_subagent` calls
    /// are deferred — returned in [`IterationOutcome::Continue`] so
    /// the caller can dispatch them as **peer** steps once `with_step`
    /// has closed this iteration's step (steps cannot nest per
    /// `trace.md`).
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
        accumulated_tool_uses: &mut Vec<ContentBlock>,
        accumulated_attachments: &mut Vec<ContentBlock>,
    ) -> anyhow::Result<IterationOutcome> {
        let (response, llm_span_id) = self
            .call_llm_with_retry(session, span_recorder, &step, delta_tx)
            .await?;

        // If no tool calls, we have the final response.
        if response.tool_calls.is_empty() {
            // Use content_blocks when available, falling back to the
            // text string.
            let response_blocks = if response.content_blocks.is_empty() {
                vec![ContentBlock::Text(response.content.clone())]
            } else {
                response.content_blocks.clone()
            };

            // Append the tool_use blocks issued during intermediate
            // iterations after the final narration so channels that key
            // off them (e.g. the TUI cron hint) can render below the
            // assistant's reply.
            let mut final_blocks = response_blocks.clone();
            final_blocks.extend(std::mem::take(accumulated_tool_uses));
            final_blocks.extend(std::mem::take(accumulated_attachments));

            let final_text = aura_llm::multimodal::extract_text(&response_blocks);

            info!(
                iterations,
                content_len = final_text.len(),
                "conversation loop complete"
            );

            // Append only the final response blocks to context —
            // intermediate tool_use blocks were already appended in
            // prior iterations. Capture the persisted ordinal so the
            // channel adapter can stamp it onto the live `Frame::Message`
            // and reconnecting clients advance their cursor past it.
            // Also keep a copy for `IterationOutcome::Final.assistant_reply`
            // so `Memory::on_job_complete` sees the same shape that hits
            // the transcript, not the channel-augmented `final_blocks`.
            let assistant_reply = response_blocks.clone();
            let assistant_msg = ChatMessage::assistant(response_blocks);
            let ordinal = self.context_manager.append(&assistant_msg).await;

            return Ok(IterationOutcome::Final {
                outgoing: OutgoingMessage {
                    session_id: session.id.clone(),
                    user_id: session.user.id.clone(),
                    channel: session.channel.clone(),
                    content: final_blocks,
                    reply_to: None,
                    metadata: Default::default(),
                    ordinal,
                },
                assistant_reply,
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
            let block = ContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
                signature: tc.signature.clone(),
            };
            assistant_blocks.push(block.clone());
            accumulated_tool_uses.push(block);
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

        // Execute tool calls concurrently. Approved resources are shared
        // via a Mutex so concurrent calls see each other's grants
        // immediately. Wrapped in an `Arc` so that any persist-always
        // closure injected into `ToolContext` mid-execution can clone
        // its handle into the executor boundary without a borrow
        // escape.
        //
        // Concurrency note: every tool call inside a single LLM
        // response runs in parallel via `futures::future::join_all`.
        // The post-execution pass that mutates `session.messages` is
        // kept SEQUENTIAL — appending tool results in original
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
        let recorder_for_calls = Arc::clone(span_recorder);
        let step_for_calls = step.clone();
        let notifier_for_calls = notifier.clone();
        let llm_for_calls = Arc::clone(&self.llm_client);
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
            async move {
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

            let mut llm_visible_images: Vec<ContentBlock> = Vec::new();
            let raw_result_text = match &tool_result {
                Ok(ToolOutput::Text(s)) => s.clone(),
                Ok(ToolOutput::Json(v)) => {
                    serde_json::to_string(v).unwrap_or_else(|_| v.to_string())
                }
                Ok(ToolOutput::WithAttachments { text, attachments }) => {
                    push_bounded(accumulated_attachments, attachments.iter().cloned());
                    text.clone()
                }
                Ok(ToolOutput::MultiModalText { text, llm_images }) => {
                    // LLM-visible images go in BOTH directions: a
                    // follow-up User-role message (so the next turn
                    // sees them through the standard multimodal user
                    // path) AND the final OutgoingMessage (so the user
                    // channel renders them too).
                    push_bounded(accumulated_attachments, llm_images.iter().cloned());
                    llm_visible_images.extend(llm_images.iter().cloned());
                    text.clone()
                }
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

            // ToolResult.content is text-only; provider adapters
            // serialize it as plain text. To get images back into the
            // LLM's view, follow with a User-role message that carries
            // the same images plus a marker tying them to this tool
            // call. Vision-capable providers fetch the blob bytes via
            // the existing user_content_for_block path; non-vision
            // providers fall back to a text stub.
            if !llm_visible_images.is_empty() {
                let mut content: Vec<ContentBlock> =
                    Vec::with_capacity(llm_visible_images.len() + 1);
                content.push(ContentBlock::Text(format!(
                    "[image attachment(s) returned by tool `{}` (tool_use_id={})]",
                    tool_call.name, tool_call.id
                )));
                content.extend(llm_visible_images);
                let image_msg = ChatMessage::agent_context(content);
                self.context_manager.append(&image_msg).await;
            }
        }

        // Flush accumulated approvals back into session state.
        session.state.approved_resources = approved.lock().clone();

        Ok(IterationOutcome::Continue)
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
    ) -> anyhow::Result<(LlmResponse, aura_model::SpanId)> {
        let mut attempt = 0u32;
        loop {
            match self.call_llm(session, span_recorder, step, delta_tx).await {
                Ok(pair) => return Ok(pair),
                Err(e) => {
                    if !self.error_handler.should_retry(attempt, &e) {
                        return Err(e);
                    }
                    let backoff = self.error_handler.backoff_duration(attempt);
                    warn!(
                        attempt = attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e,
                        "retrying LLM call after transient error"
                    );
                    tokio::time::sleep(backoff).await;
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
            None,
            |span| async move {
                let started_at = std::time::Instant::now();
                // Bind this call to its `LlmCall` span so the spend lands
                // on the right span. `BoundBilledLlm` does gate → call →
                // record internally — no manual `record_call` afterward.
                let bound = self.llm_client.bind(Attribution {
                    user_id: session.user.id.clone(),
                    session_id: session.id.clone(),
                    job_id: step.job_id,
                    span_id: span.span_id,
                });
                let (partial_usage, llm_result) = match delta_tx {
                    Some(tx) => self.chat_streaming(&bound, &request, session, tx).await,
                    None => match bound.chat(&request).await {
                        Ok(billed) => (billed.response.usage, Ok(billed.response)),
                        Err(e) => (TokenUsage::default(), Err(e)),
                    },
                };
                let latency_ms = started_at.elapsed().as_millis() as u64;

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
                        self.write_session_log(
                            &request,
                            LlmCallOutcome::Ok {
                                response: LlmResponseMeta::from_response(&response),
                                latency_ms,
                            },
                        )
                        .await;
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
                        // Sanitize the JSONL log; return the raw typed
                        // `LlmError` so `should_retry` can dispatch on
                        // the variant. The trace's `Failed` reason
                        // still carries the provider text.
                        let raw = e.to_string();
                        let log_msg = self
                            .security_gateway
                            .sanitize_error(&raw)
                            .await
                            .unwrap_or(raw);
                        self.write_session_log(
                            &request,
                            LlmCallOutcome::Err {
                                error: log_msg,
                                latency_ms,
                            },
                        )
                        .await;
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

    /// Log a single LLM call to the per-session JSONL log. Context owns the
    /// logger + record format and assembles + writes it; the agent only
    /// supplies the model identity (from its `BillableLlm`, which context
    /// doesn't hold) plus the call's request + outcome.
    async fn write_session_log(&self, request: &ChatRequest, outcome: LlmCallOutcome) {
        let info = self.llm_client.model_info();
        self.context_manager
            .log_llm_call(request, outcome, &info.provider, &info.id)
            .await;
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
    /// user-facing per [`should_fire_session_end`] (subagents, maintenance,
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
    /// It is rebuilt from the durable `pending_subagent_results` buffer on
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
    async fn chat_streaming(
        &self,
        bound: &BoundBilledLlm,
        request: &ChatRequest,
        session: &Session,
        delta_tx: &mpsc::Sender<AgentOutput>,
    ) -> (TokenUsage, aura_llm::Result<LlmResponse>) {
        let mut stream = match bound.chat_stream(request).await {
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

        while let Some(event) = stream.next().await {
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
            .maybe_compress(&model_id, |req| async move {
                runner.run(req).await.map(|run| run.response)
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
    /// billed LLM call and ship it as a `Notice`. No-op unless the gate
    /// (`should_fire_observer`) passes; throttled to one per
    /// `OBSERVER_MIN_INTERVAL`.
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
        last_observer_at: &mut Option<std::time::Instant>,
    ) {
        let now = std::time::Instant::now();
        if cancel_token.is_cancelled()
            || !should_fire_observer(
                delta_tx.is_some(),
                session.trigger.kind() == aura_model::TriggerKind::User,
                iterations,
                turn_started,
                *last_observer_at,
                now,
            )
        {
            return;
        }

        // Reuse the main call's prefix (cache hit) + a summarize turn; no
        // tools, so the observer only narrates.
        let mut messages = self.context_manager.messages_for_llm();
        messages.push(ChatMessage::user(vec![ContentBlock::Text(
            PROGRESS_OBSERVER_PROMPT.to_string(),
        )]));
        let request = ChatRequest {
            messages,
            temperature: None,
            tools: Vec::new(),
        };

        // Throttle on attempt, not just success — a failing or empty call
        // must not re-fire every iteration boundary.
        *last_observer_at = Some(now);

        let runner =
            self.build_progress_observer_runner(session, span_recorder, job_id, cancel_token);
        match runner.run(request).await {
            Ok(text) if !text.trim().is_empty() => {
                // Re-check cancel: the call may have raced a preempt/stop.
                if !cancel_token.is_cancelled() {
                    self.emit_progress_notice(delta_tx, session, text).await;
                }
            }
            Ok(_) => {}
            Err(e) => debug!(error = %e, "progress observer call failed; skipping this tick"),
        }
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

    /// Emit the observer's one-line summary as an `Info` Notice. The text
    /// is already scrubbed by the runner (`sanitize_llm_response`), so
    /// this only routes it. No-op when the turn isn't streaming.
    async fn emit_progress_notice(
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
                event: AgentEvent::Notice {
                    level: aura_channels::NoticeLevel::Info,
                    text,
                },
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
        // Match the session trigger so the JobKind invariant holds for
        // both user-triggered and spawned sessions. `/compact` is a
        // user-typed command, so UserChat is the natural default; for
        // sessions whose root trigger is anything else (Cron / System
        // / Spawned) we fall back to JobKind::Spawned which is allowed
        // under every trigger.
        let job_input = match session.trigger.kind() {
            aura_model::TriggerKind::User => JobInput::UserChat {
                content: vec![ContentBlock::Text(COMPACT_COMMAND.to_string())],
            },
            _ => JobInput::Spawned {
                initial_prompt: vec![ContentBlock::Text(COMPACT_COMMAND.to_string())],
            },
        };
        let spec = JobSpec {
            session_id: session.id.clone(),
            session_trigger_kind: session.trigger.kind(),
            input: job_input,
            effective_soul_version: session.bound_soul_version.clone(),
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
                    .force_compress(&model_id, |req| async move {
                        runner.run(req).await.map(|run| run.response)
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

    /// Parent-side trigger gate. Fires at iteration boundaries and
    /// on terminal-state commit. When tokens and activity have
    /// crossed their thresholds and no maintenance session is
    /// already in flight for this parent, sends a
    /// `SystemSpawnRequest::BackgroundCompression` on the system-spawn
    /// channel for the router to materialise into a maintenance
    /// session + actor. Fire-and-forget — a full or closed channel
    /// never blocks the user's turn.
    ///
    /// `job_done = true` is passed at end-of-job (where the
    /// activity disjunct is trivially satisfied); `false` at
    /// iteration boundaries (where it relies on
    /// `tool_calls_since_anchor` exceeding the threshold).
    ///
    /// **Anchor-cursor sync (lazy pull).** The in-memory
    /// `last_summary_anchor` is only advanced by inline-compression
    /// applies. A successful background pass writes a fresh
    /// `session_summaries.cursor` but doesn't touch this loop's
    /// state — without intervention `tokens_since_anchor` would keep
    /// reporting the same delta and the gate would re-spawn a fresh
    /// pass on every later job. The fix is to read the metadata
    /// **once per evaluation** (we already need the same row for
    /// the `in_flight` check) and use its cursor to advance the
    /// in-memory anchor before measuring `tokens_since_anchor` /
    /// `tool_calls_since_anchor`. `sync_anchor_to_cursor` is
    /// monotonic, so a stale cursor or one already inside a
    /// rewritten slice is a no-op.
    async fn maybe_spawn_background_compression(
        &mut self,
        current_job_id: aura_model::JobId,
        job_done: bool,
    ) {
        let Some(tx) = self.system_spawn_tx.as_ref() else {
            return;
        };
        let tx = tx.clone();
        let actor_token = self.actor_token.clone();

        self.context_manager
            .maybe_request_background_summary(job_done, move |payload| {
                let request = SystemSpawnRequest::BackgroundCompression {
                    parent_session_id: payload.parent_session_id.clone(),
                    parent_job_id: current_job_id,
                    parent_actor_token: actor_token,
                    payload,
                };
                // `TrySendError<SystemSpawnRequest>` carries the
                // request back as its payload — large enough to trip
                // clippy's `result_large_err`. The gate only needs the
                // Display message for its rollback warn, so flatten
                // here.
                tx.try_send(request).map_err(|e| e.to_string())
            })
            .await;
    }

    /// Maintenance entry point — runs one async summary-refresh
    /// pass on behalf of the parent session named in `payload`. The
    /// surrounding actor (a System session with
    /// `LineageKind::SystemMaintenance`) invokes this from its
    /// `AgentMessage::SystemTrigger` mailbox handler, bypassing the
    /// normal chat-turn `run` cycle entirely. Wraps the LLM call in
    /// a `JobInput::System { reason: BackgroundCompression }` job so cost
    /// + trace are properly attributed.
    ///
    /// Errors when `workspace_paths` or `sessions` are unwired (test
    /// harnesses) — production bootstrap always sets both.
    pub(crate) async fn run_background_compression(
        &mut self,
        session: &mut Session,
        payload: aura_model::BackgroundCompressionPayload,
        job_lifecycle: &Arc<JobLifecycle>,
        span_recorder: &Arc<SpanRecorder>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<aura_context::BackgroundSummaryOutcome> {
        let workspace_paths = self
            .workspace_paths
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("workspace_paths not configured for background summary")
            })?
            .clone();
        let sessions = self
            .sessions
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sessions not configured for background summary"))?
            .clone();
        // Held for the post-`with_job` defensive cleanup that
        // guarantees `in_flight` is cleared even when the runner
        // returns Err before reaching `record_summary_*` (cancel,
        // job-lifecycle rejection, transcript load failure). The
        // cleanup is gated on the owner token so a stale Pass A
        // finishing after Pass B already remarked the parent cannot
        // wipe Pass B's mark.
        let cleanup_sessions = sessions.clone();
        let parent_id_for_cleanup = payload.parent_session_id.clone();
        let cleanup_owner = payload.in_flight_owner.clone();
        let llm_client = self.llm_client.clone();
        let security_gateway = self.security_gateway.clone();
        let tokenizer = Arc::clone(self.context_manager.tokenizer());
        let model_info = self.llm_client.model_info().clone();
        let user_id = session.user.id.clone();
        let maintenance_session_id = session.id.clone();
        let recorder = span_recorder.clone();

        let spec = JobSpec {
            session_id: session.id.clone(),
            session_trigger_kind: session.trigger.kind(),
            input: aura_job::JobInput::System {
                payload: payload.clone(),
            },
            effective_soul_version: session.bound_soul_version.clone(),
            parent_job_id: session.lineage.as_ref().map(|l| l.parent_job_id),
        };

        let result = crate::runtime::scope::with_job(
            job_lifecycle,
            cancel_token.clone(),
            spec,
            move |job_id| {
                let payload = payload.clone();
                let cancel_token = cancel_token.clone();
                async move {
                    let refresher = crate::runtime::compression::BackgroundCompressionRunner {
                        llm_client,
                        security_gateway,
                        sessions,
                        workspace_paths,
                        tokenizer,
                        recorder,
                        model_info,
                        maintenance_session_id,
                        maintenance_user_id: user_id,
                        job_id,
                        cancel_token,
                    };
                    let outcome = refresher.run(payload).await?;
                    let value = serde_json::to_value(&outcome)?;
                    let output = aura_job::JobOutput::Structured { value };
                    Ok((output, outcome))
                }
            },
        )
        .await;

        // Defense in depth: CAS-clear `in_flight` after the runner
        // returns. Successful and failed passes already clear the
        // flag via `record_summary_success`/`record_summary_failure`
        // (which also nulls `in_flight_owner`), so this owned-clear
        // is a no-op on those paths. It only fires for runner exits
        // that bypass `record_*` (cancellation before the runner
        // started, job-lifecycle rejection, mid-await drop). The
        // owner-token gate keeps a stale cleanup from this pass from
        // wiping a fresher pass' mark.
        match cleanup_sessions
            .clear_summary_in_flight_if_owned(&parent_id_for_cleanup, &cleanup_owner)
            .await
        {
            Ok(true) => {
                debug!(
                    parent_session_id = %parent_id_for_cleanup,
                    owner_token = %cleanup_owner,
                    "background summary: defensive in_flight clear fired (runner bypassed record_*)"
                );
            }
            Ok(false) => {
                // Either record_* already cleared it, or a fresher
                // pass took ownership — both are correct outcomes.
            }
            Err(e) => {
                warn!(
                    parent_session_id = %parent_id_for_cleanup,
                    error = %e,
                    "background summary: clear_summary_in_flight_if_owned after runner failed"
                );
            }
        }

        result
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
mod attachment_bound_tests {
    use super::{MAX_ATTACHMENTS_PER_TURN, push_bounded};
    use aura_model::{BlobRef, ContentBlock};

    fn img(tag: &str) -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: format!("sha256:{tag}"),
            },
            mime_type: "image/png".into(),
        }
    }

    #[test]
    fn under_cap_keeps_everything() {
        let mut v: Vec<ContentBlock> = Vec::new();
        push_bounded(&mut v, vec![img("a"), img("b"), img("c")]);
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn over_cap_evicts_oldest_first() {
        let mut v: Vec<ContentBlock> = Vec::new();
        let pushed: Vec<ContentBlock> = (0..MAX_ATTACHMENTS_PER_TURN + 5)
            .map(|i| img(&format!("{i:0>2}")))
            .collect();
        push_bounded(&mut v, pushed);
        assert_eq!(v.len(), MAX_ATTACHMENTS_PER_TURN);
        // Newest 16 survive; the first 5 (00..04) evicted.
        match &v[0] {
            ContentBlock::Image { blob, .. } => {
                assert!(
                    blob.blob_id.ends_with("05"),
                    "oldest survivor should be 05, got {}",
                    blob.blob_id
                );
            }
            _ => panic!("expected image"),
        }
        match v.last().unwrap() {
            ContentBlock::Image { blob, .. } => {
                let last = format!("{:0>2}", MAX_ATTACHMENTS_PER_TURN + 4);
                assert!(
                    blob.blob_id.ends_with(&last),
                    "newest should be {last}, got {}",
                    blob.blob_id
                );
            }
            _ => panic!("expected image"),
        }
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
    //! runs when an actor processes `ActorStop`. Subagent /
    //! `SystemMaintenance` actors and `System`-triggered (background
    //! compression) sessions also stop, but their teardown is not a
    //! user-session ending — firing the hook for them would write
    //! garbage memory.
    use super::should_fire_session_end;
    use aura_model::{
        ChannelType, JobId, Lineage, LineageKind, Session, SessionId, SessionState, SystemReason,
        TriggerSource, User,
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
            bound_soul_version: "soul-gate".into(),
            hidden: false,
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
    fn fires_for_user_fork_branch() {
        let lineage = Lineage {
            parent_session_id: SessionId::from("parent"),
            parent_job_id: JobId::new(),
            parent_span_id: None,
            kind: LineageKind::UserFork {
                fork_at_job_id: JobId::new(),
                prefix_state_hash: "deadbeef".into(),
            },
        };
        assert!(should_fire_session_end(&session_with(
            TriggerSource::User,
            Some(lineage)
        )));
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

    #[test]
    fn skips_system_maintenance_session() {
        let lineage = Lineage {
            parent_session_id: SessionId::from("parent"),
            parent_job_id: JobId::new(),
            parent_span_id: None,
            kind: LineageKind::SystemMaintenance,
        };
        let s = session_with(
            TriggerSource::System {
                reason: SystemReason::BackgroundCompression,
            },
            Some(lineage),
        );
        assert!(!should_fire_session_end(&s));
    }

    #[test]
    fn skips_root_system_session() {
        // No lineage but System trigger → still a maintenance-class actor
        // (a hypothetical future System variant without SystemMaintenance
        // lineage), not a user session.
        let s = session_with(
            TriggerSource::System {
                reason: SystemReason::BackgroundCompression,
            },
            None,
        );
        assert!(!should_fire_session_end(&s));
    }
}
