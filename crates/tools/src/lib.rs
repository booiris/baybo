pub mod approval;
pub mod builtin;
pub mod error;
pub mod mcp;
pub mod progress;
pub mod read_tracker;
pub mod registry;
pub mod virtual_read;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
#[cfg(any(test, feature = "test-support"))]
use baybo_model::ChannelType;
use baybo_model::{AgentProfileId, SessionId, SpanId, TriggerSource, TurnId, User};
use baybo_trace::ToolEventPayload;
use baybo_workspace::WorkspacePaths;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use approval::{
    ApprovalDecision, ApprovalGate, ApprovalGateMap, ApprovalQueue, ApprovalRequest,
    ApprovedResource, AutoDenyGate, ChannelApprovalGate, HostPattern, PendingEdge, PendingWatcher,
    ResourceAccess,
};
pub(crate) use baybo_model::FileFingerprint;
pub use builtin::read::READ_TOOL_NAME;
pub use builtin::read::{paginate_end_line, paginate_numbered};
pub use error::ToolError;
pub use read_tracker::ReadTracker;
pub use virtual_read::{VirtualReadAccess, VirtualReadResolver, VirtualReadWindow};

pub type Result<T> = std::result::Result<T, ToolError>;

/// Whether a tool is safe to run concurrently with the sibling tool
/// calls in the same LLM response.
///
/// The agent loop dispatches every tool call of one response together
/// under a per-response permit pool.
/// A [`Concurrent`](ToolConcurrency::Concurrent) call may overlap other
/// `Concurrent` calls — the loop runs at most a fixed number at once. An
/// [`Exclusive`](ToolConcurrency::Exclusive) call runs alone: it waits
/// for any in-flight pool calls to drain, then blocks every other pool
/// call (read or write) until it returns, so a tool that mutates shared
/// state never races a concurrent reader. An
/// [`Independent`](ToolConcurrency::Independent) call opts out of the
/// pool entirely and governs its own concurrency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConcurrency {
    /// Safe to run alongside other `Concurrent` calls — a read-only tool
    /// that touches no shared mutable state. Bounded by the agent loop's
    /// concurrency cap.
    Concurrent,
    /// Must run exclusively; no other `Concurrent`/`Exclusive` call
    /// overlaps it. The conservative default for any tool with side
    /// effects.
    Exclusive,
    /// Runs independently of the per-response permit pool: acquires no
    /// permit, so it neither waits for one nor blocks others, and can
    /// overlap even an `Exclusive` call. For tools that bound their own
    /// concurrency out-of-band — today only `spawn_subagent`, capped by
    /// its `SubagentDispatchLimiter` (per-root fan-out). A foreground
    /// subagent mostly *waits* on its child actor, so holding a shared
    /// tool slot for the child's whole lifetime would needlessly
    /// serialize the parent's fan-out and starve its other tools.
    Independent,
}

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

/// Tool trait — the unified interface for all tool implementations.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    /// LLM-facing description. Owned `String`: every consumer
    /// materialises it into a `ToolDefinition` / `ToolManifest` anyway,
    /// and tools whose description depends on runtime state (e.g.
    /// `spawn_subagent` enumerates the registered subagent profiles each
    /// turn) compute it per call. Static tools just `"...".to_string()`.
    fn description(&self) -> String;

    fn parameters_schema(&self) -> Value;

    /// Resources this call will touch, derived from the parameters.
    ///
    /// The approval gate consults these at runtime before execution.
    /// Tools with no side effects return an empty vec (the default).
    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        Vec::new()
    }

    /// Caller-supplied human-readable label for this call (typically a
    /// short summary the model writes alongside its arguments). The
    /// executor surfaces it in approval prompts and traces. Default
    /// returns `None`; tools that accept such a parameter override.
    fn call_label(&self, _params: &Value) -> Option<String> {
        None
    }

    /// Short preview of this call for the live progress line
    /// (`⏺ tool(label)` — see `docs/turn-progress-events.md`). Defaults to
    /// [`Self::call_label`] so a tool whose approval label is already a
    /// good preview reuses it (e.g. WebFetch's URL). A tool whose
    /// `call_label` is a *warning* rather than a preview — e.g. Bash,
    /// which only labels destructive commands — overrides this to return a
    /// plain preview of the action (the command) for every call.
    fn progress_label(&self, params: &Value) -> Option<String> {
        self.call_label(params)
    }

    /// Maximum wall-clock time this tool is allowed to run, declared
    /// by the tool itself. The default is 30 s; tools whose natural
    /// upper bound differs (e.g. `BashTool` for long shell commands,
    /// `WebFetchTool` for network round-trips) override this. The
    /// returned value is what the executor places into
    /// [`ToolContext::timeout`] and uses to size the outer cancel
    /// deadline.
    fn max_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    /// Whether this tool may run concurrently with the other tool calls
    /// in the same LLM response. Defaults to
    /// [`ToolConcurrency::Exclusive`] — only read-only tools that touch
    /// no shared mutable state should override to
    /// [`ToolConcurrency::Concurrent`]. See [`ToolConcurrency`] for how
    /// the agent loop schedules each variant.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Exclusive
    }

    /// Which session-trigger contexts this tool is visible in. The default
    /// [`ToolTriggerScope::Any`] is unrestricted (the norm); a tool that is
    /// only meaningful inside a specific kind of turn (e.g. `report_nothing`,
    /// which suppresses a recurring cron fire's notification) narrows it so it
    /// never appears where it cannot act. Mirrors the manifest's `channels`
    /// axis — the agent loop omits an out-of-scope tool from the LLM's list
    /// (`ToolRegistry::tool_definitions_for_session`), keeping the list
    /// byte-stable within a session (the trigger, like the channel, never
    /// changes) so the prompt cache holds.
    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::Any
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput>;
}

/// Which session-trigger contexts a tool is visible in (see
/// [`Tool::trigger_scope`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolTriggerScope {
    /// Visible in every session — the default.
    #[default]
    Any,
    /// Visible only in a recurring cron fire's own conversation
    /// (`TriggerSource::Cron { conversation: true }`). Used by `report_nothing`,
    /// which can only suppress a recurring fire's own notification.
    CronFire,
}

impl ToolTriggerScope {
    /// Whether a session started by `trigger` may see this tool.
    pub fn allows_trigger(&self, trigger: &TriggerSource) -> bool {
        match self {
            ToolTriggerScope::Any => true,
            ToolTriggerScope::CronFire => trigger.is_cron_conversation(),
        }
    }
}

/// A tool's request that the current turn produce **no** user-facing
/// notification. The agent loop hands a fresh signal to the tools of a turn
/// that *can* be silenced — today only a recurring cron fire, for its
/// `report_nothing` tool — and reads it back after the turn; it is `None`
/// everywhere else, so a tool that flips it outside such a turn is a no-op.
/// Cheap to clone: the flag is shared through an `Arc`.
#[derive(Clone, Default)]
pub struct NotifySilence(Arc<std::sync::atomic::AtomicBool>);

impl NotifySilence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request that this turn stay silent (no notification row, push, or pulse).
    pub fn request(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a tool requested silence during this turn.
    pub fn requested(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Context injected into tool execution by the agent layer.
pub struct ToolContext {
    pub session_id: SessionId,
    /// What started the session this call runs in. A tool that records the
    /// calling conversation for later use needs it: `CronCreate` running
    /// inside a cron fire (fire sessions get the full tool registry) must
    /// record the fire's *own* origin as the new turn's origin, not the
    /// transient fire session — otherwise a one-shot's result would be
    /// delivered into a session nobody can see.
    pub session_trigger: TriggerSource,
    /// Turn this tool call belongs to. Tools that emit downstream work
    /// (e.g. spawn a subagent) carry it so the spawned work can be
    /// lineaged back to the originating turn. Production wiring sources
    /// it from the per-turn context the executor opens around each tool
    /// call.
    pub turn_id: TurnId,
    /// This tool's own `ToolCall` span id. Tools that emit downstream
    /// work record it as `parent_span_id` so the resulting lineage
    /// pins back to the exact span that spawned them.
    pub span_id: SpanId,
    pub user: User,
    /// The agent this call's session runs as — its memory partition and its
    /// private skill overlay. Always populated (an unbound session resolves
    /// to the built-in via `SessionState::agent_id_or_builtin`), so a tool
    /// never has to decide what "no agent" means. Sessions whose work
    /// originates inside a bound one (subagent children, cron fires) carry
    /// the originating agent, so a worker's `mem0_add` lands in the right
    /// partition.
    pub agent_id: AgentProfileId,
    pub timeout: Duration,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    /// Sandbox FS scope — points at `<workspace>/work/`. Tools whose
    /// reach is bounded by the OS sandbox use this; tools that need to
    /// touch other workspace subtrees (`profile/`, `config/`, `logs/`,
    /// `state/`) reach for [`Self::workspace_paths`] instead.
    pub workspace_root: PathBuf,
    /// Layout addresses anchored at the actual workspace root. Lets a
    /// tool resolve `profile/SOUL.md`, `state/storage.db`, etc. without
    /// hard-coding the relative offset from `workspace_root`. Cheap to
    /// clone (one `PathBuf` inside).
    pub workspace_paths: WorkspacePaths,
    pub sandbox: Option<Arc<dyn ExecSandbox>>,
    /// Optional user-facing reason the runtime intentionally did not wire
    /// [`Self::sandbox`] for an ExecCommand-capable tool. `Some` means Bash
    /// should surface a downgrade notice before running without the inner OS
    /// sandbox; `None` keeps the downgrade silent (for bench / outer-container
    /// environments where nested sandboxing is expected to be absent).
    pub sandbox_bypass_reason: Option<String>,
    /// Mid-execution approval handle. Tools that decide which resources
    /// they will touch only after some internal work (e.g. one that
    /// runs an LLM to draft a program before knowing what files it will
    /// read or whether it needs network) prompt the user through this
    /// handle. `None` means the executor did not wire one in; callers
    /// must fail-closed.
    pub approval: Option<ApprovalHandle>,
    /// Side-channel to surface non-fatal verdicts (warnings, blocks)
    /// to the user channel without going through the LLM-visible tool
    /// result. Today only the `Skill` tool emits notices (when the
    /// risk assessor returns `Suspicious` or `Dangerous`) and Bash emits
    /// sandbox-downgrade / escape notices; other tools leave this `None`.
    pub notifier: Option<Arc<dyn SessionNotifier>>,
    /// Per-tool-call event sink. Tools emit arbitrary observations —
    /// phase timers (`http_request`, `html_to_markdown`, …), HTTP
    /// response summaries, side-LLM round-trips — via
    /// [`ToolEventSink::emit`] or the RAII [`start_timer`] helper.
    /// The agent layer flushes each entry as a
    /// `SpanEventKind::ToolEvent` so the trace view can render the
    /// structured payload. Always present — call sites without a
    /// real sink wire [`noop_event_sink`] so tool bodies never have
    /// to branch on `Option`.
    pub events: Arc<dyn ToolEventSink>,
    /// Per-call billed-LLM handle for tools that need in-flow LLM access
    /// — WebFetch's prompt-driven extraction, the Bash risk judges. The
    /// agent layer binds it to the current `(user, session, turn, span)`
    /// so `chat()` records cost against the running tool's span. `None`
    /// when no LLM is wired (argv-mode boots, tests that don't exercise
    /// the side LLM); tools must fail-closed by ignoring their
    /// LLM-dependent code path.
    ///
    /// **This is the session entry's LITE model, not its chat model** —
    /// every tool-side call is a one-shot classification or extraction,
    /// so they all resolve through `LlmClientPool::resolve_lite`. It
    /// equals the chat model only when no lite model is configured. A
    /// tool that genuinely needs the session's own model would have to
    /// have it plumbed separately; do not assume this is it.
    pub lite_llm: Option<Arc<dyn baybo_llm::BilledChat>>,
    /// Tool-side access to user-managed secrets, bound by the agent layer
    /// (mirrors [`Self::lite_llm`]). `BashTool` uses it to resolve `secret_env`
    /// names to plaintext for child-process injection and to redact those
    /// values from the output; the `secret_*` tools use it to add / list /
    /// check. `None` when not wired (argv-mode boots, tests that don't
    /// exercise secrets); consumers must fail-closed.
    pub secrets: Option<Arc<dyn SecretAccess>>,
    /// Optional resolver for **virtual** reads (paths with no on-disk backing,
    /// e.g. the session transcript). [`builtin::read::ReadTool`] consults it
    /// before the filesystem; `None` (most call sites) means every `Read` hits
    /// the real filesystem. The resolver self-enforces access control via
    /// [`VirtualReadAccess`].
    pub virtual_reads: Option<Arc<dyn VirtualReadResolver>>,
    /// Read-before-write tracker enforcing the `Read`→`Edit`/`Write`
    /// contract. `Read` records each file's fingerprint here; `Edit`
    /// (always) and `Write` (when overwriting an existing file) refuse to
    /// run unless the file was read and is unchanged since. `Some` **only
    /// for the interactive agent loop**; argv-mode / test call sites leave it
    /// `None`, which disables the contract. See [`ReadTracker`].
    pub read_tracker: Option<ReadTracker>,
    /// Runtime hook for detaching a slow command into the background.
    /// `Some` **only for user-facing sessions** (the runtime gates the
    /// injection), so its presence is the "may this command convert to
    /// background on timeout" signal. `None` for cron / nested-subagent
    /// sessions and argv-mode boots — those keep kill-on-timeout.
    pub background_jobs: Option<Arc<dyn BackgroundJobSink>>,
    /// View + control of this session's in-flight background jobs, for the
    /// `JobList` / `JobStop` tools. Gated like [`Self::background_jobs`].
    pub background_control: Option<Arc<dyn BackgroundJobControl>>,
    /// Handle a tool flips to suppress this turn's user-facing notification.
    /// `Some` **only for a recurring cron fire turn** (wired by the actor's
    /// cron dispatch, read by the `report_nothing` tool); `None` everywhere
    /// else, so the tool no-ops on a user reply, a one-shot fire, or any
    /// ordinary turn. See [`NotifySilence`].
    pub notify_silence: Option<NotifySilence>,
}

#[cfg(any(test, feature = "test-support"))]
impl ToolContext {
    /// Minimal context for tests: placeholder identity, a `/tmp` workspace,
    /// and every optional capability (sandbox, approval, llm, secrets,
    /// virtual reads, read tracker, background jobs) left unset. Override
    /// the few fields a test actually cares about with struct-update syntax
    /// so a new `ToolContext` field never has to be threaded through every
    /// fixture again:
    ///
    /// ```ignore
    /// ToolContext { timeout: Duration::from_secs(10), ..ToolContext::for_test() }
    /// ```
    pub fn for_test() -> Self {
        Self {
            session_id: "test".into(),
            session_trigger: TriggerSource::User,
            turn_id: TurnId::default(),
            span_id: SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            agent_id: AgentProfileId::builtin(),
            timeout: Duration::from_secs(5),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            workspace_root: PathBuf::from("/tmp"),
            workspace_paths: WorkspacePaths::new("/tmp"),
            sandbox: None,
            sandbox_bypass_reason: None,
            approval: None,
            notifier: None,
            events: noop_event_sink(),
            lite_llm: None,
            secrets: None,
            virtual_reads: None,
            read_tracker: None,
            background_jobs: None,
            background_control: None,
            notify_silence: None,
        }
    }
}

/// Severity of a [`SessionNotifier`] event. Matches
/// `baybo_channels::NoticeLevel` exactly — the agent-loop bridge does
/// a one-to-one variant mapping when it forwards onto
/// `AgentEvent::Notice`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warn,
    Error,
}

/// Side-channel emitter for tool-side notices. Sync, fire-and-forget —
/// tools should not `await` notice delivery.
pub trait SessionNotifier: Send + Sync {
    fn emit(&self, level: NoticeLevel, summary: &str, detail: &str);
}

/// No-op notifier for tests and for call sites that don't have a
/// channel-attached notifier wired in.
pub struct NoopNotifier;

impl SessionNotifier for NoopNotifier {
    fn emit(&self, _level: NoticeLevel, _summary: &str, _detail: &str) {}
}

/// Per-tool-call event sink. Tools emit arbitrary observations
/// (phase timings, HTTP fetches, side-LLM calls, …) keyed by an
/// `action` label; the agent layer drains the buffer after the tool
/// returns and surfaces each entry as a `SpanEventKind::ToolEvent`.
/// Sync and fire-and-forget — tools must not `await` emission.
///
/// Use [`start_timer`] for the common phase-timer case (RAII guard
/// emits a `ToolEventPayload::Phase` on `Drop`); call
/// [`ToolEventSink::emit`] directly for richer payloads.
pub trait ToolEventSink: Send + Sync {
    fn emit(&self, action: &str, payload: ToolEventPayload);
}

/// RAII helper that emits a `ToolEventPayload::Phase` carrying
/// `now - started` when it drops. Build via [`start_timer`] for the
/// common `ctx.events`-backed case.
pub struct ToolTimer<'a> {
    sink: &'a dyn ToolEventSink,
    action: String,
    started: Instant,
}

impl<'a> ToolTimer<'a> {
    /// Construct a timer that emits via a borrowed `ToolEventSink`.
    /// Most callers should prefer [`start_timer`] which derives the
    /// borrow from the `Arc<dyn ToolEventSink>` carried in `ToolContext`.
    pub fn new(sink: &'a dyn ToolEventSink, action: impl Into<String>) -> Self {
        Self {
            sink,
            action: action.into(),
            started: Instant::now(),
        }
    }
}

impl<'a> Drop for ToolTimer<'a> {
    fn drop(&mut self) {
        self.sink.emit(
            &self.action,
            ToolEventPayload::Phase {
                duration_ms: self.started.elapsed().as_millis() as u64,
            },
        );
    }
}

/// Start an RAII timer against the event sink in `ToolContext`.
/// Usage: `let _t = start_timer(&ctx.events, "http_request");` — the
/// guard emits a `Phase` payload on drop, so the elapsed time is
/// whatever the enclosing scope took. The borrow keeps the
/// `Arc<dyn ToolEventSink>` alive for the timer's lifetime.
pub fn start_timer<'a>(
    sink: &'a Arc<dyn ToolEventSink>,
    action: impl Into<String>,
) -> ToolTimer<'a> {
    ToolTimer::new(sink.as_ref(), action)
}

/// No-op event sink. Used as the default wired into `ToolContext`
/// when no agent-level recorder is attached (tests, argv-mode boot,
/// any code path that calls a tool without going through
/// `ToolExecutor`). Built via [`noop_event_sink`] to avoid repeating
/// the `Arc::new(...) as Arc<dyn ToolEventSink>` ceremony at every
/// call site.
pub struct NoopEventSink;

impl ToolEventSink for NoopEventSink {
    fn emit(&self, _action: &str, _payload: ToolEventPayload) {}
}

/// Allocate a fresh `Arc<dyn ToolEventSink>` backed by [`NoopEventSink`].
/// Default sink for every `ToolContext` literal outside the agent
/// path — tools never see a `None`, so they don't have to branch.
pub fn noop_event_sink() -> Arc<dyn ToolEventSink> {
    Arc::new(NoopEventSink)
}

/// Mid-execution approval entry point handed to a tool through
/// [`ToolContext::approval`]. Wraps the resolved gate plus a shared
/// handle to the session's approved-resources cache. The handle is
/// cache-aware in both directions:
///
/// - Read: before forwarding to the gate, request filters out
///   accesses that the cache already covers (matches the pre-execute
///   gate in `baybo-agent`'s `ToolExecutor`). When *all* accesses are
///   covered the call short-circuits to `Approve` without prompting.
/// - Write: on `ApproveAlways` the granted accesses are appended to
///   the cache so a follow-up call inside the same session does not
///   re-prompt.
#[derive(Clone)]
pub struct ApprovalHandle {
    gate: Arc<dyn ApprovalGate>,
    /// Shared cache. Same `Arc` the agent's `ToolExecutor` consults
    /// for pre-execute approvals, so mid-execution prompts and
    /// pre-execute prompts use a single source of truth.
    approved_cache: Arc<parking_lot::Mutex<Vec<ApprovedResource>>>,
    /// `tool_use_id` of the call this handle serves, stamped onto every
    /// [`ApprovalRequest`] so clients can badge the right work step. `None`
    /// for handles built outside a tool-call context (tests).
    tool_call_id: Option<String>,
    /// Last decision an ACTUAL prompt returned (cache short-circuits never
    /// record). Shared across clones so the executor reads back what
    /// mid-execution prompts decided after the tool returns.
    last_decision: Arc<parking_lot::Mutex<Option<ApprovalDecision>>>,
}

impl ApprovalHandle {
    pub fn new(
        gate: Arc<dyn ApprovalGate>,
        approved_cache: Arc<parking_lot::Mutex<Vec<ApprovedResource>>>,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            gate,
            approved_cache,
            tool_call_id,
            last_decision: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// Decision of the most recent prompt this handle (or any clone) actually
    /// raised — `None` when everything was covered by cache. The executor
    /// stamps this into the persisted [`baybo_model::ToolResultMeta`].
    pub fn last_decision(&self) -> Option<ApprovalDecision> {
        *self.last_decision.lock()
    }

    /// Forward a request to the gate WITHOUT consulting the session
    /// approval cache. Use when an access is meaningfully different
    /// from a previously-cached one (e.g. an *unsandboxed* re-run of a
    /// command whose sandboxed run was already approved): the cache
    /// entry covers the original privilege but not the elevated one,
    /// so we must always re-prompt. Never persists the decision —
    /// follow-up calls always re-prompt too.
    pub async fn request_uncached(
        &self,
        tool: &str,
        session_id: &SessionId,
        user: &User,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
    ) -> ApprovalDecision {
        let req = ApprovalRequest {
            call_id: Uuid::new_v4().to_string(),
            tool_call_id: self.tool_call_id.clone(),
            session_id: session_id.clone(),
            user_id: user.id.clone(),
            tool: tool.to_string(),
            accesses,
            params_preview,
            description: None,
        };
        let decision = self.gate.request(req).await;
        *self.last_decision.lock() = Some(decision);
        decision
    }

    /// Forward a request to the gate, filtered by the session approval
    /// cache. Returns `Approve` without prompting when every access is
    /// already covered. On `ApproveAlways`, persists the (uncovered)
    /// accesses into the cache before returning.
    pub async fn request(
        &self,
        tool: &str,
        session_id: &SessionId,
        user: &User,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
    ) -> ApprovalDecision {
        // Filter against the cache up front. Read-only file accesses
        // were already a no-op for the pre-execute gate (see
        // `ToolExecutor::execute`); preserve that behaviour here so
        // mid-execution prompts do not appear stricter than the
        // pre-execute pass.
        let uncovered: Vec<ResourceAccess> = {
            let cache = self.approved_cache.lock();
            accesses
                .into_iter()
                .filter(|acc| {
                    if matches!(acc, ResourceAccess::ReadFile { .. }) {
                        return false;
                    }
                    !cache.iter().any(|ar| ar.covers(acc))
                })
                .collect()
        };

        if uncovered.is_empty() {
            return ApprovalDecision::Approve;
        }

        let req = ApprovalRequest {
            call_id: Uuid::new_v4().to_string(),
            tool_call_id: self.tool_call_id.clone(),
            session_id: session_id.clone(),
            user_id: user.id.clone(),
            tool: tool.to_string(),
            accesses: uncovered.clone(),
            params_preview,
            description: None,
        };
        let decision = self.gate.request(req).await;
        *self.last_decision.lock() = Some(decision);
        if decision == ApprovalDecision::ApproveAlways {
            let mut cache = self.approved_cache.lock();
            for access in &uncovered {
                let entry = access.to_approved();
                if !cache.iter().any(|existing| existing == &entry) {
                    cache.push(entry);
                }
            }
        }
        decision
    }
}

/// Per-call options for [`ExecSandbox::spawn_command`]. `extra_env` injects
/// `KEY=value` pairs into ONLY this child process (e.g. secrets resolved for a
/// Bash `secret_env`) without ever putting them in the command string. NOTE:
/// `timeout` defaults to zero — every caller must set it explicitly.
#[derive(Debug, Clone, Default)]
pub struct SpawnOpts {
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub extra_env: Vec<(String, String)>,
    pub timeout: Duration,
}

/// OS-level sandbox runner exposed to tools that need to spawn an
/// external process. The `baybo-agent` crate adapts a real
/// `baybo_sandbox::SandboxRunner` into this trait so `baybo-tools` does
/// not gain a transitive dependency on `baybo-sandbox`.
#[async_trait]
pub trait ExecSandbox: Send + Sync {
    async fn spawn_command(
        &self,
        program: &Path,
        args: &[String],
        opts: SpawnOpts,
    ) -> crate::Result<SandboxedOutput>;

    /// Spawn a command **detached**: return a live [`RunningChild`]
    /// immediately instead of waiting. The caller owns the foreground
    /// wait + timeout, and can hand the still-running child to a
    /// [`BackgroundJobSink`] to keep it alive past the tool call. The
    /// `opts.timeout` is ignored (the caller times out). Backends that
    /// can't expose a live child return an error and the caller falls
    /// back to the blocking [`Self::spawn_command`] (kill-on-timeout).
    async fn spawn_command_detached(
        &self,
        _program: &Path,
        _args: &[String],
        _opts: SpawnOpts,
    ) -> crate::Result<Box<dyn RunningChild>> {
        Err(crate::ToolError::Execution(
            "detached spawn is not supported by this sandbox backend".into(),
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct SandboxedOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// A live, detached child process the runtime keeps streaming and awaits
/// out-of-band. Backs the "Bash timeout → background" path: the tool takes
/// the stdout/stderr readers (to stream them to files), then hands the
/// child to a [`BackgroundJobSink`] which awaits [`Self::wait`] and routes
/// a completion notification. `/stop` / `JobStop` trip the escort's cancel
/// token, which calls [`Self::start_kill`].
#[async_trait]
pub trait RunningChild: Send {
    /// Take the child's stdout reader (once). `None` if already taken or
    /// not piped.
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;
    /// Take the child's stderr reader (once).
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;
    /// Wait for the child to exit; returns its exit code (or -1 if it was
    /// killed by signal / produced no code).
    async fn wait(&mut self) -> i32;
    /// Request termination (best-effort). The escort calls this on cancel.
    fn start_kill(&mut self);
    /// The host process-group id (== leader pid) for an unsandboxed child in
    /// its own group, so the runtime can persist it for crash-safe boot reaping.
    /// `None` for children with no host group to reap directly (sandboxed
    /// docker/bwrap children, reaped by their backend's own teardown).
    fn process_group_id(&self) -> Option<i32> {
        None
    }
}

/// [`RunningChild`] over a plain [`tokio::process::Child`] — the
/// unsandboxed path, and what the sandbox adapter wraps the bwrap child in.
pub struct TokioRunningChild(pub tokio::process::Child);

#[async_trait]
impl RunningChild for TokioRunningChild {
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.0
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.0
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    async fn wait(&mut self) -> i32 {
        self.0
            .wait()
            .await
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1)
    }
    fn start_kill(&mut self) {
        let _ = self.0.start_kill();
    }
}

/// What a tool hands to the runtime when a command crosses into the
/// background: the still-running child plus the in-flight stdout/stderr →
/// file copy tasks (the escort awaits them so the files are fully flushed
/// before it reads their tails for the completion notification).
pub struct DetachedCommand {
    /// Minted by the tool so the output file names, the handle returned to
    /// the LLM, and the eventual completion notification all share one id.
    pub handle_id: String,
    pub session_id: SessionId,
    pub command: String,
    pub child: Box<dyn RunningChild>,
    pub copy_tasks: Vec<tokio::task::JoinHandle<()>>,
    pub stdout_path: std::path::PathBuf,
    pub stderr_path: std::path::PathBuf,
}

/// Runtime hook that takes ownership of a [`DetachedCommand`], streams it
/// to completion off the tool call, and routes a background-turn completion
/// notification to the parent session. Implemented by the agent layer
/// (`baybo-agent`) and injected via [`ToolContext::background_jobs`], which
/// the runtime populates **only for user-facing sessions** — so the
/// presence of the sink is itself the "is this conversation eligible to
/// background work" gate.
#[async_trait]
pub trait BackgroundJobSink: Send + Sync {
    /// Take the detached command; return the handle id the tool surfaces
    /// to the LLM. Returns promptly (registers + spawns an escort task) —
    /// it does NOT await the command.
    async fn detach_command(&self, turn: DetachedCommand) -> String;
}

/// One in-flight background job, as surfaced by the `JobList` tool.
pub struct BackgroundJobInfo {
    pub handle: String,
    /// Subagent profile name, or `"command"` for a detached `Bash` command.
    pub kind: String,
    /// The task summary (subagent) or the command line (command).
    pub summary: String,
}

/// Per-session view + control of in-flight background jobs (detached
/// subagents and commands), backing the `JobList` / `JobStop` tools.
/// Injected via [`ToolContext::background_control`] only for user-facing
/// sessions (mirrors [`BackgroundJobSink`]); the same runtime component
/// implements both.
#[async_trait]
pub trait BackgroundJobControl: Send + Sync {
    /// In-flight background jobs dispatched from `session_id`.
    async fn list(&self, session_id: &SessionId) -> Vec<BackgroundJobInfo>;
    /// Kill one in-flight turn by handle. `true` if it was running.
    async fn stop(&self, session_id: &SessionId, handle: &str) -> bool;
}

/// Tool-side access to user-managed secrets, injected by the agent layer
/// through [`ToolContext::secrets`]. The concrete impl wraps the security
/// gateway + `UserSecretManager`; `baybo-tools` sees only this trait, so the
/// reveal/mint pipeline and storage details stay in the agent layer (the
/// trait is implemented "from above" — see `docs/secret-management.md`).
#[async_trait]
pub trait SecretAccess: Send + Sync {
    /// Resolve named user secrets to plaintext for child-process env
    /// injection. Errors if any requested name is missing, so a bash run
    /// fails loudly rather than silently running with an unset variable.
    async fn resolve_env(&self, names: &[String]) -> crate::Result<Vec<(String, String)>>;

    /// Mint and vault a deterministic placeholder for each known plaintext
    /// `value` and literal-replace its occurrences in `text`. Reuses the
    /// gateway's mint/vault pipeline, so the placeholder stays reveal-able.
    /// Values too short to redact safely are skipped by the implementation.
    async fn redact(&self, text: &str, values: &[String]) -> crate::Result<String>;

    /// Mint and vault a placeholder for every secret the leak detector finds in
    /// `text`, returning the sanitized copy.
    ///
    /// Unlike [`Self::redact`] the caller supplies no candidate values, so this
    /// covers text whose secrets it cannot enumerate — raw captured output
    /// reaching an LLM outside the agent transcript, i.e. the bash risk
    /// judges.
    async fn sanitize(&self, text: &str) -> crate::Result<String>;

    /// Store a user secret. Returns whether it was newly created or replaced.
    async fn add(
        &self,
        name: &str,
        value: &[u8],
        overwrite: bool,
    ) -> crate::Result<baybo_security::AddOutcome>;

    /// All user-secret names (no values).
    async fn list_names(&self) -> crate::Result<Vec<String>>;

    /// Per-name existence for a batch, preserving input order.
    async fn exists(&self, names: &[String]) -> crate::Result<Vec<(String, bool)>>;
}

/// Output from a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutput {
    Text(String),
    Json(Value),
    Error(String),
    /// Tool result that also delivers attachments to the user channel.
    /// `text` is what the LLM sees as the tool result; `attachments`
    /// (media only — `Image` / `Audio` / `File`) are accumulated across the
    /// turn by the agent loop and folded into the final assistant message,
    /// so they persist with the transcript and survive a reload. They reach
    /// clients on `Frame::Message.attachments`; a turn that never reaches a
    /// clean final response drops them.
    WithAttachments {
        text: String,
        attachments: Vec<baybo_model::ContentBlock>,
    },
    /// Tool result that includes images the *LLM itself* should see on
    /// the next turn (e.g. `browser_screenshot`). The agent loop
    /// appends `text` as the normal text-only `ToolResult`, then emits
    /// a follow-up `Role::User` message carrying `llm_images` so a
    /// vision-capable provider receives them through the standard
    /// multimodal user-content path. These images are for the model, not
    /// the user: they never reach a channel. Use [`ToolOutput::WithAttachments`]
    /// to show media to the user.
    ///
    /// Invariant (asserted by [`ToolOutput::multi_modal_text`]): every entry
    /// of `llm_images` is `ContentBlock::Image`. Other variants are
    /// silently dropped at construction.
    MultiModalText {
        text: String,
        llm_images: Vec<baybo_model::ContentBlock>,
    },
}

impl ToolOutput {
    /// Construct a [`ToolOutput::MultiModalText`], filtering
    /// `llm_images` to only `ContentBlock::Image` entries. Other
    /// variants are silently dropped — the variant is documented as
    /// images-only and accidental misuse from a tool author should
    /// not surprise the agent loop.
    pub fn multi_modal_text(text: String, llm_images: Vec<baybo_model::ContentBlock>) -> Self {
        let llm_images = llm_images
            .into_iter()
            .filter(|b| matches!(b, baybo_model::ContentBlock::Image { .. }))
            .collect();
        Self::MultiModalText { text, llm_images }
    }
}

/// Definition visible to the LLM for function calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// Tool manifest carrying governance and runtime metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub description: String,
    pub trust_level: baybo_model::TrustLevel,
    pub parameters_schema: Value,
    pub capabilities: Vec<ToolCapability>,
    /// Channels whose sessions may see and call this tool. Empty = every
    /// channel, the norm. Enforced twice: the agent loop omits the tool
    /// from the LLM-visible list for other sessions
    /// (`ToolRegistry::tool_definitions_for_session`), and the executor
    /// refuses a call that names it anyway — omission alone is not a
    /// gate against a hallucinated or prompted-by-skill-body call.
    #[serde(default)]
    pub channels: Vec<baybo_model::ChannelType>,
}

impl ToolManifest {
    /// Whether a session on `channel` may see or call this tool.
    /// An empty `channels` list means no restriction.
    pub fn allows_channel(&self, channel: &baybo_model::ChannelType) -> bool {
        self.channels.is_empty() || self.channels.contains(channel)
    }
}

/// Coarse capability ceiling declared in a tool's manifest.
///
/// A manifest capability says "this tool may do X at most"; the concrete
/// resource touched per call is described by [`ResourceAccess`] produced by
/// [`Tool::accessed_resources`]. The approval gate routes on `ResourceAccess`,
/// not on this enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    /// Reads from the filesystem. Approval gate prompts per path.
    ReadFile,
    /// Writes to the filesystem. Approval gate prompts per path.
    WriteFile,
    /// Performs network requests. Approval gate prompts per host.
    Http,
    /// Spawns a subprocess. Approval gate prompts per full command string.
    ExecCommand,
}

/// Convenience constructor for paths in [`ResourceAccess`] / [`ApprovedResource`].
pub fn resource_path(p: impl Into<PathBuf>) -> PathBuf {
    p.into()
}

pub use registry::ToolRegistry;

#[cfg(test)]
mod multi_modal_text_tests {
    use super::ToolOutput;
    use baybo_model::{BlobRef, ContentBlock};

    fn img() -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: format!("sha256:{}", "ab".repeat(32)),
            },
            mime_type: "image/png".into(),
            filename: None,
            width: None,
            height: None,
        }
    }

    #[test]
    fn keeps_image_blocks() {
        let out = ToolOutput::multi_modal_text("text".into(), vec![img(), img()]);
        match out {
            ToolOutput::MultiModalText { llm_images, .. } => {
                assert_eq!(llm_images.len(), 2);
                assert!(
                    llm_images
                        .iter()
                        .all(|b| matches!(b, ContentBlock::Image { .. }))
                );
            }
            _ => panic!("expected MultiModalText variant"),
        }
    }

    #[test]
    fn filters_non_image_variants() {
        // Documents the invariant: only `Image` survives. A regression
        // here means a refactor accidentally let other content-block
        // kinds reach the agent_loop's user-message forwarding path.
        let blocks = vec![
            img(),
            ContentBlock::Text("hi".into()),
            ContentBlock::Audio {
                blob: BlobRef {
                    blob_id: "sha256:0".into(),
                },
                mime_type: "audio/wav".into(),
                filename: None,
                duration_ms: None,
            },
            ContentBlock::File {
                blob: BlobRef {
                    blob_id: "sha256:0".into(),
                },
                filename: "x".into(),
                mime_type: "application/pdf".into(),
                duration_ms: None,
                page_count: None,
                size_bytes: None,
            },
            img(),
        ];
        let out = ToolOutput::multi_modal_text("text".into(), blocks);
        match out {
            ToolOutput::MultiModalText { llm_images, .. } => {
                assert_eq!(llm_images.len(), 2, "only the two Image blocks survive");
                assert!(
                    llm_images
                        .iter()
                        .all(|b| matches!(b, ContentBlock::Image { .. }))
                );
            }
            _ => panic!("expected MultiModalText variant"),
        }
    }

    #[test]
    fn empty_input_yields_empty_images() {
        let out = ToolOutput::multi_modal_text("just text".into(), vec![]);
        match out {
            ToolOutput::MultiModalText { text, llm_images } => {
                assert_eq!(text, "just text");
                assert!(llm_images.is_empty());
            }
            _ => panic!("expected MultiModalText variant"),
        }
    }
}

#[cfg(test)]
mod approval_handle_tests {
    use super::*;
    use crate::approval::{ApprovalGate, ApprovalRequest};
    use async_trait::async_trait;

    struct FixedGate(ApprovalDecision);

    #[async_trait]
    impl ApprovalGate for FixedGate {
        async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
            assert_eq!(
                req.tool_call_id.as_deref(),
                Some("use-1"),
                "prompt carries the tool call id"
            );
            self.0
        }
    }

    fn handle(decision: ApprovalDecision) -> ApprovalHandle {
        ApprovalHandle::new(
            Arc::new(FixedGate(decision)),
            Arc::new(parking_lot::Mutex::new(Vec::new())),
            Some("use-1".into()),
        )
    }

    fn user() -> User {
        User {
            id: "u".into(),
            name: None,
            channel: baybo_model::ChannelType::tui(),
        }
    }

    #[tokio::test]
    async fn records_last_decision_shared_across_clones() {
        let h = handle(ApprovalDecision::Deny);
        let probe = h.clone();
        assert_eq!(probe.last_decision(), None);
        let d = h
            .request(
                "Bash",
                &"s".into(),
                &user(),
                vec![ResourceAccess::ExecCommand {
                    command: "x".into(),
                }],
                "{}".into(),
            )
            .await;
        assert_eq!(d, ApprovalDecision::Deny);
        assert_eq!(probe.last_decision(), Some(ApprovalDecision::Deny));
    }

    #[tokio::test]
    async fn cache_short_circuit_records_nothing() {
        let h = handle(ApprovalDecision::Approve);
        let probe = h.clone();
        // ReadFile accesses are exempt from prompting, so the gate is never
        // consulted and the short-circuit Approve must NOT be recorded as a
        // prompted decision.
        let d = h
            .request(
                "Read",
                &"s".into(),
                &user(),
                vec![ResourceAccess::ReadFile {
                    path: std::path::PathBuf::from("/tmp/x"),
                }],
                "{}".into(),
            )
            .await;
        assert_eq!(d, ApprovalDecision::Approve);
        assert_eq!(probe.last_decision(), None, "no prompt raised, no record");
    }

    #[tokio::test]
    async fn request_uncached_records_too() {
        let h = handle(ApprovalDecision::ApproveAlways);
        let _ = h
            .request_uncached(
                "Bash",
                &"s".into(),
                &user(),
                vec![ResourceAccess::ExecCommand {
                    command: "x".into(),
                }],
                "{}".into(),
            )
            .await;
        assert_eq!(h.last_decision(), Some(ApprovalDecision::ApproveAlways));
    }
}
