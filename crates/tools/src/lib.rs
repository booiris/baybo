pub mod approval;
pub mod builtin;
pub mod error;
pub mod mcp;
pub mod progress;
pub mod registry;
pub mod virtual_read;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aura_model::{JobId, SessionId, SpanId, User};
use aura_trace::ToolEventPayload;
use aura_workspace::WorkspacePaths;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use approval::{
    ApprovalDecision, ApprovalGate, ApprovalGateMap, ApprovalQueue, ApprovalRequest,
    ApprovedResource, AutoDenyGate, ChannelApprovalGate, HostPattern, ResourceAccess,
};
pub use error::ToolError;
pub use virtual_read::{VirtualReadAccess, VirtualReadResolver};

pub type Result<T> = std::result::Result<T, ToolError>;

/// Whether a tool is safe to run concurrently with the sibling tool
/// calls in the same LLM response.
///
/// The agent loop dispatches every tool call of one response together.
/// A [`Concurrent`](ToolConcurrency::Concurrent) call may overlap other
/// `Concurrent` calls — the loop runs at most a fixed number at once. An
/// [`Exclusive`](ToolConcurrency::Exclusive) call runs alone: it waits
/// for any in-flight calls to drain, then blocks every other call (read
/// or write) until it returns, so a tool that mutates shared state never
/// races a concurrent reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConcurrency {
    /// Safe to run alongside other `Concurrent` calls — a read-only tool
    /// that touches no shared mutable state. Bounded by the agent loop's
    /// concurrency cap.
    Concurrent,
    /// Must run exclusively; no other tool call overlaps it. The
    /// conservative default for any tool with side effects.
    Exclusive,
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

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput>;
}

/// Context injected into tool execution by the agent layer.
pub struct ToolContext {
    pub session_id: SessionId,
    /// Job this tool call belongs to. Tools that emit downstream work
    /// (e.g. spawn a subagent) carry it so the spawned work can be
    /// lineaged back to the originating job. Production wiring sources
    /// it from the per-job context the executor opens around each tool
    /// call.
    pub job_id: JobId,
    /// This tool's own `ToolCall` span id. Tools that emit downstream
    /// work record it as `parent_span_id` so the resulting lineage
    /// pins back to the exact span that spawned them.
    pub span_id: SpanId,
    pub user: User,
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
    /// risk assessor returns `Suspicious` or `Dangerous`); other tools
    /// leave this `None`.
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
    /// Per-call billed-LLM handle for tools that need in-flow LLM
    /// access (today: WebFetch's prompt-driven extraction). The agent
    /// layer binds it to the current `(user, session, job, span)` so
    /// `chat()` records cost against the running tool's span. `None`
    /// when no LLM is wired (argv-mode boots, tests that don't
    /// exercise the side LLM); tools must fail-closed by ignoring
    /// their LLM-dependent code path.
    pub llm: Option<Arc<dyn aura_llm::BilledChat>>,
    /// Tool-side access to user-managed secrets, bound by the agent layer
    /// (mirrors [`Self::llm`]). `BashTool` uses it to resolve `secret_env`
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
}

/// Severity of a [`SessionNotifier`] event. Matches
/// `aura_channels::NoticeLevel` exactly — the agent-loop bridge does
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
///   gate in `aura-agent`'s `ToolExecutor`). When *all* accesses are
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
}

impl ApprovalHandle {
    pub fn new(
        gate: Arc<dyn ApprovalGate>,
        approved_cache: Arc<parking_lot::Mutex<Vec<ApprovedResource>>>,
    ) -> Self {
        Self {
            gate,
            approved_cache,
        }
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
            session_id: session_id.clone(),
            user_id: user.id.clone(),
            tool: tool.to_string(),
            accesses,
            params_preview,
            description: None,
        };
        self.gate.request(req).await
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
            session_id: session_id.clone(),
            user_id: user.id.clone(),
            tool: tool.to_string(),
            accesses: uncovered.clone(),
            params_preview,
            description: None,
        };
        let decision = self.gate.request(req).await;
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
/// external process. The `aura-agent` crate adapts a real
/// `aura_sandbox::SandboxRunner` into this trait so `aura-tools` does
/// not gain a transitive dependency on `aura-sandbox`.
#[async_trait]
pub trait ExecSandbox: Send + Sync {
    async fn spawn_command(
        &self,
        program: &Path,
        args: &[String],
        opts: SpawnOpts,
    ) -> crate::Result<SandboxedOutput>;
}

#[derive(Debug, Clone, Default)]
pub struct SandboxedOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// Tool-side access to user-managed secrets, injected by the agent layer
/// through [`ToolContext::secrets`]. The concrete impl wraps the security
/// gateway + `UserSecretManager`; `aura-tools` sees only this trait, so the
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

    /// Store a user secret. Returns whether it was newly created or replaced.
    async fn add(
        &self,
        name: &str,
        value: &[u8],
        overwrite: bool,
    ) -> crate::Result<aura_security::AddOutcome>;

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
    /// are hoisted into the assistant's `OutgoingMessage` by the agent
    /// loop and the channel sidecar then sends them out-of-band.
    WithAttachments {
        text: String,
        attachments: Vec<aura_model::ContentBlock>,
    },
    /// Tool result that includes images the *LLM itself* should see on
    /// the next turn (e.g. `browser_screenshot`). The agent loop
    /// appends `text` as the normal text-only `ToolResult`, then emits
    /// a follow-up `Role::User` message carrying `llm_images` so a
    /// vision-capable provider receives them through the standard
    /// multimodal user-content path. The same images are also mirrored
    /// into the final `OutgoingMessage` so the user channel sees them.
    ///
    /// Invariant (asserted by [`MultiModalText::new`]): every entry of
    /// `llm_images` is `ContentBlock::Image`. Other variants are
    /// silently dropped at construction.
    MultiModalText {
        text: String,
        llm_images: Vec<aura_model::ContentBlock>,
    },
}

impl ToolOutput {
    /// Construct a [`ToolOutput::MultiModalText`], filtering
    /// `llm_images` to only `ContentBlock::Image` entries. Other
    /// variants are silently dropped — the variant is documented as
    /// images-only and accidental misuse from a tool author should
    /// not surprise the agent loop.
    pub fn multi_modal_text(text: String, llm_images: Vec<aura_model::ContentBlock>) -> Self {
        let llm_images = llm_images
            .into_iter()
            .filter(|b| matches!(b, aura_model::ContentBlock::Image { .. }))
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
    pub trust_level: aura_model::TrustLevel,
    pub parameters_schema: Value,
    pub capabilities: Vec<ToolCapability>,
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
    use aura_model::{BlobRef, ContentBlock};

    fn img() -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: format!("sha256:{}", "ab".repeat(32)),
            },
            mime_type: "image/png".into(),
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
            },
            ContentBlock::File {
                blob: BlobRef {
                    blob_id: "sha256:0".into(),
                },
                filename: "x".into(),
                mime_type: "application/pdf".into(),
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
