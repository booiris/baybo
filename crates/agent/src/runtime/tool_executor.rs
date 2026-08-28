use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use baybo_llm::{Attribution, BillableLlm, BilledChat};
use baybo_model::{
    ContentBlock, McpToolGrant, McpTransportIdentity, ParallelGroup, SessionId, SpanId, TrustLevel,
    TurnId, User, normalize_mcp_tool_grants,
};

use baybo_sandbox::{NetworkPolicy, SandboxRunner, default_sensitive_denylist};
use baybo_tools::{
    ApprovalDecision, ApprovalGate, ApprovalGateMap, ApprovalHandle, ApprovalOutcome,
    ApprovalRequest, ApprovedResource, AutoDenyGate, ExecSandbox, ReadTracker, ResourceAccess,
    ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput, ToolRegistry,
    VirtualReadResolver, approval::preview_params, refusal_reason,
};
use baybo_trace::{
    LifecycleOutcome, PersistedToolCallOutput, SpanEventKind, SpanFinalize, SpanKind, SpanRecorder,
    StepHandle, ToolCallBegin, ToolCallOrigin, ToolCallOutput, ToolCallResult, ToolEventPayload,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use baybo_project::worktree::{self, Checkout, ProjectRepo};
use baybo_store::agent_profile::AgentProfileStore;

use crate::runtime::sandbox::SandboxAdapter;
use crate::security::SecurityGateway;

fn admits_worktree(resolved: &Path, work_dir: &Path) -> Result<(), String> {
    if resolved.starts_with(work_dir) {
        Ok(())
    } else {
        Err("it is no longer inside the work directory".to_owned())
    }
}

fn admits_repo(resolved: &Path, paths: &baybo_workspace::WorkspacePaths) -> Result<(), String> {
    if let Some(why) = baybo_sandbox::args::writable_bind_refusal(resolved) {
        return Err(why);
    }
    let root = baybo_workspace::absolutise(paths.root());
    let work = baybo_workspace::absolutise(&paths.work_dir());
    if root.starts_with(resolved) {
        return Err("it contains baybo's own workspace".to_owned());
    }
    // No `work/` exemption on the ancestor side above: containing `work/`
    // does not stop a parent from containing `state/` and `.key/` as well.
    if resolved.starts_with(&root) && !resolved.starts_with(&work) {
        return Err("it is inside baybo's protected workspace data".to_owned());
    }
    Ok(())
}

const BAYBO_DEFAULT_STATE_DIR: &str = ".baybo";

/// Permissive Bash root and the state trees masked within it.
struct PermissiveScope {
    extra_root: PathBuf,
    denied: Vec<PathBuf>,
}

/// Mask live and default state roots; fall back when `$HOME` is absent.
fn permissive_scope(
    live_root: &Path,
    home: Option<&Path>,
    baybo_home: Option<&Path>,
    fs_fallback_root: &Path,
) -> PermissiveScope {
    let mut state_roots = vec![baybo_workspace::absolutise(live_root)];
    let default_state = baybo_home
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(BAYBO_DEFAULT_STATE_DIR)));
    if let Some(default_state) = default_state {
        state_roots.push(default_state);
    }
    PermissiveScope {
        extra_root: home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| fs_fallback_root.to_path_buf()),
        denied: default_sensitive_denylist(home, &state_roots),
    }
}

async fn board_handle(
    agents: &Arc<dyn AgentProfileStore>,
    agent: &baybo_model::AgentProfileId,
    project: &baybo_model::ProjectId,
) -> Option<baybo_model::AgentHandle> {
    match agents.get(agent).await {
        Ok(Some(row)) => row
            .team
            .filter(|team| team.project_id == *project)
            .map(|team| team.handle),
        Ok(None) => {
            debug!(agent_id = %agent, "the agent working this issue has no profile row; its commits carry no agent trailer");
            None
        }
        Err(e) => {
            debug!(agent_id = %agent, error = %e, "could not read the profile of the agent working this issue; its commits carry no agent trailer");
            None
        }
    }
}

/// Preview length used when rendering parameters inside an approval prompt.
const APPROVAL_PARAMS_PREVIEW_LEN: usize = 512;

/// Authority available only while a cron fire runs its first, unattended
/// turn. It is passed with that turn and never copied into `SessionState`, so
/// a later user reply in the same cron conversation follows ordinary approval.
#[derive(Debug, Clone, Default)]
pub struct InitialCronToolContext {
    mcp_tool_grants: Arc<[McpToolGrant]>,
}

impl InitialCronToolContext {
    pub fn new(mut mcp_tool_grants: Vec<McpToolGrant>) -> Self {
        normalize_mcp_tool_grants(&mut mcp_tool_grants);
        Self {
            mcp_tool_grants: mcp_tool_grants.into(),
        }
    }

    fn grants(&self, tool_name: &str, transport_identity: &McpTransportIdentity) -> bool {
        self.mcp_tool_grants
            .binary_search_by(|grant| {
                grant
                    .tool_name
                    .as_str()
                    .cmp(tool_name)
                    .then_with(|| grant.transport_identity.cmp(transport_identity))
            })
            .is_ok()
    }
}

/// Headroom added to the per-tool outer timeout so a slow user
/// approval (handled mid-execution via [`baybo_tools::ApprovalHandle`])
/// cannot kill the tool while it is legitimately blocked on a modal.
/// Tracks the channel gate's own `APPROVAL_TIMEOUT` in
/// `crates/gateway/src/channel/adapter.rs`; if you change one, change
/// the other.
const APPROVAL_HEADROOM: Duration = Duration::from_secs(300);

/// Cap on the failure-reason string we copy out of a `ToolOutput::Error`
/// payload. This bound governs only the row-level reason label; the body
/// lands either behind the span's transcript reference or in the capped
/// inline fallback (see [`cap_trace_output_with_len`]).
const FAILURE_REASON_MAX_BYTES: usize = 512;

/// Hard cap on the *aggregate* byte size of the drained payloads of a single
/// tool call — a backstop against a tool that emits a very large number of
/// events. Individual fields are already bounded by
/// `SPAN_EVENT_TEXT_MAX_BYTES` in the sink, so no single event can approach
/// this on its own.
///
/// Entries past the cap are dropped whole and silently (the trace is
/// best-effort, not load-bearing), which is exactly why the per-field bound
/// has to come first: a fat field here would evict later events, including the
/// `ParseFailure` audit record.
///
/// There is no separate cap on entry count: tools that emit a steady
/// trickle of small `Phase` events are bounded by this same byte
/// budget (each entry costs at least its `action` label length).
const TOOL_EVENTS_MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Buffer that implements [`baybo_tools::ToolEventSink`] by stashing
/// `(action, payload)` entries for the executor to drain into
/// [`SpanEventKind::ToolEvent`] events once the tool returns. Sync,
/// fire-and-forget — the tool body sees a plain trait object.
struct SpanEventRecorder {
    entries: Mutex<SpanEventRecorderState>,
}

#[derive(Default)]
struct SpanEventRecorderState {
    items: Vec<(String, ToolEventPayload)>,
    text_bytes: usize,
}

impl SpanEventRecorder {
    fn new() -> Self {
        Self {
            entries: Mutex::new(SpanEventRecorderState::default()),
        }
    }

    fn drain(&self) -> Vec<(String, ToolEventPayload)> {
        std::mem::take(&mut self.entries.lock().items)
    }
}

/// Approximate textual size of a payload — used to enforce the
/// per-call byte cap. Counts bytes of every owned `String` field;
/// fixed-size scalars (status codes, durations) contribute nothing.
fn payload_text_bytes(payload: &ToolEventPayload) -> usize {
    match payload {
        ToolEventPayload::Phase { .. } => 0,
        ToolEventPayload::HttpFetch {
            content_type,
            body_preview,
            ..
        } => {
            content_type.as_deref().map(str::len).unwrap_or(0)
                + body_preview.as_deref().map(str::len).unwrap_or(0)
        }
        ToolEventPayload::LlmCall {
            model,
            input,
            output,
        } => model.len() + input.len() + output.len(),
        ToolEventPayload::ParseFailure { command } => command.len(),
    }
}

impl baybo_tools::ToolEventSink for SpanEventRecorder {
    fn emit(&self, action: &str, mut payload: ToolEventPayload) {
        // Bound each text field here, at the one sink every emitter passes
        // through, rather than trusting five producers to remember. Note this
        // must happen *before* the running-total check below: that check drops
        // the whole entry, so an unbounded field would not merely bloat the
        // trace, it would make later events (including `ParseFailure`, which is
        // an audit record) vanish depending on arrival order.
        payload.truncate_text_fields();
        let mut guard = self.entries.lock();
        let cost = action.len() + payload_text_bytes(&payload);
        if guard.text_bytes.saturating_add(cost) > TOOL_EVENTS_MAX_PAYLOAD_BYTES {
            return;
        }
        guard.text_bytes += cost;
        guard.items.push((action.to_string(), payload));
    }
}

fn truncate_for_reason(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// Project a [`ToolOutput`] into a trace-friendly JSON value.
///
/// We can't just `serde_json::to_value(&output)` because `ToolOutput`
/// is `#[serde(tag = "type")]` (internally tagged) but `Text(String)`,
/// `Error(String)`, and `Json(<non-object>)` are tuple variants whose
/// content can't host the injected `type` tag — serde returns
/// "cannot serialize tagged newtype variant … containing a string"
/// and the previous `.unwrap_or(Value::Null)` quietly stored `null`,
/// so the trace UI's Output panel was empty for every text-returning
/// tool (browser/*, WebFetch text, etc.). The struct variants and
/// `Json(Object)` are unaffected — those are the cases that previously
/// rendered correctly (e.g. `NowTool` returns `Json({...})`), so we
/// preserve their on-disk shape verbatim.
#[cfg(test)]
fn tool_output_to_trace_value(output: &ToolOutput) -> (Value, Option<usize>) {
    cap_trace_output(tool_output_to_trace_value_uncapped(output))
}

/// Sanitized tool output shaped into trace-friendly JSON, before the inline
/// cap. Larger results reference the transcript row instead of storing this
/// value inline, so it is computed once and only kept when the inline form wins
/// the size race in [`ToolExecutor::execute`].
pub(crate) fn tool_output_to_trace_value_uncapped(output: &ToolOutput) -> Value {
    use serde_json::Map;
    match output {
        ToolOutput::Text(s) => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("text".into()));
            m.insert("text".into(), Value::String(s.clone()));
            Value::Object(m)
        }
        ToolOutput::Error(s) => {
            let mut m = Map::new();
            m.insert("type".into(), Value::String("error".into()));
            m.insert("error".into(), Value::String(s.clone()));
            Value::Object(m)
        }
        ToolOutput::Json(v) => match v {
            Value::Object(map) => {
                // Match the existing on-disk shape: type-tag injected
                // into the inner object alongside the original fields.
                let mut m = map.clone();
                m.insert("type".into(), Value::String("json".into()));
                Value::Object(m)
            }
            _ => {
                let mut m = Map::new();
                m.insert("type".into(), Value::String("json".into()));
                m.insert("value".into(), v.clone());
                Value::Object(m)
            }
        },
        ToolOutput::WithAttachments { .. } | ToolOutput::MultiModalText { .. } => {
            // Struct variants serialize correctly under
            // `#[serde(tag = "type")]`; defer to the derive so we
            // keep the historical shape exactly.
            serde_json::to_value(output).unwrap_or(Value::Null)
        }
    }
}

/// Cap the inline copy of a tool result at the same byte budget the LLM
/// transcript uses. Larger results reference that transcript row instead; this
/// bounds the inline copy kept for the smaller results that stay inline.
///
/// Capping happens here, after the variant-shaping match, so no variant can
/// bypass it: the struct variants (`WithAttachments` / `MultiModalText`) route
/// through `serde_json::to_value` and would slip past a per-variant cap.
/// Attachments themselves are `BlobRef` pointers, not bytes, so only the
/// text-bearing payload is ever large.
///
/// Returns the untruncated serialized length when it cut, for
/// `ToolCallResult::output_truncated_from`.
#[cfg(test)]
fn cap_trace_output(value: Value) -> (Value, Option<usize>) {
    let full_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    cap_trace_output_with_len(value, full_len)
}

pub(crate) fn cap_trace_output_with_len(value: Value, full_len: usize) -> (Value, Option<usize>) {
    use baybo_model::MAX_TOOL_OUTPUT_BYTES;

    if full_len <= MAX_TOOL_OUTPUT_BYTES {
        return (value, None);
    }

    // Over budget. Keep the type tag (the viewer keys off it) and replace the
    // body with a capped rendering of whatever carried the bytes.
    let Value::Object(map) = &value else {
        return (value, None);
    };
    let tag = map
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("json")
        .to_string();
    // The field the payload actually lives in, per the shapes built above.
    let (field, body) = match tag.as_str() {
        "text" => (
            "text",
            map.get("text").and_then(Value::as_str).map(String::from),
        ),
        "error" => (
            "error",
            map.get("error").and_then(Value::as_str).map(String::from),
        ),
        _ => ("text", None),
    };
    let body = body.unwrap_or_else(|| {
        // `json` and the struct variants have no single string field — render
        // the whole thing and cap that.
        serde_json::to_string(&value).unwrap_or_default()
    });

    // The budget is on the row as PERSISTED, i.e. the serialized JSON —
    // escaping (`\n` → `\\n`, `"` → `\\"`, control chars → `\uXXXX`) inflates
    // a raw-byte cut by up to ~6x, so trim against the serialized candidate
    // until it fits. The proportional shrink converges in a couple of rounds;
    // `min(cut - 1, …)` makes every round strictly smaller, so it terminates.
    let mut cut = MAX_TOOL_OUTPUT_BYTES.min(body.len());
    loop {
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut out = serde_json::Map::new();
        out.insert("type".into(), Value::String(tag.clone()));
        out.insert(field.into(), Value::String(body[..cut].to_string()));
        let candidate = Value::Object(out);
        let serialized = serde_json::to_string(&candidate)
            .map(|s| s.len())
            .unwrap_or(0);
        if serialized <= MAX_TOOL_OUTPUT_BYTES || cut == 0 {
            return (candidate, Some(full_len));
        }
        cut = (cut.saturating_mul(MAX_TOOL_OUTPUT_BYTES) / serialized).min(cut - 1);
    }
}

/// A finished tool call: whatever the call produced, plus the approval decision
/// it went through when it raised a prompt.
///
/// `output` keeps the call's own `Result` rather than making the whole struct
/// one, because `approval` must survive EVERY exit — a denial, a tool error, a
/// timeout — not just success. A mid-call denial (a tool that asks through
/// [`baybo_tools::ApprovalHandle`] and turns the refusal into its own error) is
/// exactly the case that loses its verdict otherwise: it reaches the loop as an
/// untyped error, indistinguishable from a crash.
pub struct ExecutedTool {
    pub output: anyhow::Result<ToolOutput>,
    /// `None` when the call never prompted (covered by a prior grant, or an
    /// ungated tool). When a call prompts more than once, the LAST decision
    /// wins — it is the one that shaped the outcome.
    pub approval: Option<ApprovalDecision>,
}

/// The out-of-band media blocks a `WithAttachments` / `MultiModalText` result
/// carries beside its text. They never enter the `ToolResult` content, so a
/// transcript-backed span keeps them on its `PersistedToolCallOutput` to
/// preserve the trace panel's blob list. Every other variant has none.
fn tool_output_media(output: &ToolOutput) -> (Vec<ContentBlock>, Vec<ContentBlock>) {
    match output {
        ToolOutput::WithAttachments { attachments, .. } => (attachments.clone(), Vec::new()),
        ToolOutput::MultiModalText { llm_images, .. } => (Vec::new(), llm_images.clone()),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The two board facts an issue run's tool call reads: where the
/// repository is, and what the agent working it is called.
///
/// `projects` is a one-method port rather than the store, because that is
/// the whole of what a tool call is entitled to ask a board.
pub struct BoardStores {
    pub projects: Arc<dyn ProjectRepo>,
    pub agents: Arc<dyn AgentProfileStore>,
}

/// Executes tools with trust-level validation, approval gating, and
/// observability recording.
pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    gate_map: Arc<ApprovalGateMap>,
    security_gateway: Arc<SecurityGateway>,
    /// Sandbox FS scope (= `<workspace>/work/`). Forwarded to
    /// [`ToolContext::workspace_root`].
    workspace_root: PathBuf,
    /// Layout addresses anchored at the actual workspace root.
    /// Forwarded to [`ToolContext::workspace_paths`].
    workspace_paths: baybo_workspace::WorkspacePaths,
    sandbox_runner: Option<Arc<dyn SandboxRunner>>,
    sandbox_bypass_reason: Option<String>,
    /// Resolver for virtual reads (e.g. the session transcript), forwarded into
    /// every [`ToolContext`] this executor builds; `ReadTool` consults it before
    /// the filesystem. `None` ⇒ no virtual reads wired.
    virtual_reads: Option<Arc<dyn VirtualReadResolver>>,
    /// Runtime hook for backgrounding a slow command, forwarded into every
    /// [`ToolContext`] this executor builds. Whether a given call may use it
    /// is the per-turn [`ToolContext::background_eligible`] gate, not the
    /// presence of this handle. `None` ⇒ no manager wired (argv-mode boots,
    /// tests), so commands keep kill-on-timeout regardless.
    background_jobs: Option<Arc<dyn baybo_tools::BackgroundJobSink>>,
    /// Control surface for the `JobList` / `JobStop` tools. Wired like
    /// [`Self::background_jobs`] (same runtime component implements both).
    background_control: Option<Arc<dyn baybo_tools::BackgroundJobControl>>,
    /// Board stores, consulted only for a session whose trigger is an
    /// issue: the checkout it works in, and the handle its commits are
    /// authored as. Both resolved per call rather than held on the session,
    /// because a session's issue is stable but the project's workdir and
    /// the agent's team membership are rows a user can edit. `None` ⇒ no
    /// board wired (argv-mode boots, tests), and issue sessions then behave
    /// like ordinary ones.
    board: Option<BoardStores>,
    #[cfg(test)]
    after_authorization_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ToolExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        gate_map: Arc<ApprovalGateMap>,
        security_gateway: Arc<SecurityGateway>,
        workspace_root: PathBuf,
        workspace_paths: baybo_workspace::WorkspacePaths,
        sandbox_runner: Option<Arc<dyn SandboxRunner>>,
        sandbox_bypass_reason: Option<String>,
        virtual_reads: Option<Arc<dyn VirtualReadResolver>>,
        background_jobs: Option<Arc<dyn baybo_tools::BackgroundJobSink>>,
        background_control: Option<Arc<dyn baybo_tools::BackgroundJobControl>>,
        board: Option<BoardStores>,
    ) -> Self {
        Self {
            tool_registry,
            gate_map,
            security_gateway,
            workspace_root,
            workspace_paths,
            sandbox_runner,
            sandbox_bypass_reason,
            virtual_reads,
            background_jobs,
            background_control,
            board,
            #[cfg(test)]
            after_authorization_hook: None,
        }
    }

    #[cfg(test)]
    fn with_after_authorization_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.after_authorization_hook = Some(hook);
        self
    }

    /// Scrub leak-detector matches out of every text field carried in
    /// a [`ToolEventPayload`] before it lands in the trace store.
    /// Fixed-size scalars (status codes, byte counts, durations) are
    /// untouched. Sanitize failures are logged at debug — the event
    /// is still emitted with whatever the gateway returned, falling
    /// through to the original string on Err.
    async fn sanitize_tool_event_payload(&self, payload: &mut ToolEventPayload) {
        match payload {
            ToolEventPayload::Phase { .. } => {}
            ToolEventPayload::HttpFetch { body_preview, .. } => {
                if let Some(text) = body_preview.as_mut() {
                    self.sanitize_in_place(text).await;
                }
            }
            ToolEventPayload::LlmCall { input, output, .. } => {
                self.sanitize_in_place(input).await;
                self.sanitize_in_place(output).await;
            }
            ToolEventPayload::ParseFailure { command } => {
                self.sanitize_in_place(command).await;
            }
        }
    }

    async fn sanitize_in_place(&self, text: &mut String) {
        match self.security_gateway.sanitize_stream_fragment(text).await {
            Ok(scrubbed) => *text = scrubbed,
            Err(e) => {
                debug!(error = %e, "sanitize_tool_event_payload: sanitize_stream_fragment failed");
            }
        }
    }

    async fn issue_checkout(&self, trigger: &baybo_model::TriggerSource) -> Option<Checkout> {
        let baybo_model::TriggerSource::Issue {
            project_id, number, ..
        } = trigger
        else {
            return None;
        };
        let workdir = self.board.as_ref()?.projects.workdir(project_id).await?;
        let root = worktree::worktree_root(&self.workspace_paths, project_id, *number);

        let work_dir = self.workspace_root.clone();
        let root = self.bindable(&root, |p| admits_worktree(p, &work_dir))?;
        let repo = self.bindable(&workdir, |p| admits_repo(p, &self.workspace_paths))?;
        Some(Checkout { root, repo })
    }

    fn bindable(
        &self,
        path: &Path,
        admit: impl Fn(&Path) -> Result<(), String>,
    ) -> Option<PathBuf> {
        let resolved = match path.canonicalize() {
            Ok(resolved) => resolved,
            Err(e) => {
                debug!(path = %path.display(), error = %e, "checkout path does not resolve");
                return None;
            }
        };
        if !resolved.is_dir() {
            debug!(path = %resolved.display(), "checkout path is not a directory");
            return None;
        }
        if let Err(why) = admit(&resolved) {
            tracing::warn!(
                requested = %path.display(),
                resolved = %resolved.display(),
                reason = %why,
                "refusing to bind a checkout path into the sandbox"
            );
            return None;
        }
        Some(resolved)
    }

    /// Validate that the tool's trust level permits execution with its declared capabilities.
    fn validate_trust(&self, tool_name: &str, manifest: &ToolManifest) -> anyhow::Result<()> {
        match manifest.trust_level {
            TrustLevel::Untrusted => {
                anyhow::bail!(
                    "security: tool '{}' has Untrusted trust level and cannot be auto-executed",
                    tool_name
                );
            }
            TrustLevel::Installed => {
                for cap in &manifest.capabilities {
                    match cap {
                        ToolCapability::WriteFile => {
                            anyhow::bail!(
                                "security: tool '{}' is Installed but declares WriteFile capability; \
                                 requires Trusted level",
                                tool_name
                            );
                        }
                        ToolCapability::ExecCommand => {
                            anyhow::bail!(
                                "security: tool '{}' is Installed but declares ExecCommand capability; \
                                 requires Trusted level",
                                tool_name
                            );
                        }
                        _ => {}
                    }
                }
            }
            TrustLevel::Trusted => {
                // Trusted tools are allowed all capabilities
            }
        }
        Ok(())
    }

    /// Execute a tool call inside the given `step`, with full
    /// observability and approval gating.
    ///
    /// Tool calls live as `Span`s under their parent agent-loop `Step`
    /// (see `docs/modules/trace.md`). `triggering_llm_span` is the LLM
    /// span that emitted the `tool_use` block; `parallel_group` ties
    /// concurrent siblings together; `turn_id` is the parent turn (for
    /// the WAL log).
    ///
    /// `approved_resources` is a shared, mutable set of session-scoped
    /// approvals. The executor reads it to check coverage and writes to
    /// it on `ApproveAlways`, so concurrent tool calls within a turn
    /// see each other's grants immediately.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        tool_name: &str,
        params: Value,
        session_id: &SessionId,
        // What started this session. Reaches the tool as
        // [`ToolContext::session_trigger`]; `CronCreate` reads it so a turn
        // created inside a cron fire inherits the fire's origin conversation.
        session_trigger: &baybo_model::TriggerSource,
        // The agent this session runs as — the tool's memory partition and
        // its `Skill` overlay. Passed per call (not held on the executor,
        // which is process-wide) exactly like `session_trigger`.
        agent_id: &baybo_model::AgentProfileId,
        user: &User,
        approved_resources: &Arc<Mutex<Vec<ApprovedResource>>>,
        recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        triggering_llm_span: Option<SpanId>,
        tool_use_id: String,
        parallel_group: Option<ParallelGroup>,
        _parent_turn_for_log: Option<TurnId>,
        cancel_token: CancellationToken,
        notifier: Option<Arc<dyn baybo_tools::SessionNotifier>>,
        // `None` ⇒ tool's `ctx.lite_llm` is unset (argv-mode / older tests).
        bind_source: Option<&Arc<BillableLlm>>,
        // Whether this turn may create background work — forwarded verbatim
        // to `ToolContext::background_eligible`, which documents the rule.
        background_eligible: bool,
        // Per-session read-before-write tracker. `Read` records into it;
        // `Edit`/`Write` enforce against it. Cheap to clone (one `Arc`).
        read_tracker: ReadTracker,
        // Handle a cron-fire tool (`report_nothing`) flips to suppress this
        // fire's notification. `None` for every turn that cannot be silenced.
        notify_silence: Option<baybo_tools::NotifySilence>,
        // Present only on the first turn started by a cron trigger. Never
        // derived from the session trigger, which remains Cron on later replies.
        initial_cron: Option<InitialCronToolContext>,
    ) -> ExecutedTool {
        debug!(tool = tool_name, "executing tool");

        let registration = self.tool_registry.get_with_manifest(tool_name);
        if let Some((_, Some(manifest))) = registration.as_ref() {
            if let Err(e) = self.validate_trust(tool_name, manifest) {
                return ExecutedTool {
                    output: Err(e),
                    approval: None,
                };
            }
            // The channel-restricted tool is already omitted from the LLM's
            // list for this session; refusing here closes the gap a
            // hallucinated or skill-body-prompted name would slip through.
            if !manifest.allows_channel(&user.channel) {
                return ExecutedTool {
                    output: Err(anyhow::anyhow!(
                        "security: tool '{}' is not available on channel '{}'",
                        tool_name,
                        user.channel.as_str()
                    )),
                    approval: None,
                };
            }
        }
        // Registry omission is not an enforcement boundary for hallucinated calls.
        if let Some((tool, _)) = registration.as_ref()
            && !tool.trigger_scope().allows_trigger(session_trigger)
        {
            return ExecutedTool {
                output: Err(anyhow::anyhow!(
                    "security: tool '{}' is not available to this session",
                    tool_name
                )),
                approval: None,
            };
        }
        let (pinned_tool, pinned_manifest) = match registration {
            Some((tool, manifest)) => (Some(tool), manifest),
            None => (None, None),
        };

        let turn_id = step.turn_id;
        let tool_name_owned = tool_name.to_string();
        // Reborrow the cancel token so the closure can move its
        // ownership into `ctx` while `with_span` keeps a borrow for
        // the cancel-aware close-on-Err path.
        let cancel_for_close = cancel_token.clone();
        let cancel_for_gate = cancel_token.clone();

        // The call's approval decision, written from inside the span body and
        // read back after it — including on the paths where the body returns
        // `Err` (denial, tool error, timeout) and the decision would otherwise
        // die with the closure.
        let approval_slot: Arc<Mutex<Option<ApprovalDecision>>> = Arc::new(Mutex::new(None));
        let approval_sink = Arc::clone(&approval_slot);

        // Open the tool span up front so denials and approval failures still
        // appear in the trace tree. The handle carries the begin-time kind, so
        // close-time only supplies the result: a `Persisted` pointer keyed on
        // this call's begin-time `tool_use_id`, or an inline copy when that is
        // smaller. The pointer is resolved on read against the transcript row
        // `AgentLoop` appends after this returns.
        let output = crate::runtime::scope::with_span(
            recorder,
            step,
            turn_id,
            SpanKind::ToolCall {
                begin: ToolCallBegin {
                    tool_name: tool_name_owned.clone(),
                    // ToolManifest does not yet carry an artifact hash.
                    triggered_by: triggering_llm_span.map(|llm_span_id| ToolCallOrigin {
                        llm_span_id,
                        tool_use_id: tool_use_id.clone(),
                    }),
                    params: params.clone(),
                },
                result: None,
            },
            parallel_group,
            Some((&cancel_for_close, baybo_turn::CancelReason::ParentCancelled)),
            |span_handle| async move {
                let mut event_seq: u32 = 0;

                // Approval gate: derive resource accesses from the tool
                // and check them against the session's cached approvals.
                // Also pull the tool's declared `max_timeout` (default
                // 30 s) so the executor uses the right deadline below.
                let (accesses, call_label, effective_timeout, output_source) = pinned_tool
                    .as_ref()
                    .map(|tool| {
                        (
                            tool.accessed_resources(&params),
                            tool.call_label(&params),
                            tool.max_timeout(),
                            tool.output_source(),
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            Vec::new(),
                            None,
                            Duration::from_secs(30),
                            baybo_tools::OutputSource::default(),
                        )
                    });

                let mcp_metadata = pinned_tool.as_ref().and_then(|tool| tool.mcp_metadata());
                let exact_mcp_grant = initial_cron.as_ref().is_some_and(|context| {
                    mcp_metadata.as_ref().is_some_and(|metadata| {
                        metadata.tool_name == tool_name_owned
                            && context.grants(&tool_name_owned, &metadata.transport_identity)
                    })
                });
                let initial_cron_requires_mcp_grant =
                    initial_cron.is_some() && mcp_metadata.is_some() && !exact_mcp_grant;
                let uncovered: Vec<ResourceAccess> = {
                    let approved = approved_resources.lock();
                    accesses
                        .iter()
                        .filter(|acc| {
                            if matches!(acc, ResourceAccess::ReadFile { .. }) {
                                return false;
                            }
                            if approved.iter().any(|ar| ar.covers(acc)) {
                                return false;
                            }
                            !(exact_mcp_grant
                                && mcp_metadata.as_ref().is_some_and(|metadata| {
                                    metadata.transport_accesses.contains(acc)
                                }))
                        })
                        .cloned()
                        .collect()
                };
                #[cfg(test)]
                if let Some(hook) = &self.after_authorization_hook {
                    hook();
                }

                if initial_cron_requires_mcp_grant || !uncovered.is_empty() {
                    if initial_cron.is_some() {
                        *approval_sink.lock() = Some(ApprovalDecision::Deny);
                        for access in &uncovered {
                            let _ = recorder
                                .emit_event(
                                    span_handle.span_id,
                                    event_seq,
                                    SpanEventKind::Approval {
                                        decision: ApprovalDecision::Deny,
                                        resource: access.clone(),
                                    },
                                )
                                .await;
                            event_seq += 1;
                        }
                        let reason = match (&mcp_metadata, exact_mcp_grant) {
                            (Some(_), false) => String::from(
                                "the initial scheduled turn is unattended and this exact MCP tool \
                                 and transport configuration is not granted on the cron job; grant \
                                 the current namespaced operation and transport identity in the \
                                 job's settings before its next fire",
                            ),
                            (Some(_), true) => String::from(
                                "the cron job's exact MCP tool grant covers only that operation's \
                                 transport access; the call requested additional approval-gated \
                                 resources, which an unattended initial turn cannot approve",
                            ),
                            (None, _) => String::from(
                                "the initial scheduled turn is unattended and cannot request new \
                                 approval for a non-MCP tool; run this operation from a user reply \
                                 or expose it through an exact MCP tool grant",
                            ),
                        };
                        return Err(anyhow::Error::new(ToolError::Denied {
                            tool: tool_name_owned.clone(),
                            reason,
                        }));
                    }
                    let gate = self.gate_map.get(&user.channel, session_id);
                    let request = gate.request(ApprovalRequest {
                        call_id: Uuid::new_v4().to_string(),
                        tool_call_id: Some(tool_use_id.clone()),
                        session_id: session_id.clone(),
                        user_id: user.id.clone(),
                        tool: tool_name_owned.clone(),
                        accesses: uncovered.clone(),
                        params_preview: preview_params(&params, APPROVAL_PARAMS_PREVIEW_LEN),
                        description: call_label.clone(),
                    });
                    // A prompt parks the turn for up to the gate's timeout, so
                    // it MUST answer to `/stop` — otherwise cancelling a turn
                    // that is waiting on the user hangs until the gate denies
                    // itself minutes later, and the client keeps showing a card
                    // for a turn that is already unwinding. Dropping the request
                    // future also unqueues it (the gate's RAII cleanup), so the
                    // prompt disappears everywhere rather than lingering.
                    let outcome = tokio::select! {
                        outcome = request => outcome,
                        // Cancellation abandons the unanswered prompt.
                        _ = cancel_for_gate.cancelled() => ApprovalOutcome::abandoned(),
                    };
                    let decision = outcome.decision;
                    *approval_sink.lock() = Some(decision);
                    for access in &uncovered {
                        let _ = recorder
                            .emit_event(
                                span_handle.span_id,
                                event_seq,
                                SpanEventKind::Approval {
                                    decision,
                                    resource: access.clone(),
                                },
                            )
                            .await;
                        event_seq += 1;
                    }
                    match decision {
                        ApprovalDecision::Approve => {
                            info!(tool = %tool_name_owned, "tool call approved once");
                        }
                        ApprovalDecision::ApproveAlways => {
                            info!(tool = %tool_name_owned, "tool call approved always");
                            let mut approved = approved_resources.lock();
                            for access in &uncovered {
                                let entry = access.to_approved();
                                if !approved.iter().any(|existing| existing == &entry) {
                                    approved.push(entry);
                                }
                            }
                        }
                        ApprovalDecision::Deny => {
                            return Err(anyhow::Error::new(ToolError::Denied {
                                tool: tool_name_owned.clone(),
                                reason: refusal_reason(outcome.resolution).to_string(),
                            }));
                        }
                    }
                    // Approved — but the turn may have been cancelled WHILE the
                    // prompt was up (the select above only fires when the token
                    // trips before the answer). Running now would execute a
                    // side-effecting call on behalf of a turn the user already
                    // stopped.
                    if cancel_for_gate.is_cancelled() {
                        return Err(anyhow::anyhow!(
                            "tool '{tool_name_owned}' cancelled while awaiting approval"
                        ));
                    }
                }

                // Build per-call sandbox adapter for tools declaring
                // ExecCommand.
                let uses_exec_command = pinned_manifest.as_ref().is_some_and(|manifest| {
                    manifest.capabilities.contains(&ToolCapability::ExecCommand)
                });
                let checkout = self.issue_checkout(session_trigger).await;
                let agent_handle = match (&checkout, &self.board, session_trigger.project()) {
                    (Some(_), Some(board), Some(project)) => {
                        board_handle(&board.agents, agent_id, project).await
                    }
                    _ => None,
                };
                // Re-resolved per call rather than cached, so an operator who
                // edits their git identity is obeyed by the next command. It
                // costs one `git config` against the sandbox spawn below.
                let checkout_git_config = match (uses_exec_command, &checkout) {
                    (true, Some(checkout)) => worktree::ensure_identity_config(checkout).await,
                    _ => None,
                };
                let sandbox: Option<Arc<dyn ExecSandbox>> = if uses_exec_command {
                    self.sandbox_runner.as_ref().map(|runner| {
                        let PermissiveScope { extra_root, denied } = permissive_scope(
                            self.workspace_paths.root(),
                            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
                            std::env::var_os("BAYBO_HOME").map(PathBuf::from).as_deref(),
                            &self.workspace_root,
                        );
                        let readable = baybo_tools::shell_reachable_workspace_roots(
                            &self.workspace_paths,
                            agent_id,
                        );
                        let checkout_paths = checkout
                            .as_ref()
                            .map(|c| vec![c.root.clone(), c.repo.clone()])
                            .unwrap_or_default();
                        Arc::new(
                            SandboxAdapter::new(
                                Arc::clone(runner),
                                self.workspace_root.clone(),
                                NetworkPolicy::All,
                            )
                            .with_permissive_filesystem(extra_root, denied)
                            .with_readable_paths(readable)
                            .with_writable_paths(checkout_paths),
                        ) as Arc<dyn ExecSandbox>
                    })
                } else {
                    None
                };
                let sandbox_bypass_reason = if uses_exec_command && sandbox.is_none() {
                    self.sandbox_bypass_reason.clone()
                } else {
                    None
                };

                // Mid-execution approval handle. The probe clone shares the
                // handle's decision cell, so after the tool returns we can
                // read back what any mid-call prompt decided.
                let approval_gate: Arc<dyn ApprovalGate> = if initial_cron.is_some() {
                    Arc::new(AutoDenyGate)
                } else {
                    self.gate_map.get(&user.channel, session_id)
                };
                let approval = ApprovalHandle::new(
                    approval_gate,
                    Arc::clone(approved_resources),
                    Some(tool_use_id.clone()),
                );
                let approval_probe = approval.clone();

                // Per-call event sink. Drained into span events after
                // tool execution so phase timers and richer artifacts
                // (e.g. WebFetch's HTTP response summary, side-LLM I/O)
                // surface in the trace UI.
                let event_sink = Arc::new(SpanEventRecorder::new());

                // Build tool context. The token comes from the agent
                // loop's per-turn cancel tree — tripping it (via
                // TurnLifecycle::cancel or a parent subagent's cascade)
                // signals the running tool. The billed-LLM handle (if
                // any) is bound to this exact tool span so a tool that
                // makes a side-LLM call (e.g. `WebFetch`'s extraction)
                // sees its `cost_records` row attributed to the running
                // tool span, not a synthetic placeholder. `bind_source`
                // is the agent loop's LITE client — see
                // `ToolContext::lite_llm`.
                let lite_llm: Option<Arc<dyn BilledChat>> = bind_source.map(|guarded| {
                    let bound = guarded.bind(Attribution {
                        user_id: user.id.clone(),
                        session_id: session_id.clone(),
                        turn_id,
                        span_id: span_handle.span_id,
                        reason: baybo_llm::CallReason::Tool(tool_name_owned.clone()),
                    });
                    Arc::new(crate::runtime::billed_chat::BilledChatRunner::new(
                        bound,
                        Arc::clone(&self.security_gateway),
                    )) as Arc<dyn BilledChat>
                });
                let ctx = ToolContext {
                    session_id: session_id.clone(),
                    session_trigger: session_trigger.clone(),
                    agent_id: agent_id.clone(),
                    turn_id,
                    span_id: span_handle.span_id,
                    user: user.clone(),
                    timeout: effective_timeout,
                    cancellation_token: cancel_token,
                    workspace_root: self.workspace_root.clone(),
                    workspace_paths: self.workspace_paths.clone(),
                    sandbox,
                    sandbox_bypass_reason,
                    approval: Some(approval),
                    notifier: notifier.clone(),
                    events: Arc::clone(&event_sink) as Arc<dyn baybo_tools::ToolEventSink>,
                    lite_llm,
                    secrets: Some(
                        Arc::clone(&self.security_gateway) as Arc<dyn baybo_tools::SecretAccess>
                    ),
                    // `ReadTool` consults this before the filesystem; today
                    // just the session transcript. Cheap clone (Arc bump).
                    virtual_reads: self.virtual_reads.clone(),
                    // Read-before-write tracker is always wired for the
                    // interactive agent loop, so `Edit`/`Write` enforce the
                    // contract here (unlike the background-summary pass).
                    read_tracker: Some(read_tracker),
                    background_eligible,
                    background_jobs: self.background_jobs.clone(),
                    background_control: self.background_control.clone(),
                    notify_silence,
                    checkout_root: checkout.map(|c| c.root),
                    agent_handle,
                    checkout_git_config,
                };

                // Reveal placeholders in the tool's arguments just
                // before execution. The pre-reveal `params` is what the
                // trace + approval surfaces saw; the tool itself
                // receives plaintext for its API call.
                let mut params_revealed = params.clone();
                self.security_gateway
                    .reveal_in_value(&mut params_revealed)
                    .await
                    .map_err(|e| anyhow::anyhow!("reveal_in_value: {e}"))?;

                // Execute with timeout enforcement. A `Read` of a virtual path
                // (e.g. the session transcript) is served inside `ReadTool` from
                // `ctx.virtual_reads` before it touches the filesystem.
                let outer_deadline = ctx.timeout + APPROVAL_HEADROOM;
                let execution = async {
                    let tool = pinned_tool.as_ref().ok_or_else(|| {
                        ToolError::NotFound(format!("tool not found: {tool_name_owned}"))
                    })?;
                    tool.execute(params_revealed, &ctx).await
                };
                let result = tokio::time::timeout(outer_deadline, execution).await;

                // A tool can raise its own prompts mid-execution (through
                // `ctx.approval`), and those come AFTER the pre-execute gate —
                // so the handle's last decision wins. Read it here, before any
                // of the arms below can return early: a mid-call denial reaches
                // the loop as the tool's own error, and without this the verdict
                // would be indistinguishable from a crash.
                if let Some(decision) = approval_probe.last_decision() {
                    *approval_sink.lock() = Some(decision);
                }

                // Flush tool-reported events as span events, regardless
                // of whether the tool succeeded, failed, or hit the
                // outer timeout. Each entry continues the span-local
                // seq sequence that approval emission left off at.
                // Text fields inside the payload are sanitized so any
                // leaked secrets get minted into placeholders before
                // they hit the trace store. Drops on emit_event errors
                // are intentional — events are non-load-bearing.
                for (action, mut payload) in event_sink.drain() {
                    self.sanitize_tool_event_payload(&mut payload).await;
                    let _ = recorder
                        .emit_event(
                            span_handle.span_id,
                            event_seq,
                            SpanEventKind::ToolEvent { action, payload },
                        )
                        .await;
                    event_seq += 1;
                }

                match result {
                    Ok(Ok(mut output)) => {
                        // Defensive sanitize before result flows into
                        // trace / memory / next LLM call.
                        let span_id = span_handle.span_id.to_string();
                        self.security_gateway
                            .sanitize_tool_output(
                                &mut output,
                                crate::security::ScanOrigin {
                                    session_id: Some(session_id.as_str()),
                                    tool: Some(&tool_name_owned),
                                    span_id: Some(&span_id),
                                    provenance: crate::security::OutputProvenance::classify(
                                        output_source,
                                        &accesses,
                                        ctx.checkout_root.as_deref(),
                                    ),
                                },
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("sanitize_tool_output: {e}"))?;
                        let success = !matches!(output, ToolOutput::Error(_));
                        let outcome = if success {
                            LifecycleOutcome::Ok
                        } else {
                            // Surface the actual error text in the
                            // outcome reason so the trace row label
                            // ("failed: …") is informative without
                            // having to drill into the Output panel.
                            // The body has already been sanitized by
                            // sanitize_tool_output above; we only
                            // truncate for display sanity.
                            let reason = match &output {
                                ToolOutput::Error(s) => {
                                    let trimmed = s.trim();
                                    if trimmed.is_empty() {
                                        "tool returned empty error output".to_string()
                                    } else {
                                        truncate_for_reason(trimmed, FAILURE_REASON_MAX_BYTES)
                                    }
                                }
                                _ => "tool returned error output".to_string(),
                            };
                            LifecycleOutcome::Failed { reason }
                        };

                        // Close-time result. The span points at the transcript
                        // row `AgentLoop` will append for this `tool_use_id`
                        // (resolved lazily on read), unless an inline copy of
                        // the shaped value serializes smaller — the common case
                        // for tiny outputs, which then need no join. Media
                        // blocks ride the pointer since they never enter the
                        // `ToolResult` text.
                        use baybo_model::MAX_TOOL_OUTPUT_BYTES;
                        let output_value = tool_output_to_trace_value_uncapped(&output);
                        let full_len = serde_json::to_string(&output_value).map_or(0, |s| s.len());
                        let output_truncated_from =
                            (full_len > MAX_TOOL_OUTPUT_BYTES).then_some(full_len);
                        let (inline_value, _) = cap_trace_output_with_len(output_value, full_len);
                        let inline = ToolCallOutput::inline(inline_value);
                        let (attachments, llm_images) = tool_output_media(&output);
                        let reference = ToolCallOutput::persisted(PersistedToolCallOutput::new(
                            tool_use_id.clone(),
                            attachments,
                            llm_images,
                        ));
                        let serialized_len = |value: &ToolCallOutput| -> usize {
                            serde_json::to_string(value).map_or(usize::MAX, |s| s.len())
                        };
                        let trace_output = if serialized_len(&reference) < serialized_len(&inline) {
                            reference
                        } else {
                            inline
                        };
                        Ok((
                            SpanFinalize::ToolCall(ToolCallResult {
                                output: trace_output,
                                success,
                                output_truncated_from,
                            }),
                            outcome,
                            output,
                        ))
                    }
                    Ok(Err(e)) => {
                        // Surface the *sanitized* message: this Err
                        // bubbles into `with_span`, which writes
                        // `e.to_string()` into the span's
                        // `Failed { reason }` and from there into
                        // persisted trace storage. Returning the raw
                        // error would leak any secrets in the original
                        // text. The error's TYPE is lost here (a
                        // `ToolError::Denied` a tool raised from a mid-call
                        // prompt included) — the decision that shaped it rides
                        // `approval_slot` instead, read back by the caller.
                        let raw = e.to_string();
                        let sanitized = self
                            .security_gateway
                            .sanitize_error(&raw)
                            .await
                            .unwrap_or(raw);
                        Err(anyhow::anyhow!(sanitized))
                    }
                    Err(_) => Err(anyhow::anyhow!(
                        "tool '{}' exceeded timeout ({:?})",
                        tool_name_owned,
                        ctx.timeout
                    )),
                }
            },
        )
        .await;

        ExecutedTool {
            output,
            approval: *approval_slot.lock(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutedTool, InitialCronToolContext, ToolExecutor, admits_repo, admits_worktree,
        permissive_scope,
    };
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn the_mask_covers_the_live_workspace_not_just_the_default_location() {
        let home = Path::new("/home/u");
        let live = Path::new("/home/u/projects/baybo-ws");
        let scope = permissive_scope(live, Some(home), None, Path::new("/unused"));
        assert_eq!(scope.extra_root, home);
        assert!(
            scope.denied.contains(&live.to_path_buf()),
            "the live workspace root is unmasked: {:?}",
            scope.denied
        );
        assert!(
            scope.denied.contains(&home.join(".baybo")),
            "an operator who moved the workspace may still have state at the default location",
        );
        assert!(scope.denied.contains(&home.join(".ssh")));

        let scope = permissive_scope(
            live,
            Some(home),
            Some(Path::new("/srv/baybo")),
            Path::new("/unused"),
        );
        assert!(scope.denied.contains(&PathBuf::from("/srv/baybo")));
        assert!(scope.denied.contains(&live.to_path_buf()));

        let scope = permissive_scope(live, None, None, Path::new("/ws/work"));
        assert_eq!(scope.extra_root, Path::new("/ws/work"));
        assert_eq!(scope.denied, vec![live.to_path_buf()]);
    }

    #[test]
    fn a_worktree_that_resolves_outside_the_work_dir_is_not_bound() {
        let work = Path::new("/ws/work");
        assert!(admits_worktree(Path::new("/ws/work/projects/p/4"), work).is_ok());
        for escaped in ["/", "/etc", "/ws", "/home/u"] {
            assert!(
                admits_worktree(Path::new(escaped), work).is_err(),
                "a worktree resolving to {escaped} must not be bound"
            );
        }
    }

    #[test]
    fn a_repository_may_be_anywhere_except_over_the_system_or_the_workspace() {
        let ws = baybo_workspace::WorkspacePaths::new("/home/u/.baybo");
        assert!(admits_repo(Path::new("/data/kanban"), &ws).is_ok());
        assert!(admits_repo(Path::new("/home/u/code/app"), &ws).is_ok());
        for bad in ["/", "/usr", "/etc", "/home/u/.baybo", "/home/u"] {
            assert!(
                admits_repo(Path::new(bad), &ws).is_err(),
                "{bad} must not be bindable as a repository"
            );
        }
    }

    #[test]
    fn a_repository_inside_the_workspace_is_refused_unless_it_is_under_work() {
        let ws = baybo_workspace::WorkspacePaths::new("/home/u/.baybo");
        for bad in [
            "/home/u/.baybo/state",
            "/home/u/.baybo/.key",
            "/home/u/.baybo/personas/01J",
        ] {
            assert!(
                admits_repo(Path::new(bad), &ws).is_err(),
                "{bad} is inside the workspace and must not be bound as a repository"
            );
        }
        assert!(
            admits_repo(Path::new("/home/u/.baybo/work/importer"), &ws).is_ok(),
            "the auto-created workdir of a project with no repository must still bind"
        );
    }

    #[test]
    fn a_repo_that_contains_the_workspace_through_a_symlink_is_still_refused() {
        let real = tempfile::tempdir().expect("tempdir");
        let workspace = real.path().join(".baybo");
        std::fs::create_dir(&workspace).expect("workspace dir");
        let home = tempfile::tempdir().expect("tempdir");
        let link = home.path().join(".baybo");
        std::os::unix::fs::symlink(&workspace, &link).expect("symlink");
        let repo = real.path().canonicalize().expect("canonical repo");

        assert!(
            admits_repo(&repo, &baybo_workspace::WorkspacePaths::new(link)).is_err(),
            "the guard must see the workspace's real location however the root is spelled"
        );
    }

    fn profile_row(
        id: baybo_model::AgentProfileId,
        team: Option<baybo_model::TeamMembership>,
    ) -> baybo_store::AgentProfileRow {
        let now = chrono::Utc::now();
        baybo_store::AgentProfileRow {
            id,
            description: String::new(),
            avatar_blob_id: None,
            framework: baybo_model::AgentFramework::Baybo,
            llm: baybo_model::LlmPin::unpinned(),
            builtin: false,
            team,
            hired_by: None,
            deleted_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn a_team_members_handle_is_what_its_commits_are_credited_to() {
        let agents = baybo_store::test_support::MemoryAgentProfileStore::new();
        let id = baybo_model::AgentProfileId::generate();
        let board = baybo_model::ProjectId::generate();
        agents.insert(profile_row(
            id.clone(),
            Some(baybo_model::TeamMembership {
                project_id: board.clone(),
                handle: baybo_model::AgentHandle::parse("dev-1").expect("handle"),
            }),
        ));
        let agents: Arc<dyn AgentProfileStore> = Arc::new(agents);

        assert_eq!(
            super::board_handle(&agents, &id, &board)
                .await
                .map(|h| h.as_str().to_owned()),
            Some("dev-1".to_owned()),
        );
    }

    #[tokio::test]
    async fn a_removed_teammate_still_resolves_on_the_board_it_worked() {
        let agents = baybo_store::test_support::MemoryAgentProfileStore::new();
        let id = baybo_model::AgentProfileId::generate();
        let board = baybo_model::ProjectId::generate();
        let mut row = profile_row(
            id.clone(),
            Some(baybo_model::TeamMembership {
                project_id: board.clone(),
                handle: baybo_model::AgentHandle::parse("dev-1").expect("handle"),
            }),
        );
        row.deleted_at = Some(chrono::Utc::now());
        agents.insert(row);
        let agents: Arc<dyn AgentProfileStore> = Arc::new(agents);

        assert_eq!(
            super::board_handle(&agents, &id, &board)
                .await
                .map(|h| h.as_str().to_owned()),
            Some("dev-1".to_owned()),
        );
    }

    #[tokio::test]
    async fn an_agent_on_another_board_does_not_sign_this_ones_commits() {
        let agents = baybo_store::test_support::MemoryAgentProfileStore::new();
        let id = baybo_model::AgentProfileId::generate();
        agents.insert(profile_row(
            id.clone(),
            Some(baybo_model::TeamMembership {
                project_id: baybo_model::ProjectId::generate(),
                handle: baybo_model::AgentHandle::parse("dev-1").expect("handle"),
            }),
        ));
        let agents: Arc<dyn AgentProfileStore> = Arc::new(agents);

        assert_eq!(
            super::board_handle(&agents, &id, &baybo_model::ProjectId::generate()).await,
            None,
        );
    }

    #[tokio::test]
    async fn an_agent_with_no_board_and_a_row_that_is_gone_have_no_handle() {
        let agents = baybo_store::test_support::MemoryAgentProfileStore::new();
        let global = baybo_model::AgentProfileId::generate();
        let board = baybo_model::ProjectId::generate();
        agents.insert(profile_row(global.clone(), None));
        let agents: Arc<dyn AgentProfileStore> = Arc::new(agents);

        assert_eq!(super::board_handle(&agents, &global, &board).await, None);
        assert_eq!(
            super::board_handle(&agents, &baybo_model::AgentProfileId::generate(), &board).await,
            None,
        );
    }

    const FIXTURE_HANDLE: &str = "dev-1";
    const CAPTURE_TOOL: &str = "capture_ctx";

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SeenContext {
        checkout_root: Option<PathBuf>,
        agent_handle: Option<baybo_model::AgentHandle>,
        checkout_git_config: Option<PathBuf>,
    }

    type Seen = Arc<Mutex<Option<SeenContext>>>;

    struct CaptureTool {
        seen: Seen,
    }

    #[async_trait::async_trait]
    impl baybo_tools::Tool for CaptureTool {
        fn name(&self) -> &str {
            CAPTURE_TOOL
        }
        fn description(&self) -> String {
            "records its context".to_owned()
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn execute(
            &self,
            _params: Value,
            ctx: &ToolContext,
        ) -> baybo_tools::Result<ToolOutput> {
            *self.seen.lock() = Some(SeenContext {
                checkout_root: ctx.checkout_root.clone(),
                agent_handle: ctx.agent_handle.clone(),
                checkout_git_config: ctx.checkout_git_config.clone(),
            });
            Ok(ToolOutput::Text(String::new()))
        }
    }

    struct IssueRunFixture {
        executor: ToolExecutor,
        trigger: baybo_model::TriggerSource,
        agent: baybo_model::AgentProfileId,
        worktree: PathBuf,
        repo: PathBuf,
        seen: Seen,
        _real: tempfile::TempDir,
        _home: tempfile::TempDir,
        _repo_dir: tempfile::TempDir,
    }

    async fn issue_run_fixture() -> IssueRunFixture {
        let real = tempfile::tempdir().expect("tempdir");
        let workspace = real.path().join(".baybo");
        std::fs::create_dir(&workspace).expect("workspace dir");
        let home = tempfile::tempdir().expect("tempdir");
        let link = home.path().join(".baybo");
        std::os::unix::fs::symlink(&workspace, &link).expect("symlink");

        let paths = baybo_workspace::WorkspacePaths::new(link.clone());
        let paths_for_board = paths.clone();
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        let repo_dir = tempfile::tempdir().expect("tempdir");
        let store = baybo_storage::Store::open(workspace.join("storage.db"))
            .await
            .expect("store");

        let project_id = baybo_model::ProjectId::generate();
        let number = 4;
        let now = chrono::Utc::now();
        store
            .project
            .create_project(&baybo_store::project::ProjectRow {
                id: project_id.clone(),
                name: "Importer".to_owned(),
                description: String::new(),
                workdir: repo_dir.path().to_string_lossy().into_owned(),
                daily_budget: None,
                daily_budget_tokens: None,
                max_parallel_issue_runs: baybo_store::project::DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                rules_changed_at: now,
                archived_at: None,
                created_at: now,
                updated_at: now,
                agents_may_merge: false,
            })
            .await
            .expect("project");

        let agent = baybo_model::AgentProfileId::generate();
        store
            .agent_profile
            .create(&profile_row(
                agent.clone(),
                Some(baybo_model::TeamMembership {
                    project_id: project_id.clone(),
                    handle: baybo_model::AgentHandle::parse(FIXTURE_HANDLE).expect("handle"),
                }),
            ))
            .await
            .expect("teammate");

        let worktree = worktree::worktree_root(&paths, &project_id, number);
        std::fs::create_dir_all(&worktree).expect("worktree");

        let seen = Arc::new(Mutex::new(None));
        let mut registry = ToolRegistry::new();
        registry.register(
            Arc::new(CaptureTool {
                seen: Arc::clone(&seen),
            }),
            ToolManifest {
                name: CAPTURE_TOOL.to_owned(),
                description: "records its context".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object", "properties": {}}),
                capabilities: vec![ToolCapability::ExecCommand],
                channels: Vec::new(),
            },
        );

        let key = baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("key");
        let security_gateway = Arc::new(SecurityGateway::new(
            Arc::new(baybo_security::LeakDetector::with_default_rules()),
            Arc::new(baybo_security::SecretVault::new(
                key,
                Arc::new(baybo_security::test_support::MemorySecretStore::new()),
            )),
        ));

        let executor = ToolExecutor::new(
            Arc::new(registry),
            Arc::new(ApprovalGateMap::new()),
            security_gateway,
            sandbox_root,
            paths,
            None,
            None,
            None,
            None,
            None,
            Some(BoardStores {
                projects: Arc::new(baybo_project::ProjectManager::new(
                    Arc::clone(&store.project),
                    Arc::clone(&store.agent_profile),
                    Arc::clone(&store.blob),
                    paths_for_board,
                    Arc::new(baybo_project::NoopProjectEvents),
                    baybo_project::no_dispatch(),
                    baybo_project::no_stopper(),
                )),
                agents: Arc::clone(&store.agent_profile),
            }),
        );

        IssueRunFixture {
            executor,
            trigger: baybo_model::TriggerSource::Issue {
                project_id,
                issue_id: baybo_model::IssueId::generate(),
                number,
            },
            agent,
            worktree: worktree.canonicalize().expect("canonical worktree"),
            repo: repo_dir.path().canonicalize().expect("canonical repo"),
            seen,
            _real: real,
            _home: home,
            _repo_dir: repo_dir,
        }
    }

    #[tokio::test]
    async fn a_worktree_under_a_symlinked_workspace_is_still_bound() {
        let fixture = issue_run_fixture().await;

        let checkout = fixture
            .executor
            .issue_checkout(&fixture.trigger)
            .await
            .expect("an issue session gets its checkout");
        assert_eq!(checkout.root, fixture.worktree);
        assert_eq!(checkout.repo, fixture.repo);
    }

    #[tokio::test]
    async fn an_issue_runs_tool_call_carries_the_agents_handle() {
        let fixture = issue_run_fixture().await;
        let session_id = SessionId::from("sess-issue-4");
        let recorder = Arc::new(SpanRecorder::new(
            session_id.clone(),
            "u1".to_owned(),
            Arc::new(baybo_trace::test_support::MemoryTraceStore::new()),
            baybo_trace::TraceEventStream::default(),
        ));
        let step = recorder
            .begin_step(TurnId::new(), baybo_trace::StepKind::LlmIteration)
            .await
            .expect("step");
        let user = User {
            id: "u1".to_owned(),
            name: None,
            channel: baybo_model::ChannelType::tui(),
        };

        let executed = fixture
            .executor
            .execute(
                CAPTURE_TOOL,
                json!({}),
                &session_id,
                &fixture.trigger,
                &fixture.agent,
                &user,
                &Arc::new(Mutex::new(Vec::new())),
                &recorder,
                &step,
                None,
                "tool-use-1".to_owned(),
                None,
                None,
                CancellationToken::new(),
                None,
                None,
                false,
                ReadTracker::default(),
                None,
                None,
            )
            .await;
        assert!(executed.output.is_ok(), "the capture tool ran");

        let seen = fixture.seen.lock().clone().expect("the tool saw a context");
        assert_eq!(
            seen.checkout_root,
            Some(fixture.worktree.clone()),
            "an issue run's tool works in the card's worktree"
        );
        assert_eq!(
            seen.agent_handle.map(|h| h.as_str().to_owned()),
            Some(FIXTURE_HANDLE.to_owned()),
            "and credits the agent working the card, not its ULID"
        );

        // The identity is resolved on the host and handed over already
        // answered; the shell only ever sees the file.
        let expected = seen
            .checkout_root
            .as_ref()
            .expect("worktree")
            .with_extension("gitconfig");
        assert_eq!(seen.checkout_git_config.as_ref(), Some(&expected));
        assert!(
            std::fs::read_to_string(&expected)
                .expect("the identity config is materialised")
                .contains("[user]"),
            "an issue run's shell must have somebody to commit as"
        );
    }

    use super::*;
    use baybo_tools::ToolEventSink;
    use baybo_trace::SPAN_EVENT_TEXT_MAX_BYTES;
    use serde_json::json;

    /// The sink is the enforcement point: emitters used to be trusted to
    /// truncate their own payloads (WebFetch shipped a whole 96 KiB page), and
    /// the only backstop dropped the *entire* entry, so an oversized field
    /// could silently swallow later audit events. Bound it here instead.
    #[test]
    fn event_sink_truncates_oversized_payload_fields() {
        let sink = SpanEventRecorder::new();
        sink.emit(
            "llm_summary",
            ToolEventPayload::LlmCall {
                model: "m".into(),
                input: "p".repeat(SPAN_EVENT_TEXT_MAX_BYTES * 4),
                output: "o".repeat(SPAN_EVENT_TEXT_MAX_BYTES * 4),
            },
        );
        // A later event must still land — the old whole-entry drop made this
        // order-dependent.
        sink.emit(
            "parse_failure",
            ToolEventPayload::ParseFailure {
                command: "rm -rf /".into(),
            },
        );

        let items = sink.drain();
        assert_eq!(
            items.len(),
            2,
            "the oversized event must not evict later ones"
        );
        let ToolEventPayload::LlmCall { input, output, .. } = &items[0].1 else {
            panic!("expected the LlmCall payload first");
        };
        for (name, field) in [("input", input), ("output", output)] {
            assert!(
                field.len() < SPAN_EVENT_TEXT_MAX_BYTES * 2,
                "{name} must be bounded, got {} bytes",
                field.len()
            );
            assert!(
                field.contains("truncated"),
                "{name} must say it was truncated rather than look complete"
            );
        }
        assert!(
            matches!(&items[1].1, ToolEventPayload::ParseFailure { command } if command == "rm -rf /"),
            "the audit event must survive intact"
        );
    }

    use baybo_model::MAX_TOOL_OUTPUT_BYTES;

    #[test]
    fn trace_value_preserves_text_payload() {
        let (v, truncated) = tool_output_to_trace_value(&ToolOutput::Text("hello world".into()));
        assert_eq!(v, json!({ "type": "text", "text": "hello world" }));
        assert_eq!(truncated, None);
    }

    #[test]
    fn trace_value_preserves_error_payload() {
        let (v, truncated) =
            tool_output_to_trace_value(&ToolOutput::Error("read_page failed: …".into()));
        assert_eq!(
            v,
            json!({ "type": "error", "error": "read_page failed: …" })
        );
        assert_eq!(truncated, None);
    }

    #[test]
    fn trace_value_preserves_json_object_shape() {
        // NowTool-shaped output: an object payload. The historical
        // shape — type tag flattened into the inner map — is what the
        // web UI is already showing for spans like Now, so changing
        // it would invalidate older traces.
        let (v, truncated) = tool_output_to_trace_value(&ToolOutput::Json(json!({
            "utc": "2026-01-01T00:00:00Z",
            "timezone": "UTC",
        })));
        assert_eq!(
            v,
            json!({
                "type": "json",
                "utc": "2026-01-01T00:00:00Z",
                "timezone": "UTC",
            })
        );
        assert_eq!(truncated, None);
    }

    #[test]
    fn trace_value_wraps_non_object_json_payload() {
        let (v, truncated) = tool_output_to_trace_value(&ToolOutput::Json(json!("scalar")));
        assert_eq!(v, json!({ "type": "json", "value": "scalar" }));
        assert_eq!(truncated, None);
    }

    /// A 285 KB `Grep` result used to be stored verbatim in the span — a third
    /// copy of bytes the model never saw (its transcript copy is capped and
    /// spilled). The span copy now rides the same budget and says so.
    #[test]
    fn trace_value_caps_oversized_text_and_records_original_size() {
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES * 3);
        let (v, truncated) = tool_output_to_trace_value(&ToolOutput::Text(big));

        let text = v["text"].as_str().expect("text field survives the cap");
        assert_eq!(v["type"], "text", "the type tag must survive");
        assert!(
            text.len() <= MAX_TOOL_OUTPUT_BYTES,
            "capped text must fit the budget, got {}",
            text.len()
        );
        let original = truncated.expect("truncation must be reported");
        assert!(
            original > MAX_TOOL_OUTPUT_BYTES * 3,
            "the reported original length must be the full serialized size"
        );
    }

    /// The struct variants route through `serde_json::to_value`, so a cap
    /// applied per-variant to Text/Error/Json only would let them through.
    #[test]
    fn trace_value_caps_oversized_struct_variant() {
        let (v, truncated) = tool_output_to_trace_value(&ToolOutput::MultiModalText {
            text: "y".repeat(MAX_TOOL_OUTPUT_BYTES * 2),
            llm_images: vec![],
        });
        assert!(
            serde_json::to_string(&v).unwrap().len() <= MAX_TOOL_OUTPUT_BYTES + 64,
            "no variant may bypass the cap"
        );
        assert!(truncated.is_some());
    }

    /// Truncation must not split a multi-byte character.
    #[test]
    fn trace_value_cap_lands_on_a_char_boundary() {
        let big = "★".repeat(MAX_TOOL_OUTPUT_BYTES); // 3 bytes each
        let (v, _) = tool_output_to_trace_value(&ToolOutput::Text(big));
        let text = v["text"].as_str().expect("still valid UTF-8 text");
        assert!(text.chars().all(|c| c == '★'), "no mangled char at the cut");
    }

    /// The budget is on the row as persisted — the serialized JSON. Escaping
    /// doubles a newline (`\n` → `\\n`), so a raw-byte cut of escape-heavy
    /// content used to overshoot the stated bound by ~2x (and up to ~6x for
    /// control characters).
    #[test]
    fn trace_value_cap_bounds_the_serialized_size() {
        let big = "line with \"quotes\" and \\slashes\\\n".repeat(MAX_TOOL_OUTPUT_BYTES / 16);
        let (v, truncated) = tool_output_to_trace_value(&ToolOutput::Text(big));
        let serialized = serde_json::to_string(&v).unwrap().len();
        assert!(
            serialized <= MAX_TOOL_OUTPUT_BYTES,
            "the persisted row must fit the budget after escaping, got {serialized}"
        );
        assert!(truncated.is_some());
    }

    const GATED_TOOL: &str = "gated_exec";

    struct GatedTool;

    #[async_trait::async_trait]
    impl baybo_tools::Tool for GatedTool {
        fn name(&self) -> &str {
            GATED_TOOL
        }
        fn description(&self) -> String {
            "asks before it runs".to_owned()
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
            vec![ResourceAccess::ExecCommand {
                command: "x".to_owned(),
            }]
        }
        async fn execute(
            &self,
            _params: Value,
            _ctx: &ToolContext,
        ) -> baybo_tools::Result<ToolOutput> {
            Ok(ToolOutput::Text("ran".to_owned()))
        }
    }

    struct FixedOutcomeGate(ApprovalOutcome);

    #[async_trait::async_trait]
    impl baybo_tools::ApprovalGate for FixedOutcomeGate {
        async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
            self.0
        }
    }

    struct CountingApproveGate {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl baybo_tools::ApprovalGate for CountingApproveGate {
        async fn request(&self, _req: ApprovalRequest) -> ApprovalOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ApprovalOutcome::answered(ApprovalDecision::Approve)
        }
    }

    const SELECTED_MCP_TOOL: &str = "lighthouse/run_audit";
    const SIBLING_MCP_TOOL: &str = "lighthouse/get_report";

    struct TypedMcpTool {
        namespaced_name: &'static str,
        upstream_name: &'static str,
        transport_identity: McpTransportIdentity,
        declares_transport_access: bool,
        extra_access: bool,
        mid_execution_access: bool,
        ran: Arc<AtomicBool>,
    }

    impl TypedMcpTool {
        fn transport_access() -> ResourceAccess {
            ResourceAccess::Http {
                host: "mcp.example".to_string(),
            }
        }
    }

    #[async_trait::async_trait]
    impl baybo_tools::Tool for TypedMcpTool {
        fn name(&self) -> &str {
            self.namespaced_name
        }

        fn description(&self) -> String {
            "typed MCP authorization test".to_string()
        }

        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
            let mut accesses = if self.declares_transport_access {
                vec![Self::transport_access()]
            } else {
                Vec::new()
            };
            if self.extra_access {
                accesses.push(ResourceAccess::WriteFile {
                    path: PathBuf::from("/tmp/not-transport"),
                });
            }
            accesses
        }

        fn mcp_metadata(&self) -> Option<baybo_tools::mcp::McpToolMetadata> {
            Some(baybo_tools::mcp::McpToolMetadata {
                tool_name: self.namespaced_name.to_string(),
                server_name: "lighthouse".to_string(),
                upstream_name: self.upstream_name.to_string(),
                transport_identity: self.transport_identity.clone(),
                transport_accesses: if self.declares_transport_access {
                    vec![Self::transport_access()]
                } else {
                    Vec::new()
                },
            })
        }

        async fn execute(
            &self,
            _params: Value,
            ctx: &ToolContext,
        ) -> baybo_tools::Result<ToolOutput> {
            if self.mid_execution_access {
                let outcome = ctx
                    .approval
                    .as_ref()
                    .expect("approval handle")
                    .request(
                        self.name(),
                        &ctx.session_id,
                        &ctx.user,
                        vec![ResourceAccess::WriteFile {
                            path: PathBuf::from("/tmp/discovered-late"),
                        }],
                        "{}".to_string(),
                    )
                    .await;
                if outcome.decision == ApprovalDecision::Deny {
                    return Err(ToolError::Denied {
                        tool: self.name().to_string(),
                        reason: "mid-execution access denied".to_string(),
                    });
                }
            }
            self.ran.store(true, Ordering::SeqCst);
            Ok(ToolOutput::Text("ran".to_string()))
        }
    }

    fn cron_trigger() -> baybo_model::TriggerSource {
        baybo_model::TriggerSource::Cron {
            cron_job_id: "cron-1".to_string(),
            origin_session_id: None,
            conversation: true,
            job_title: Some("MCP job".to_string()),
            project_id: None,
        }
    }

    async fn execute_authorization_tool(
        tool: Arc<dyn baybo_tools::Tool>,
        gate: Arc<dyn baybo_tools::ApprovalGate>,
        approved_resources: Arc<Mutex<Vec<ApprovedResource>>>,
        initial_cron: Option<InitialCronToolContext>,
    ) -> ExecutedTool {
        let tool_name = tool.name().to_string();
        let mut registry = ToolRegistry::new();
        registry.register(tool, authorization_manifest(&tool_name));
        execute_authorization_registration(
            Arc::new(registry),
            tool_name,
            gate,
            approved_resources,
            initial_cron,
            None,
        )
        .await
    }

    fn authorization_manifest(tool_name: &str) -> ToolManifest {
        ToolManifest {
            name: tool_name.to_string(),
            description: "authorization test".to_string(),
            trust_level: TrustLevel::Trusted,
            parameters_schema: json!({"type": "object", "properties": {}}),
            capabilities: vec![ToolCapability::Http, ToolCapability::WriteFile],
            channels: Vec::new(),
        }
    }

    async fn execute_authorization_registration(
        registry: Arc<ToolRegistry>,
        tool_name: String,
        gate: Arc<dyn baybo_tools::ApprovalGate>,
        approved_resources: Arc<Mutex<Vec<ApprovedResource>>>,
        initial_cron: Option<InitialCronToolContext>,
        after_authorization_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> ExecutedTool {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        let gates = ApprovalGateMap::new();
        gates.insert(baybo_model::ChannelType::tui(), gate);
        let key = baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("key");
        let security_gateway = Arc::new(SecurityGateway::new(
            Arc::new(baybo_security::LeakDetector::with_default_rules()),
            Arc::new(baybo_security::SecretVault::new(
                key,
                Arc::new(baybo_security::test_support::MemorySecretStore::new()),
            )),
        ));
        let mut executor = ToolExecutor::new(
            registry,
            Arc::new(gates),
            security_gateway,
            sandbox_root,
            paths,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        if let Some(hook) = after_authorization_hook {
            executor = executor.with_after_authorization_hook(hook);
        }

        let session_id = SessionId::from("sess-cron-authorization");
        let recorder = Arc::new(SpanRecorder::new(
            session_id.clone(),
            "u1".to_string(),
            Arc::new(baybo_trace::test_support::MemoryTraceStore::new()),
            baybo_trace::TraceEventStream::default(),
        ));
        let step = recorder
            .begin_step(TurnId::new(), baybo_trace::StepKind::LlmIteration)
            .await
            .expect("step");
        let user = User {
            id: "u1".to_string(),
            name: None,
            channel: baybo_model::ChannelType::tui(),
        };

        executor
            .execute(
                &tool_name,
                json!({}),
                &session_id,
                &cron_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &approved_resources,
                &recorder,
                &step,
                None,
                "tool-use-cron".to_string(),
                None,
                None,
                CancellationToken::new(),
                None,
                None,
                false,
                ReadTracker::default(),
                None,
                initial_cron,
            )
            .await
    }

    fn counting_gate() -> (Arc<AtomicUsize>, Arc<dyn baybo_tools::ApprovalGate>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate: Arc<dyn baybo_tools::ApprovalGate> = Arc::new(CountingApproveGate {
            calls: Arc::clone(&calls),
        });
        (calls, gate)
    }

    #[tokio::test]
    async fn exact_mcp_tool_grant_bypasses_only_its_transport_access() {
        let identity = McpTransportIdentity::from_sha256([10; 32]);
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let approved = Arc::new(Mutex::new(Vec::new()));
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: identity.clone(),
                declares_transport_access: true,
                extra_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::clone(&approved),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                identity,
            )])),
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, None);
        assert!(ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
        assert!(approved.lock().is_empty());
    }

    #[tokio::test]
    async fn initial_cron_denies_ungranted_zero_access_typed_mcp() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: McpTransportIdentity::from_sha256([19; 32]),
                declares_transport_access: false,
                extra_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            Some(InitialCronToolContext::default()),
        )
        .await;

        let error = executed
            .output
            .expect_err("zero-access typed MCP still requires an exact grant");
        assert!(
            error
                .to_string()
                .contains("exact MCP tool and transport configuration"),
            "{error}"
        );
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn initial_cron_allows_granted_zero_access_typed_mcp() {
        let identity = McpTransportIdentity::from_sha256([20; 32]);
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: identity.clone(),
                declares_transport_access: false,
                extra_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                identity,
            )])),
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, None);
        assert!(ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dynamic_replacement_after_authorization_executes_the_pinned_tool() {
        let granted_identity = McpTransportIdentity::from_sha256([17; 32]);
        let replacement_identity = McpTransportIdentity::from_sha256([18; 32]);
        let original_ran = Arc::new(AtomicBool::new(false));
        let replacement_ran = Arc::new(AtomicBool::new(false));
        let original: Arc<dyn baybo_tools::Tool> = Arc::new(TypedMcpTool {
            namespaced_name: SELECTED_MCP_TOOL,
            upstream_name: "run_audit",
            transport_identity: granted_identity.clone(),
            declares_transport_access: true,
            extra_access: false,
            mid_execution_access: false,
            ran: Arc::clone(&original_ran),
        });
        let replacement: Arc<dyn baybo_tools::Tool> = Arc::new(TypedMcpTool {
            namespaced_name: SELECTED_MCP_TOOL,
            upstream_name: "run_audit",
            transport_identity: replacement_identity.clone(),
            declares_transport_access: true,
            extra_access: true,
            mid_execution_access: false,
            ran: Arc::clone(&replacement_ran),
        });
        let registry = Arc::new(ToolRegistry::new());
        registry.replace_source(
            "lighthouse",
            vec![(original, authorization_manifest(SELECTED_MCP_TOOL))],
        );

        let barrier = Arc::new(Barrier::new(2));
        let hook_barrier = Arc::clone(&barrier);
        let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            hook_barrier.wait();
            hook_barrier.wait();
        });
        let replacement_registry = Arc::clone(&registry);
        let replacement_barrier = Arc::clone(&barrier);
        let replacement_task = tokio::task::spawn_blocking(move || {
            replacement_barrier.wait();
            replacement_registry.replace_source(
                "lighthouse",
                vec![(replacement, authorization_manifest(SELECTED_MCP_TOOL))],
            );
            replacement_barrier.wait();
        });

        let (gate_calls, gate) = counting_gate();
        let approved = Arc::new(Mutex::new(Vec::new()));
        let executed = execute_authorization_registration(
            Arc::clone(&registry),
            SELECTED_MCP_TOOL.to_string(),
            gate,
            Arc::clone(&approved),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                granted_identity,
            )])),
            Some(hook),
        )
        .await;
        replacement_task.await.expect("replacement task");

        assert_eq!(
            registry
                .mcp_metadata(SELECTED_MCP_TOOL)
                .expect("replacement remains registered")
                .transport_identity,
            replacement_identity,
            "the registry replacement must complete before the pinned call resumes"
        );
        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, None);
        assert!(
            original_ran.load(Ordering::SeqCst),
            "the generation whose identity and accesses were authorized must execute"
        );
        assert!(
            !replacement_ran.load(Ordering::SeqCst),
            "a same-name replacement must not execute under the prior generation's grant"
        );
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
        assert!(approved.lock().is_empty());
    }

    #[tokio::test]
    async fn initial_cron_denies_another_tool_on_the_same_mcp_transport() {
        let identity = McpTransportIdentity::from_sha256([11; 32]);
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SIBLING_MCP_TOOL,
                upstream_name: "get_report",
                transport_identity: identity.clone(),
                declares_transport_access: true,
                extra_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                identity,
            )])),
        )
        .await;

        let error = executed
            .output
            .expect_err("a sibling operation must not inherit the grant");
        assert!(
            error
                .to_string()
                .contains("exact MCP tool and transport configuration"),
            "{error}"
        );
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn initial_cron_denies_the_granted_tool_after_its_transport_identity_changes() {
        let granted_identity = McpTransportIdentity::from_sha256([12; 32]);
        let current_identity = McpTransportIdentity::from_sha256([13; 32]);
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: current_identity,
                declares_transport_access: true,
                extra_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                granted_identity,
            )])),
        )
        .await;

        let error = executed
            .output
            .expect_err("a changed transport identity must invalidate the grant");
        assert!(
            error
                .to_string()
                .contains("exact MCP tool and transport configuration"),
            "{error}"
        );
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn exact_mcp_tool_grant_does_not_cover_non_transport_access() {
        let identity = McpTransportIdentity::from_sha256([14; 32]);
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: identity.clone(),
                declares_transport_access: true,
                extra_access: true,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                identity,
            )])),
        )
        .await;

        let error = executed.output.expect_err("extra access must fail");
        assert!(error.to_string().contains("covers only"), "{error}");
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn initial_cron_denies_unapproved_non_mcp_access_without_prompting() {
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(GatedTool),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            Some(InitialCronToolContext::default()),
        )
        .await;

        let error = executed.output.expect_err("non-MCP access must fail");
        assert!(error.to_string().contains("non-MCP tool"), "{error}");
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn initial_cron_auto_denies_access_discovered_during_mcp_execution() {
        let identity = McpTransportIdentity::from_sha256([15; 32]);
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let approved = Arc::new(Mutex::new(Vec::new()));
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: identity.clone(),
                declares_transport_access: true,
                extra_access: false,
                mid_execution_access: true,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::clone(&approved),
            Some(InitialCronToolContext::new(vec![McpToolGrant::new(
                SELECTED_MCP_TOOL,
                identity,
            )])),
        )
        .await;

        let error = executed
            .output
            .expect_err("mid-execution access must fail closed");
        assert!(error.to_string().contains("mid-execution access denied"));
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
        assert!(approved.lock().is_empty());
    }

    #[tokio::test]
    async fn later_reply_in_a_cron_session_uses_ordinary_approval() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let executed = execute_authorization_tool(
            Arc::new(TypedMcpTool {
                namespaced_name: SELECTED_MCP_TOOL,
                upstream_name: "run_audit",
                transport_identity: McpTransportIdentity::from_sha256([16; 32]),
                declares_transport_access: true,
                extra_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            None,
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, Some(ApprovalDecision::Approve));
        assert!(ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 1);
    }

    async fn gated_call_error(outcome: ApprovalOutcome) -> String {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        let mut registry = ToolRegistry::new();
        registry.register(
            Arc::new(GatedTool),
            ToolManifest {
                name: GATED_TOOL.to_owned(),
                description: "asks before it runs".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object", "properties": {}}),
                capabilities: vec![ToolCapability::ExecCommand],
                channels: Vec::new(),
            },
        );

        let gates = ApprovalGateMap::new();
        gates.insert(
            baybo_model::ChannelType::tui(),
            Arc::new(FixedOutcomeGate(outcome)),
        );

        let key = baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("key");
        let security_gateway = Arc::new(SecurityGateway::new(
            Arc::new(baybo_security::LeakDetector::with_default_rules()),
            Arc::new(baybo_security::SecretVault::new(
                key,
                Arc::new(baybo_security::test_support::MemorySecretStore::new()),
            )),
        ));

        let executor = ToolExecutor::new(
            Arc::new(registry),
            Arc::new(gates),
            security_gateway,
            sandbox_root,
            paths,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let session_id = SessionId::from("sess-gate");
        let recorder = Arc::new(SpanRecorder::new(
            session_id.clone(),
            "u1".to_owned(),
            Arc::new(baybo_trace::test_support::MemoryTraceStore::new()),
            baybo_trace::TraceEventStream::default(),
        ));
        let step = recorder
            .begin_step(TurnId::new(), baybo_trace::StepKind::LlmIteration)
            .await
            .expect("step");
        let user = User {
            id: "u1".to_owned(),
            name: None,
            channel: baybo_model::ChannelType::tui(),
        };

        let executed = executor
            .execute(
                GATED_TOOL,
                json!({}),
                &session_id,
                &baybo_model::TriggerSource::User,
                &baybo_model::AgentProfileId::generate(),
                &user,
                &Arc::new(Mutex::new(Vec::new())),
                &recorder,
                &step,
                None,
                "tool-use-1".to_owned(),
                None,
                None,
                CancellationToken::new(),
                None,
                None,
                false,
                ReadTracker::default(),
                None,
                None,
            )
            .await;
        executed
            .output
            .expect_err("a denied gate must not let the tool run")
            .to_string()
    }

    #[tokio::test]
    async fn a_timed_out_gate_is_not_reported_to_the_model_as_a_human_no() {
        let msg = gated_call_error(ApprovalOutcome::timed_out()).await;
        assert!(msg.contains("expired"), "got {msg}");
        assert!(!msg.contains("said no"), "got {msg}");
        assert!(!msg.contains("user denied approval"), "got {msg}");
    }

    #[tokio::test]
    async fn a_human_no_still_reads_as_a_human_no() {
        let msg = gated_call_error(ApprovalOutcome::answered(ApprovalDecision::Deny)).await;
        assert!(
            msg.contains("a human saw this prompt and said no"),
            "got {msg}"
        );
    }
}
