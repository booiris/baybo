use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use baybo_llm::{Attribution, BillableLlm, BilledChat};
#[cfg(test)]
use baybo_model::TrustLevel;
use baybo_model::human_ask::AskPhase;
use baybo_model::{ContentBlock, ParallelGroup, SessionId, SpanId, TurnId, User};

use baybo_sandbox::{NetworkPolicy, SandboxRunner, default_sensitive_denylist};
use baybo_tools::{
    ApprovalDecision, ApprovalGate, ApprovalGateMap, ApprovalHandle, ApprovalOutcome,
    ApprovalRequest, ApprovalResolution, ApprovedResource, AskCancel, AskSite, ExecSandbox,
    ParkGauge, PreExecuteAsk, PreExecuteCheck, ReadTracker, ResourceAccess, ToolCapability,
    ToolContext, ToolError, ToolManifest, ToolOutput, ToolRegistry, TurnTokens,
    VirtualReadResolver, approval::preview_params, refusal_reason,
};
use baybo_trace::{
    LifecycleOutcome, PersistedToolCallOutput, SpanEventKind, SpanFinalize, SpanKind, SpanRecorder,
    StepHandle, ToolCallBegin, ToolCallOrigin, ToolCallOutput, ToolCallResult, ToolEventPayload,
};
use serde_json::Value;
use tracing::{debug, info};

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

/// Slack the outer tool deadline gives a tool past its own `ctx.timeout`, so
/// work a tool does after its inner deadline fires (Bash detaching a timed-out
/// command into the background) is not raced by the executor's timer. Time a
/// call spends parked on an ask is not counted at all; see
/// [`PausableDeadline`].
pub(crate) const TOOL_DEADLINE_GRACE: Duration = Duration::from_secs(15);

/// How often [`PausableDeadline`] re-reads the call's [`ParkGauge`]. Bounds
/// how far a park transition can be misattributed.
const PARK_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The wall-clock budget of one tool body. Elapsed time accrues only while
/// the call's [`ParkGauge`] reads zero, so a tool parked on a human (an
/// approval raised mid-execution) can wait indefinitely without being killed.
struct PausableDeadline {
    budget: Duration,
    gauge: ParkGauge,
}

impl PausableDeadline {
    fn new(budget: Duration, gauge: ParkGauge) -> Self {
        Self { budget, gauge }
    }

    /// Resolves once `budget` of un-parked time has passed.
    async fn expired(self) {
        let mut spent = Duration::ZERO;
        let mut last = tokio::time::Instant::now();
        loop {
            let remaining = self.budget.saturating_sub(spent);
            if remaining.is_zero() {
                return;
            }
            tokio::time::sleep(remaining.min(PARK_POLL_INTERVAL)).await;
            let now = tokio::time::Instant::now();
            if !self.gauge.is_parked() {
                spent += now - last;
            }
            last = now;
        }
    }

    /// Run `fut` under the deadline; `None` when it expired first.
    async fn run<F: std::future::Future>(self, fut: F) -> Option<F::Output> {
        tokio::select! {
            out = fut => Some(out),
            () = self.expired() => None,
        }
    }
}

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

/// How the pre-execute approval gate treats a call's uncovered accesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreExecuteGate {
    /// Raise an ask on the turn's gate, as every live call does.
    Ask,
    /// A human already answered this call's ask and the turn prelude is
    /// re-executing it: take `decision` as the gate's answer without raising
    /// anything.
    ///
    /// - [`AskPhase::BeforeExecute`]: evaluates `pre_execute_ask` in
    ///   `PreDecided` mode, verifies the original resources still cover the
    ///   request, seeds cacheable `ApproveAlways` grants and runs `Tool::execute`.
    /// - [`AskPhase::DuringExecuteResumable`]: skips `pre_execute_ask` (never
    ///   caches) and runs `DuringExecuteResumer::resume_during_execute` via
    ///   `Tool::during_execute_resumer`.
    PreDecided {
        decision: ApprovalDecision,
        phase: AskPhase,
        accesses: Vec<ResourceAccess>,
    },
}

struct PreparedApproval {
    ask: PreExecuteAsk,
    authorized_accesses: Vec<ResourceAccess>,
}

impl PreparedApproval {
    fn retain_uncovered(&mut self, cache: &Mutex<Vec<ApprovedResource>>) {
        if self.ask.cacheable {
            let approved = cache.lock();
            self.ask.accesses.retain(|access| {
                !matches!(access, ResourceAccess::ReadFile { .. })
                    && !approved.iter().any(|grant| grant.covers(access))
            });
        }
    }

    fn validate_resume(&self, mode: &PreExecuteGate, tool: &str) -> anyhow::Result<()> {
        if let PreExecuteGate::PreDecided {
            decision, accesses, ..
        } = mode
            && *decision != ApprovalDecision::Deny
            && self.ask.accesses.iter().any(|access| {
                !accesses
                    .iter()
                    .any(|original| original.to_approved().covers(access))
            })
        {
            return Err(ToolError::Denied {
                tool: tool.to_owned(),
                reason: "approval requirements changed; retry the tool call for a new approval"
                    .to_owned(),
            }
            .into());
        }
        Ok(())
    }
}

fn require_approval(outcome: ApprovalOutcome, tool: &str, ctx: &ToolContext) -> anyhow::Result<()> {
    if outcome.decision == ApprovalDecision::Deny {
        return Err(ToolError::Denied {
            tool: tool.to_owned(),
            reason: refusal_reason(outcome.resolution).to_owned(),
        }
        .into());
    }
    if ctx.cancellation_token.is_cancelled() {
        anyhow::bail!("tool '{tool}' cancelled while awaiting approval");
    }
    info!(tool, decision = ?outcome.decision, "tool call approved");
    Ok(())
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
    /// The call's last ask came back torn down undecided
    /// ([`ApprovalResolution::Abandoned`]): its result is a cancel artefact,
    /// and the loop must claim the ask before appending it.
    pub abandoned: bool,
    /// The job this call handed to the background, if any — the agent loop
    /// counts a grouped one into the turn's cohort and records it as started
    /// by the turn.
    pub handed_over: Option<baybo_tools::background::HandedOver>,
}

/// What a turn lets its calls leave running past it, fixed for the turn.
/// Forwarded verbatim to the [`baybo_tools::ToolContext`] fields of the same
/// meaning, which document the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnBackground {
    /// [`baybo_tools::ToolContext::background_eligible`].
    pub eligible: bool,
    /// [`baybo_tools::ToolContext::overrun_to_background`].
    pub overrun_to_background: bool,
}

impl TurnBackground {
    /// What a turn of `kind` in `session` lets its calls leave running. A cron
    /// fire that overran would push "results will follow" and then the result,
    /// so its overruns are killed instead.
    pub(crate) fn for_turn(
        session: &baybo_model::Session,
        kind: baybo_turn::TurnInputKind,
    ) -> Self {
        Self {
            eligible: session.can_host_background_jobs(),
            overrun_to_background: !matches!(kind, baybo_turn::TurnInputKind::Cron),
        }
    }

    /// Nothing outlives the turn.
    #[cfg(test)]
    pub(crate) const NONE: Self = Self {
        eligible: false,
        overrun_to_background: false,
    };
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
    /// The gate every ask of an unattended turn goes to, in both phases:
    /// denies by policy and records the denial on the ledger.
    policy_deny: Arc<dyn ApprovalGate>,
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
    background_jobs: Option<Arc<dyn baybo_tools::background::BackgroundJobSink>>,
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
    /// Compiled `ToolInvoke` param validators, keyed by schema content —
    /// see [`InvokeValidatorCache`].
    invoke_validators: InvokeValidatorCache,
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
        background_jobs: Option<Arc<dyn baybo_tools::background::BackgroundJobSink>>,
        background_control: Option<Arc<dyn baybo_tools::BackgroundJobControl>>,
        board: Option<BoardStores>,
        policy_deny: Arc<dyn ApprovalGate>,
    ) -> Self {
        Self {
            tool_registry,
            gate_map,
            policy_deny,
            security_gateway,
            workspace_root,
            workspace_paths,
            sandbox_runner,
            sandbox_bypass_reason,
            virtual_reads,
            background_jobs,
            background_control,
            board,
            invoke_validators: InvokeValidatorCache::default(),
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

    /// `ToolInvoke` param pre-validation, run against a REVEALED copy of
    /// the params so constraints on secret-bearing fields judge the real
    /// value, not the placeholder the model holds. `Some(msg)` is the
    /// model-visible rejection, already re-masked and leak-scrubbed:
    /// 1. reveal a local copy (failure ⇒ skip — a gate that cannot see the
    ///    real values cannot judge them; execution does its own reveal);
    /// 2. validate via the compiled-schema cache;
    /// 3. compose the message — secret-path errors go generic, every
    ///    revealed value is exact-replaced back to its placeholder;
    /// 4. `sanitize_error` as belt-and-braces before anything escapes into
    ///    the transcript / trace.
    async fn validate_invoke_params(
        &self,
        tool: &dyn baybo_tools::Tool,
        params: &Value,
    ) -> Option<String> {
        let mut revealed = params.clone();
        if let Err(e) = self.security_gateway.reveal_in_value(&mut revealed).await {
            debug!(tool = tool.name(), error = %e, "invoke params reveal failed; skipping validation");
            return None;
        }
        let rejection = match self.invoke_validators.validate(tool, &revealed) {
            Ok(()) => return None,
            Err(rejection) => rejection,
        };
        let mut secrets = Vec::new();
        let mut path = String::new();
        revealed_secret_values(params, &revealed, &mut path, &mut secrets);
        let msg = compose_invoke_rejection(tool.name(), &rejection, &secrets);
        match self.security_gateway.sanitize_error(&msg).await {
            Ok(sanitized) => Some(sanitized),
            // Never let an unscrubbed message escape: fall back to a
            // value-free rejection.
            Err(e) => {
                debug!(tool = tool.name(), error = %e, "invoke rejection sanitize failed; withholding details");
                Some(format!(
                    "ToolInvoke params for '{}' failed schema validation \
                     (details withheld: error sanitization failed). Fetch the \
                     schema via ToolSearch and retry.",
                    tool.name()
                ))
            }
        }
    }

    /// Validate that the tool's trust level permits execution with its declared capabilities.
    fn validate_trust(&self, tool_name: &str, manifest: &ToolManifest) -> anyhow::Result<()> {
        manifest
            .validate_auto_execution()
            .map_err(|error| anyhow::anyhow!("security: tool '{}' {error}", tool_name))
    }

    async fn prepare_approval(
        &self,
        tool: Option<&dyn baybo_tools::Tool>,
        masked_params: &Value,
        revealed_params: &Value,
        ctx: &ToolContext,
        mode: &PreExecuteGate,
    ) -> anyhow::Result<Option<PreparedApproval>> {
        let Some(tool) = tool else {
            return Ok(None);
        };
        let check = match mode {
            PreExecuteGate::Ask => PreExecuteCheck::Evaluate,
            PreExecuteGate::PreDecided { .. } => PreExecuteCheck::PreDecided,
        };
        let Some(ask) = tool.pre_execute_ask(revealed_params, ctx, check).await else {
            return Ok(None);
        };
        let authorized_accesses = ask.accesses.clone();
        let mut value = serde_json::to_value(ask)?;
        self.security_gateway
            .sanitize_approval_value(&mut value, masked_params)
            .await?;
        Ok(Some(PreparedApproval {
            ask: serde_json::from_value(value)?,
            authorized_accesses,
        }))
    }

    /// Drain any pending tool-reported events into span events, sanitizing
    /// text payloads so leaked secrets never hit the trace store.
    async fn flush_tool_events(
        &self,
        event_sink: &SpanEventRecorder,
        recorder: &SpanRecorder,
        span_id: SpanId,
        event_seq: &mut u32,
    ) {
        for (action, mut payload) in event_sink.drain() {
            self.sanitize_tool_event_payload(&mut payload).await;
            let _ = recorder
                .emit_event(
                    span_id,
                    *event_seq,
                    SpanEventKind::ToolEvent { action, payload },
                )
                .await;
            *event_seq += 1;
        }
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
        // This call's cancel token (`turn`, a child of the turn's) and the
        // turn's release token. A parked ask reads both to tell a turn
        // cancel from a turn ended with its asks left pending.
        tokens: TurnTokens,
        notifier: Option<Arc<dyn baybo_tools::SessionNotifier>>,
        // `None` ⇒ tool's `ctx.lite_llm` is unset (argv-mode / older tests).
        bind_source: Option<&Arc<BillableLlm>>,
        // What this turn lets its calls leave running past it.
        background: TurnBackground,
        // Whether a human can answer this turn's asks — computed once per turn
        // by the agent loop from the turn's kind. Unattended calls send every
        // ask, in both phases, to the policy-deny gate. Tools never see it.
        attended: bool,
        // Per-session read-before-write tracker. `Read` records into it;
        // `Edit`/`Write` enforce against it. Cheap to clone (one `Arc`).
        read_tracker: ReadTracker,
        // Handle a cron-fire tool (`report_nothing`) flips to suppress this
        // fire's notification. `None` for every turn that cannot be silenced.
        notify_silence: Option<baybo_tools::NotifySilence>,
        // True when the agent loop unwrapped this call from a `ToolInvoke`
        // envelope. The model wrote these params from a `ToolSearch` result
        // rather than a provider-enforced schema, so the executor
        // pre-validates them against the pinned tool's schema and hands a
        // failing call the schema back for self-correction. Every security
        // door above and below applies identically either way.
        via_invoke: bool,
        pre_execute: PreExecuteGate,
    ) -> ExecutedTool {
        debug!(tool = tool_name, "executing tool");

        let registration = self.tool_registry.get_with_manifest(tool_name);
        if let Some((_, Some(manifest))) = registration.as_ref() {
            if let Err(e) = self.validate_trust(tool_name, manifest) {
                return ExecutedTool {
                    output: Err(e),
                    approval: None,
                    abandoned: false,
                    handed_over: None,
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
                    abandoned: false,
                    handed_over: None,
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
                abandoned: false,
                handed_over: None,
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
        let cancel_token = tokens.turn.clone();
        let cancel_for_close = cancel_token.clone();
        let ask_cancel = AskCancel::new(tokens);
        let park_gauge = ParkGauge::new();
        let gate: Arc<dyn ApprovalGate> = if attended {
            self.gate_map.get(&user.channel, session_id)
        } else {
            Arc::clone(&self.policy_deny)
        };

        // The call's approval outcome, written from inside the span body and
        // read back after it — including on the paths where the body returns
        // `Err` (denial, tool error, timeout) and the outcome would otherwise
        // die with the closure.
        let mut approval_outcome = None;
        let approval_result = &mut approval_outcome;
        let mut handed_over = None;
        let background_result = &mut handed_over;

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

                // `ToolInvoke`-routed params are validated before any
                // approval prompt fires: a call that cannot execute must not
                // cost the user a prompt. Validation runs on a revealed
                // copy (constraints must judge real values, not
                // placeholders); the reported message comes back re-masked
                // and sanitized.
                if via_invoke
                    && let Some(tool) = pinned_tool.as_ref()
                    && let Some(msg) = self.validate_invoke_params(tool.as_ref(), &params).await
                {
                    return Err(ToolError::InvalidParams(msg).into());
                }

                // Sync tool metadata needed across execution and output
                // classification (`declared_accesses` feeds `OutputProvenance`
                // for `DeclaredFiles` tools like `Read`).
                let (accesses, effective_timeout, output_source) = pinned_tool
                    .as_ref()
                    .map(|tool| {
                        (
                            tool.accessed_resources(&params),
                            tool.max_timeout(),
                            tool.output_source(),
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            Vec::new(),
                            Duration::from_secs(30),
                            baybo_tools::OutputSource::default(),
                        )
                    });

                // Build per-call context before `pre_execute_ask` so tools
                // whose pre-execute gate needs runtime services (e.g. Bash's
                // risk judge needing `lite_llm`, resolved cwd, `sandbox`
                // presence, and `events`; Skill's scoped lookup needing
                // `agent_id` and `user`) can run before execution.
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
                let approval = ApprovalHandle::new(
                    Arc::clone(&gate),
                    tool_use_id.clone(),
                    turn_id,
                    park_gauge.clone(),
                    ask_cancel.clone(),
                )
                .with_resumable_during_execute(
                    pinned_tool
                        .as_ref()
                        .is_some_and(|tool| tool.during_execute_resumer().is_some()),
                );
                let approval_probe = approval.clone();

                // Per-call event sink. Drained into span events after
                // `pre_execute_ask` and after tool execution so phase timers
                // and richer artifacts (risk judge calls, WebFetch HTTP
                // summaries, side-LLM I/O) surface in the trace UI even when
                // the pre-execute gate denies.
                let event_sink = Arc::new(SpanEventRecorder::new());

                // Build tool context. The token comes from the agent
                // loop's per-turn cancel tree — tripping it (via
                // TurnLifecycle::cancel or a parent subagent's cascade)
                // signals the running tool. The billed-LLM handle (if
                // any) is bound to this exact tool span so a tool that
                // makes a side-LLM call (e.g. `WebFetch`'s extraction or
                // Bash's risk judge) sees its `cost_records` row attributed
                // to the running tool span, not a synthetic placeholder.
                // `bind_source` is the agent loop's LITE client — see
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
                let mut ctx = ToolContext {
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
                    background_eligible: background.eligible,
                    overrun_to_background: background.overrun_to_background,
                    background_jobs: self.background_jobs.clone(),
                    background_control: self.background_control.clone(),
                    background_dispatch: Default::default(),
                    notify_silence,
                    checkout_root: checkout.as_ref().map(|c| c.root.clone()),
                    agent_handle,
                    checkout_git_config: None,
                };

                let mut params_revealed = params.clone();
                self.security_gateway
                    .reveal_in_value(&mut params_revealed)
                    .await
                    .map_err(|e| anyhow::anyhow!("reveal_in_value: {e}"))?;

                let resume_during_execute = matches!(
                    &pre_execute,
                    PreExecuteGate::PreDecided {
                        phase: AskPhase::DuringExecuteResumable,
                        ..
                    }
                );
                let mut prepared = if resume_during_execute {
                    None
                } else {
                    let prepared = self
                        .prepare_approval(
                            pinned_tool.as_deref(),
                            &params,
                            &params_revealed,
                            &ctx,
                            &pre_execute,
                        )
                        .await?;
                    self.flush_tool_events(&event_sink, recorder, ctx.span_id, &mut event_seq)
                        .await;
                    prepared
                };
                if let Some(prepared) = &mut prepared {
                    prepared.retain_uncovered(approved_resources);
                    prepared.validate_resume(&pre_execute, &tool_name_owned)?;
                }
                #[cfg(test)]
                if !resume_during_execute && let Some(hook) = &self.after_authorization_hook {
                    hook();
                }

                let needs_approval = prepared
                    .as_ref()
                    // An explicit ask without resources cannot be satisfied by resource grants.
                    .is_some_and(|p| {
                        p.authorized_accesses.is_empty() || !p.ask.accesses.is_empty()
                    });
                let outcome = match (&pre_execute, prepared.as_ref()) {
                    (PreExecuteGate::PreDecided { decision, .. }, _)
                        if resume_during_execute
                            || needs_approval
                            || *decision == ApprovalDecision::Deny =>
                    {
                        Some(ApprovalOutcome::answered(*decision))
                    }
                    (PreExecuteGate::Ask, Some(prepared)) if needs_approval => Some(
                        gate.request(
                            ApprovalRequest {
                                ask_id: baybo_model::human_ask::AskId::new(),
                                tool_call_id: tool_use_id.clone(),
                                session_id: session_id.clone(),
                                user_id: user.id.clone(),
                                tool: tool_name_owned.clone(),
                                accesses: prepared.ask.accesses.clone(),
                                params_preview: prepared.ask.params_preview.clone().unwrap_or_else(
                                    || preview_params(&params, APPROVAL_PARAMS_PREVIEW_LEN),
                                ),
                                description: prepared.ask.description.clone(),
                            },
                            AskSite {
                                channel: user.channel.clone(),
                                turn_id,
                                phase: AskPhase::BeforeExecute,
                            },
                            ask_cancel.clone(),
                        )
                        .await,
                    ),
                    _ => None,
                };
                if let Some(outcome) = outcome {
                    *approval_result = Some(outcome);
                    if let Some(prepared) = &prepared {
                        for resource in &prepared.ask.accesses {
                            let _ = recorder
                                .emit_event(
                                    ctx.span_id,
                                    event_seq,
                                    SpanEventKind::Approval {
                                        decision: outcome.decision,
                                        resource: resource.clone(),
                                    },
                                )
                                .await;
                            event_seq += 1;
                        }
                        if prepared.ask.cacheable
                            && outcome.decision == ApprovalDecision::ApproveAlways
                        {
                            let mut approved = approved_resources.lock();
                            for access in &prepared.ask.accesses {
                                let grant = access.to_approved();
                                if !approved.contains(&grant) {
                                    approved.push(grant);
                                }
                            }
                        }
                    }
                    require_approval(outcome, &tool_name_owned, &ctx)?;
                }
                if let Some(prepared) = prepared
                    && let Some(approval) = ctx.approval.as_mut()
                {
                    approval.record_pre_execute_approval(prepared.authorized_accesses);
                }

                // Re-resolved per call rather than cached, so an operator who
                // edits their git identity is obeyed by the next command. Run
                // after the pre-execute gate so denied calls spawn nothing.
                ctx.checkout_git_config = match (uses_exec_command, &checkout) {
                    (true, Some(checkout)) => worktree::ensure_identity_config(checkout).await,
                    _ => None,
                };

                // Execute with timeout enforcement. A `Read` of a virtual path
                // (e.g. the session transcript) is served inside `ReadTool` from
                // `ctx.virtual_reads` before it touches the filesystem.
                let deadline =
                    PausableDeadline::new(ctx.timeout + TOOL_DEADLINE_GRACE, park_gauge.clone());
                let execution = async {
                    let tool = pinned_tool.as_ref().ok_or_else(|| {
                        ToolError::NotFound(format!("tool not found: {tool_name_owned}"))
                    })?;
                    if resume_during_execute {
                        let resumer = tool.during_execute_resumer().ok_or_else(|| {
                            ToolError::Execution(format!(
                                "tool '{}' does not support resuming a during-execute approval",
                                tool.name()
                            ))
                        })?;
                        resumer.resume_during_execute(params_revealed, &ctx).await
                    } else {
                        tool.execute(params_revealed, &ctx).await
                    }
                };
                let result = deadline.run(execution).await;

                // A tool can raise its own prompts mid-execution (through
                // `ctx.approval`), and those come AFTER the pre-execute gate —
                // so the handle's last decision wins. Read it here, before any
                // of the arms below can return early: a mid-call denial reaches
                // the loop as the tool's own error, and without this the verdict
                // would be indistinguishable from a crash.
                if let Some(outcome) = approval_probe.last_outcome() {
                    *approval_result = Some(outcome);
                }
                *background_result = ctx.background_dispatch.take();

                // Flush tool-reported events as span events, regardless
                // of whether the tool succeeded, failed, or hit the
                // outer timeout. Each entry continues the span-local
                // seq sequence that approval emission left off at.
                self.flush_tool_events(&event_sink, recorder, span_handle.span_id, &mut event_seq)
                    .await;

                match result {
                    Some(Ok(mut output)) => {
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
                    Some(Err(e)) => {
                        // Surface the *sanitized* message: this Err
                        // bubbles into `with_span`, which writes
                        // `e.to_string()` into the span's
                        // `Failed { reason }` and from there into
                        // persisted trace storage. Returning the raw
                        // error would leak any secrets in the original
                        // text. The error's TYPE is lost here (a
                        // `ToolError::Denied` a tool raised from a mid-call
                        // prompt included) — the decision that shaped it rides
                        // `approval_outcome` instead, read back by the caller.
                        let raw = e.to_string();
                        let sanitized = self
                            .security_gateway
                            .sanitize_error(&raw)
                            .await
                            .unwrap_or(raw);
                        Err(anyhow::anyhow!(sanitized))
                    }
                    None => Err(anyhow::anyhow!(
                        "tool '{}' exceeded timeout ({:?})",
                        tool_name_owned,
                        ctx.timeout
                    )),
                }
            },
        )
        .await;

        let outcome = approval_outcome;
        ExecutedTool {
            output,
            approval: outcome.map(|o| o.decision),
            abandoned: outcome.is_some_and(|o| o.resolution == ApprovalResolution::Abandoned),
            handed_over,
        }
    }
}

/// Bounded, content-keyed cache of compiled `ToolInvoke` param validators.
///
/// Keyed by the SERIALIZED SCHEMA, never the tool name: a reconnect can
/// re-register the same name with a different schema, and a name-keyed
/// entry would then validate against the wrong generation. Content keying
/// makes that case a miss (new compile) and an unchanged schema across
/// reconnects a hit. Compile happens outside the lock — two concurrent
/// first calls both compile and one insert wins, which is cheaper than
/// holding the lock across a compile. Uncompilable schemas are not
/// negatively cached: they fail fast (the 64 KB cap bounds pathological
/// compiles before they start) and stay the server's problem.
#[derive(Default)]
struct InvokeValidatorCache {
    inner: Mutex<std::collections::HashMap<String, Arc<jsonschema::Validator>>>,
}

impl InvokeValidatorCache {
    /// Enough for every distinct schema a deployment realistically holds
    /// (each MCP tool contributes one). Overflow clears wholesale — crude,
    /// but recompiling a handful of schemas beats LRU bookkeeping on a
    /// path that is pure advisory validation.
    const MAX_ENTRIES: usize = 256;

    /// Schema pre-validation for `ToolInvoke`-routed calls. Advisory
    /// quality gate, not a security door — skipped whenever it cannot give
    /// a correct answer, letting the tool (or its MCP server) decide
    /// exactly as a direct call would:
    /// * an uncompilable schema skips (the server's problem, not the call's);
    /// * an oversized schema skips (a pathological schema must not stall
    ///   the loop before the execution timeout).
    ///
    /// The caller hands in the REVEALED copy of the params (see
    /// [`ToolExecutor::validate_invoke_params`]) so that a `pattern`/`enum`/
    /// length constraint on a secret-bearing field judges the real value,
    /// not the `[{REDACTED_SECRET_…}]` placeholder the model holds — and is
    /// responsible for re-masking every revealed value out of the message
    /// it reports.
    fn validate(
        &self,
        tool: &dyn baybo_tools::Tool,
        params: &Value,
    ) -> Result<(), InvokeParamsRejection> {
        const MAX_REPORTED_ERRORS: usize = 3;
        const MAX_SCHEMA_COMPILE_BYTES: usize = 65536;
        /// Trace failure reasons stay bounded (every other failure path
        /// caps its reason), so the quoted schema is cut here and the model
        /// told where the full copy lives.
        const MAX_QUOTED_SCHEMA_BYTES: usize = 4096;

        let schema = tool.parameters_schema();
        let serialized_schema = serde_json::to_string(&schema).unwrap_or_default();
        if serialized_schema.len() > MAX_SCHEMA_COMPILE_BYTES {
            debug!(
                tool = tool.name(),
                bytes = serialized_schema.len(),
                "invoke params schema too large to compile; skipping validation"
            );
            return Ok(());
        }
        let cached = self.inner.lock().get(&serialized_schema).cloned();
        let validator = match cached {
            Some(v) => v,
            None => {
                let compiled = match jsonschema::validator_for(&schema) {
                    Ok(v) => Arc::new(v),
                    Err(e) => {
                        debug!(tool = tool.name(), error = %e, "invoke params schema did not compile; skipping validation");
                        return Ok(());
                    }
                };
                let mut cache = self.inner.lock();
                if cache.len() >= Self::MAX_ENTRIES {
                    cache.clear();
                }
                cache
                    .entry(serialized_schema.clone())
                    .or_insert_with(|| Arc::clone(&compiled))
                    .clone()
            }
        };
        let errors: Vec<(String, String)> = validator
            .iter_errors(params)
            .take(MAX_REPORTED_ERRORS)
            .map(|e| {
                let at = e.instance_path().to_string();
                let rendered = if at.is_empty() {
                    e.to_string()
                } else {
                    format!("{at}: {e}")
                };
                (at, rendered)
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            let quoted_schema = if serialized_schema.len() > MAX_QUOTED_SCHEMA_BYTES {
                let cut = serialized_schema
                    .char_indices()
                    .take_while(|(i, _)| *i < MAX_QUOTED_SCHEMA_BYTES)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                format!(
                    "{}… (truncated; fetch the full schema via ToolSearch kind=\"exact\" query=\"{}\")",
                    &serialized_schema[..cut],
                    tool.name()
                )
            } else {
                serialized_schema
            };
            Err(InvokeParamsRejection {
                errors,
                quoted_schema,
            })
        }
    }
}

/// One failed `ToolInvoke` schema validation: per-error `(instance_path,
/// rendered message)` pairs plus the (already truncated) schema to quote.
/// Rendered into the model-visible error by
/// [`ToolExecutor::validate_invoke_params`], which owns secret re-masking.
struct InvokeParamsRejection {
    errors: Vec<(String, String)>,
    quoted_schema: String,
}

/// Collect every string leaf `reveal_in_value` changed, as
/// `(json_pointer, original_masked, revealed)` triples. The original still
/// carries the placeholder, so an exact `revealed → original` replacement
/// re-masks any quotation of the value deterministically — no reliance on
/// pattern-based leak rules.
fn revealed_secret_values(
    original: &Value,
    revealed: &Value,
    path: &mut String,
    out: &mut Vec<(String, String, String)>,
) {
    match (original, revealed) {
        (Value::String(o), Value::String(r)) if o != r => {
            out.push((path.clone(), o.clone(), r.clone()));
        }
        (Value::Object(o), Value::Object(r)) => {
            for (key, ov) in o {
                if let Some(rv) = r.get(key) {
                    let len = path.len();
                    path.push('/');
                    // JSON-pointer escaping, to match `instance_path()`.
                    path.push_str(&key.replace('~', "~0").replace('/', "~1"));
                    revealed_secret_values(ov, rv, path, out);
                    path.truncate(len);
                }
            }
        }
        (Value::Array(o), Value::Array(r)) => {
            for (i, (ov, rv)) in o.iter().zip(r).enumerate() {
                let len = path.len();
                path.push('/');
                path.push_str(&i.to_string());
                revealed_secret_values(ov, rv, path, out);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

/// Render a rejection into the model-visible message. Errors anchored at a
/// secret-bearing path get a generic line instead of the validator's text —
/// the model cannot see or fix a stored secret, so quoting the constraint
/// failure would only start an unwinnable retry loop; everything else keeps
/// the validator's message. Every revealed value is then exact-replaced
/// back to its placeholder (longest first, so one revealed value embedded
/// in another cannot leave a fragment behind).
fn compose_invoke_rejection(
    tool_name: &str,
    rejection: &InvokeParamsRejection,
    secrets: &[(String, String, String)],
) -> String {
    let parts: Vec<String> = rejection
        .errors
        .iter()
        .map(|(path, rendered)| {
            let secret_here = secrets
                .iter()
                .any(|(sp, _, _)| path == sp || path.starts_with(&format!("{sp}/")));
            if secret_here {
                format!(
                    "{path}: the value is a stored secret and fails this field's schema \
                     constraint; fix the stored secret — the model cannot see or edit it"
                )
            } else {
                rendered.clone()
            }
        })
        .collect();
    let mut msg = format!(
        "ToolInvoke params for '{}' failed schema validation: {}. Expected parameters_schema: {}",
        tool_name,
        parts.join("; "),
        rejection.quoted_schema
    );
    let mut pairs: Vec<&(String, String, String)> = secrets.iter().collect();
    pairs.sort_by_key(|(_, _, revealed)| std::cmp::Reverse(revealed.len()));
    for (_, original, revealed) in pairs {
        if !revealed.is_empty() {
            msg = msg.replace(revealed, original);
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutedTool, InvokeValidatorCache, PausableDeadline, TOOL_DEADLINE_GRACE, ToolExecutor,
        admits_repo, admits_worktree, permissive_scope,
    };
    use baybo_human_ask::test_support::MemoryHumanAskStore;
    use baybo_human_ask::{AskCore, PolicyDenyGate};
    use baybo_model::human_ask::{AskKind, AskPhase, AskResolution};
    use baybo_tools::{ParkGauge, TurnTokens};
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    /// Tokens nobody trips: the call runs to completion.
    fn idle_tokens() -> TurnTokens {
        TurnTokens {
            turn: CancellationToken::new(),
            release: CancellationToken::new(),
        }
    }

    /// The production policy-deny gate over an in-memory ledger.
    fn policy_deny_gate() -> (Arc<MemoryHumanAskStore>, Arc<dyn baybo_tools::ApprovalGate>) {
        let store = MemoryHumanAskStore::new();
        let core = Arc::new(AskCore::new(store.clone()));
        (store, Arc::new(PolicyDenyGate::new(core)))
    }

    #[test]
    fn invoke_params_validation_reports_schema_and_skips_uncompilable() {
        let cache = InvokeValidatorCache::default();
        use baybo_tools::{Tool, ToolContext, ToolOutput};

        struct Fake(serde_json::Value);

        #[async_trait::async_trait]
        impl Tool for Fake {
            fn name(&self) -> &str {
                "srv/op"
            }
            fn description(&self) -> String {
                "x".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                self.0.clone()
            }
            async fn execute(
                &self,
                _p: serde_json::Value,
                _c: &ToolContext,
            ) -> baybo_tools::Result<ToolOutput> {
                Ok(ToolOutput::Text(String::new()))
            }
        }

        let strict = Fake(serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "additionalProperties": false
        }));
        assert!(
            cache
                .validate(&strict, &serde_json::json!({ "id": "a" }))
                .is_ok()
        );
        let rejection = cache
            .validate(&strict, &serde_json::json!({ "id": 7 }))
            .unwrap_err();
        assert_eq!(rejection.errors.len(), 1);
        assert_eq!(rejection.errors[0].0, "/id", "error carries its pointer");
        let msg = super::compose_invoke_rejection("srv/op", &rejection, &[]);
        assert!(msg.contains("srv/op"), "names the tool: {msg}");
        assert!(
            msg.contains("parameters_schema"),
            "hands the schema back for self-correction: {msg}"
        );
        let rejection = cache.validate(&strict, &serde_json::json!({})).unwrap_err();
        let msg = super::compose_invoke_rejection("srv/op", &rejection, &[]);
        assert!(
            msg.contains("required") || msg.contains("id"),
            "missing field reported: {msg}"
        );

        // An uncompilable schema is the server's problem, not a reason to
        // refuse the call before it can even reach the server.
        let broken = Fake(serde_json::json!({ "type": "not-a-real-type" }));
        assert!(cache.validate(&broken, &serde_json::json!({})).is_ok());

        // The quoted schema in the error is bounded for the trace store.
        let big_desc = "x".repeat(8192);
        let bloated = Fake(serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": big_desc } },
            "required": ["id"],
        }));
        let rejection = cache
            .validate(&bloated, &serde_json::json!({}))
            .unwrap_err();
        assert!(
            rejection.quoted_schema.len() < 5000,
            "quoted schema stays bounded, got {} bytes",
            rejection.quoted_schema.len()
        );
        assert!(
            rejection.quoted_schema.contains("truncated"),
            "names the cut"
        );

        // Compiles are cached by schema CONTENT: `strict` ran three times
        // and `bloated` once → two entries, not four. A schema change (a
        // reconnect re-registering the name with a new surface) is a new
        // key, and the uncompilable/oversize skips never populated
        // anything.
        assert_eq!(cache.inner.lock().len(), 2);
        let mut evolved = strict.0.clone();
        evolved["properties"]["id"]["maxLength"] = serde_json::json!(4);
        assert!(
            cache
                .validate(&Fake(evolved), &serde_json::json!({ "id": "a" }))
                .is_ok()
        );
        assert_eq!(
            cache.inner.lock().len(),
            3,
            "a changed schema under the same tool name compiles fresh"
        );
    }

    #[test]
    fn invoke_rejection_masks_revealed_secrets_and_goes_generic_on_secret_paths() {
        use super::{InvokeParamsRejection, compose_invoke_rejection, revealed_secret_values};

        let placeholder = "[{REDACTED_SECRET_0123456789abcdef01234567}]";
        let original = serde_json::json!({
            "token": placeholder,
            "region": "ap-guangzhou",
            "nested": { "a/b~c": [placeholder, "plain"] }
        });
        let revealed = serde_json::json!({
            "token": "sk-9f8e7d6c5b4a39281706f5e4d3c2b1a0",
            "region": "ap-guangzhou",
            "nested": { "a/b~c": ["sk-9f8e7d6c5b4a39281706f5e4d3c2b1a0", "plain"] }
        });
        let mut secrets = Vec::new();
        let mut path = String::new();
        revealed_secret_values(&original, &revealed, &mut path, &mut secrets);
        assert_eq!(
            secrets
                .iter()
                .map(|(p, _, _)| p.as_str())
                .collect::<Vec<_>>(),
            vec!["/nested/a~1b~0c/0", "/token"],
            "diff walks nested values with pointer escaping"
        );

        // A validator-style rejection: one error at the secret path quoting
        // the revealed value, one ordinary error, and the revealed value
        // embedded in the quoted schema too.
        let rejection = InvokeParamsRejection {
            errors: vec![
                (
                    "/token".to_string(),
                    "/token: \"sk-9f8e7d6c5b4a39281706f5e4d3c2b1a0\" does not match \"^tk-\""
                        .to_string(),
                ),
                (
                    "/region".to_string(),
                    "/region: not one of the enum".to_string(),
                ),
            ],
            quoted_schema:
                "{\"example\":\"sk-9f8e7d6c5b4a39281706f5e4d3c2b1a0\",\"type\":\"object\"}"
                    .to_string(),
        };
        let msg = compose_invoke_rejection("srv/op", &rejection, &secrets);
        assert!(
            !msg.contains("sk-9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
            "no revealed value may survive into the message: {msg}"
        );
        assert!(
            msg.contains(placeholder),
            "revealed values are re-masked to their placeholders: {msg}"
        );
        assert!(
            msg.contains("stored secret"),
            "secret-path errors go generic: {msg}"
        );
        assert!(
            msg.contains("/region: not one of the enum"),
            "non-secret errors keep the validator text: {msg}"
        );
    }

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
            policy_deny_gate().1,
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
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::Ask,
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
        async fn request(
            &self,
            _req: ApprovalRequest,
            _site: AskSite,
            _cancel: AskCancel,
        ) -> ApprovalOutcome {
            self.0
        }
    }

    struct CountingApproveGate {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl baybo_tools::ApprovalGate for CountingApproveGate {
        async fn request(
            &self,
            _req: ApprovalRequest,
            _site: AskSite,
            _cancel: AskCancel,
        ) -> ApprovalOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ApprovalOutcome::answered(ApprovalDecision::Approve)
        }
    }

    const SELECTED_MCP_TOOL: &str = "lighthouse/run_audit";

    /// The shape an MCP wrapper presents to the executor: a dynamic tool
    /// whose accesses are declared up front (usually none — see
    /// `resource_access_for` in `baybo-tools`), optionally asking for more
    /// mid-call.
    struct McpShapedTool {
        namespaced_name: &'static str,
        declares_access: bool,
        mid_execution_access: bool,
        ran: Arc<AtomicBool>,
    }

    impl McpShapedTool {
        fn declared_access() -> ResourceAccess {
            ResourceAccess::Http {
                host: "mcp.example".to_string(),
            }
        }
    }

    #[async_trait::async_trait]
    impl baybo_tools::Tool for McpShapedTool {
        fn name(&self) -> &str {
            self.namespaced_name
        }

        fn description(&self) -> String {
            "unattended authorization test".to_string()
        }

        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }

        fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
            if self.declares_access {
                vec![Self::declared_access()]
            } else {
                Vec::new()
            }
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
                    .request_uncached(
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

    fn ordinary_trigger() -> baybo_model::TriggerSource {
        baybo_model::TriggerSource::User
    }

    async fn execute_authorization_tool(
        tool: Arc<dyn baybo_tools::Tool>,
        gate: Arc<dyn baybo_tools::ApprovalGate>,
        approved_resources: Arc<Mutex<Vec<ApprovedResource>>>,
        attended: bool,
    ) -> (ExecutedTool, Arc<MemoryHumanAskStore>) {
        let tool_name = tool.name().to_string();
        let mut registry = ToolRegistry::new();
        registry.register(tool, authorization_manifest(&tool_name));
        execute_authorization_registration(
            Arc::new(registry),
            tool_name,
            gate,
            approved_resources,
            attended,
            None,
            PreExecuteGate::Ask,
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
        attended: bool,
        after_authorization_hook: Option<Arc<dyn Fn() + Send + Sync>>,
        pre_execute: PreExecuteGate,
    ) -> (ExecutedTool, Arc<MemoryHumanAskStore>) {
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
        let (ledger, policy_deny) = policy_deny_gate();
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
            policy_deny,
        );
        if let Some(hook) = after_authorization_hook {
            executor = executor.with_after_authorization_hook(hook);
        }

        let session_id = SessionId::from("sess-authorization");
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

        let executed = executor
            .execute(
                &tool_name,
                json!({}),
                &session_id,
                &ordinary_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &approved_resources,
                &recorder,
                &step,
                None,
                AUTHORIZATION_TOOL_USE_ID.to_string(),
                None,
                None,
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                attended,
                ReadTracker::default(),
                None,
                false,
                pre_execute,
            )
            .await;
        (executed, ledger)
    }

    const AUTHORIZATION_TOOL_USE_ID: &str = "tool-use-authorization";

    fn counting_gate() -> (Arc<AtomicUsize>, Arc<dyn baybo_tools::ApprovalGate>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let gate: Arc<dyn baybo_tools::ApprovalGate> = Arc::new(CountingApproveGate {
            calls: Arc::clone(&calls),
        });
        (calls, gate)
    }

    #[tokio::test]
    async fn an_unattended_turn_runs_a_zero_access_mcp_tool_without_any_gate() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let (executed, ledger) = execute_authorization_tool(
            Arc::new(McpShapedTool {
                namespaced_name: SELECTED_MCP_TOOL,
                declares_access: false,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            false,
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, None);
        assert!(ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
        assert!(ledger.rows().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dynamic_replacement_after_authorization_executes_the_pinned_tool() {
        let original_ran = Arc::new(AtomicBool::new(false));
        let replacement_ran = Arc::new(AtomicBool::new(false));
        let original: Arc<dyn baybo_tools::Tool> = Arc::new(McpShapedTool {
            namespaced_name: SELECTED_MCP_TOOL,
            declares_access: true,
            mid_execution_access: false,
            ran: Arc::clone(&original_ran),
        });
        let replacement: Arc<dyn baybo_tools::Tool> = Arc::new(McpShapedTool {
            namespaced_name: SELECTED_MCP_TOOL,
            declares_access: true,
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
            true,
            Some(hook),
            PreExecuteGate::Ask,
        )
        .await
        .0;
        replacement_task.await.expect("replacement task");

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, Some(ApprovalDecision::Approve));
        assert!(
            original_ran.load(Ordering::SeqCst),
            "the generation whose accesses were authorized must execute"
        );
        assert!(
            !replacement_ran.load(Ordering::SeqCst),
            "a same-name replacement must not execute under the prior generation's approval"
        );
        assert_eq!(gate_calls.load(Ordering::SeqCst), 1);
        assert!(approved.lock().is_empty());
    }

    /// The one ledger row an unattended call leaves: already resolved
    /// `PolicyDenied` and applied, in the phase that raised it.
    fn assert_one_policy_denial(ledger: &MemoryHumanAskStore, phase: AskPhase) {
        let rows = ledger.rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.kind, AskKind::Approval);
        assert_eq!(row.phase, phase);
        assert_eq!(row.tool_call_id, AUTHORIZATION_TOOL_USE_ID);
        assert_eq!(row.resolution, Some(AskResolution::PolicyDenied));
        assert!(row.resolved_at.is_some() && row.applied_at.is_some());
    }

    #[tokio::test]
    async fn an_unattended_turn_policy_denies_uncovered_access_without_prompting() {
        let (gate_calls, gate) = counting_gate();
        let (executed, ledger) = execute_authorization_tool(
            Arc::new(GatedTool),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            false,
        )
        .await;

        let error = executed.output.expect_err("uncovered access must fail");
        assert!(
            error
                .to_string()
                .contains(refusal_reason(ApprovalResolution::Policy)),
            "{error}"
        );
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        assert!(!executed.abandoned);
        assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
        assert_one_policy_denial(&ledger, AskPhase::BeforeExecute);
    }

    #[tokio::test]
    async fn an_unattended_turn_policy_denies_access_discovered_during_execution() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let approved = Arc::new(Mutex::new(Vec::new()));
        let (executed, ledger) = execute_authorization_tool(
            Arc::new(McpShapedTool {
                namespaced_name: SELECTED_MCP_TOOL,
                declares_access: false,
                mid_execution_access: true,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::clone(&approved),
            false,
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
        assert_one_policy_denial(&ledger, AskPhase::DuringExecuteNonResumable);
    }

    #[tokio::test]
    async fn an_attended_turn_asks_the_channel_gate() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let (executed, ledger) = execute_authorization_tool(
            Arc::new(McpShapedTool {
                namespaced_name: SELECTED_MCP_TOOL,
                declares_access: true,
                mid_execution_access: false,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            true,
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, Some(ApprovalDecision::Approve));
        assert!(ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 1);
        assert!(ledger.rows().is_empty(), "policy gate untouched");
    }

    async fn pre_decided(
        decision: ApprovalDecision,
    ) -> (ExecutedTool, usize, bool, Vec<ApprovedResource>) {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let tool: Arc<dyn baybo_tools::Tool> = Arc::new(McpShapedTool {
            namespaced_name: SELECTED_MCP_TOOL,
            declares_access: true,
            mid_execution_access: false,
            ran: Arc::clone(&ran),
        });
        let mut registry = ToolRegistry::new();
        registry.register(tool, authorization_manifest(SELECTED_MCP_TOOL));
        let approved = Arc::new(Mutex::new(Vec::new()));
        let (executed, _ledger) = execute_authorization_registration(
            Arc::new(registry),
            SELECTED_MCP_TOOL.to_string(),
            gate,
            Arc::clone(&approved),
            true,
            None,
            PreExecuteGate::PreDecided {
                decision,
                phase: AskPhase::BeforeExecute,
                accesses: vec![McpShapedTool::declared_access()],
            },
        )
        .await;
        let grants = approved.lock().clone();
        (
            executed,
            gate_calls.load(Ordering::SeqCst),
            ran.load(Ordering::SeqCst),
            grants,
        )
    }

    #[tokio::test]
    async fn a_pre_decided_approval_runs_without_raising_an_ask() {
        let (executed, gate_calls, ran, grants) = pre_decided(ApprovalDecision::Approve).await;
        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, Some(ApprovalDecision::Approve));
        assert!(ran);
        assert_eq!(gate_calls, 0, "the gate is never asked");
        assert!(grants.is_empty(), "Approve grants nothing beyond the call");
    }

    #[tokio::test]
    async fn a_pre_decided_approve_always_seeds_the_grants() {
        let (executed, gate_calls, ran, grants) =
            pre_decided(ApprovalDecision::ApproveAlways).await;
        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert!(ran);
        assert_eq!(gate_calls, 0);
        assert_eq!(grants, vec![McpShapedTool::declared_access().to_approved()]);
    }

    #[tokio::test]
    async fn a_pre_decided_denial_does_not_run_the_tool() {
        let (executed, gate_calls, ran, _) = pre_decided(ApprovalDecision::Deny).await;
        assert!(executed.output.is_err());
        assert_eq!(executed.approval, Some(ApprovalDecision::Deny));
        assert!(!ran);
        assert_eq!(gate_calls, 0);
    }

    #[tokio::test]
    async fn an_attended_turn_asks_the_channel_gate_mid_execution() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate_calls, gate) = counting_gate();
        let (executed, ledger) = execute_authorization_tool(
            Arc::new(McpShapedTool {
                namespaced_name: SELECTED_MCP_TOOL,
                declares_access: false,
                mid_execution_access: true,
                ran: Arc::clone(&ran),
            }),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            true,
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert_eq!(executed.approval, Some(ApprovalDecision::Approve));
        assert!(ran.load(Ordering::SeqCst));
        assert_eq!(gate_calls.load(Ordering::SeqCst), 1);
        assert!(ledger.rows().is_empty());
    }

    async fn gated_call_error(outcome: ApprovalOutcome) -> (String, bool) {
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
            policy_deny_gate().1,
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
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::Ask,
            )
            .await;
        let abandoned = executed.abandoned;
        let msg = executed
            .output
            .expect_err("a denied gate must not let the tool run")
            .to_string();
        (msg, abandoned)
    }

    #[tokio::test]
    async fn an_abandoned_gate_is_not_reported_to_the_model_as_a_human_no() {
        let (msg, abandoned) = gated_call_error(ApprovalOutcome::abandoned()).await;
        assert!(abandoned, "the loop must claim the ask before appending");
        assert!(msg.contains("withdrawn"), "got {msg}");
        assert!(!msg.contains("said no"), "got {msg}");
        assert!(!msg.contains("user denied approval"), "got {msg}");
    }

    #[tokio::test]
    async fn a_human_no_still_reads_as_a_human_no() {
        let (msg, abandoned) =
            gated_call_error(ApprovalOutcome::answered(ApprovalDecision::Deny)).await;
        assert!(!abandoned);
        assert!(
            msg.contains("a human saw this prompt and said no"),
            "got {msg}"
        );
    }

    const DEADLINE_BUDGET: Duration = Duration::from_secs(10);

    #[tokio::test(start_paused = true)]
    async fn a_parked_call_outlives_its_deadline() {
        let gauge = ParkGauge::new();
        let parked = gauge.clone();
        let out = PausableDeadline::new(DEADLINE_BUDGET, gauge)
            .run(async move {
                let _guard = parked.park();
                tokio::time::sleep(DEADLINE_BUDGET * 100).await;
                "answered"
            })
            .await;
        assert_eq!(out, Some("answered"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unparked_call_still_expires() {
        let out = PausableDeadline::new(DEADLINE_BUDGET, ParkGauge::new())
            .run(tokio::time::sleep(DEADLINE_BUDGET * 2))
            .await;
        assert_eq!(out, None);
    }

    #[tokio::test(start_paused = true)]
    async fn only_unparked_time_is_charged_to_the_budget() {
        async fn work_park_work(gauge: ParkGauge, work: Duration) {
            tokio::time::sleep(work).await;
            {
                let _guard = gauge.park();
                tokio::time::sleep(DEADLINE_BUDGET * 10).await;
            }
            tokio::time::sleep(work).await;
        }
        let gauge = ParkGauge::new();
        let within = PausableDeadline::new(DEADLINE_BUDGET, gauge.clone())
            .run(work_park_work(gauge, DEADLINE_BUDGET * 4 / 10))
            .await;
        assert_eq!(within, Some(()), "0.8 of the budget unparked fits");

        let gauge = ParkGauge::new();
        let over = PausableDeadline::new(DEADLINE_BUDGET, gauge.clone())
            .run(work_park_work(gauge, DEADLINE_BUDGET * 6 / 10))
            .await;
        assert_eq!(over, None, "1.2 of the budget unparked does not");
    }

    /// Sleeps through its own `max_timeout`, then does a short step — the
    /// shape of Bash detaching a timed-out command.
    struct OverrunningTool;

    const OVERRUNNING_TOOL: &str = "overrunning";
    const OVERRUNNING_TIMEOUT: Duration = Duration::from_millis(50);

    #[async_trait::async_trait]
    impl baybo_tools::Tool for OverrunningTool {
        fn name(&self) -> &str {
            OVERRUNNING_TOOL
        }
        fn description(&self) -> String {
            "overruns its own deadline briefly".to_owned()
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        fn max_timeout(&self) -> Duration {
            OVERRUNNING_TIMEOUT
        }
        async fn execute(
            &self,
            _params: Value,
            ctx: &ToolContext,
        ) -> baybo_tools::Result<ToolOutput> {
            tokio::time::sleep(ctx.timeout).await;
            tokio::time::sleep(OVERRUNNING_TIMEOUT * 4).await;
            Ok(ToolOutput::Text("handed off".to_owned()))
        }
    }

    #[tokio::test]
    async fn a_tool_that_overruns_its_timeout_briefly_completes_within_the_grace() {
        assert!(OVERRUNNING_TIMEOUT * 5 < TOOL_DEADLINE_GRACE);
        let (_, gate) = counting_gate();
        let (executed, _) = execute_authorization_tool(
            Arc::new(OverrunningTool),
            gate,
            Arc::new(Mutex::new(Vec::new())),
            true,
        )
        .await;
        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
    }

    type RecordedAsks = Arc<Mutex<Vec<(ApprovalRequest, AskSite)>>>;

    struct RecordingGate {
        outcome: ApprovalOutcome,
        recorded: RecordedAsks,
    }

    impl RecordingGate {
        fn new(outcome: ApprovalOutcome) -> (Arc<Self>, RecordedAsks) {
            let recorded = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(Self {
                    outcome,
                    recorded: Arc::clone(&recorded),
                }),
                recorded,
            )
        }
    }

    #[async_trait::async_trait]
    impl ApprovalGate for RecordingGate {
        async fn request(
            &self,
            req: ApprovalRequest,
            site: AskSite,
            _cancel: AskCancel,
        ) -> ApprovalOutcome {
            self.recorded.lock().push((req, site));
            self.outcome
        }
    }

    struct UncachedPreAskTool {
        ran: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl baybo_tools::Tool for UncachedPreAskTool {
        fn name(&self) -> &str {
            "uncached_pre_ask"
        }
        fn description(&self) -> String {
            "raises an uncached before-execute ask".to_owned()
        }
        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "properties": {}})
        }
        async fn pre_execute_ask(
            &self,
            _params: &Value,
            _ctx: &ToolContext,
            _check: PreExecuteCheck,
        ) -> Option<baybo_tools::PreExecuteAsk> {
            Some(baybo_tools::PreExecuteAsk {
                accesses: vec![ResourceAccess::ExecCommand {
                    command: "echo elevated".to_owned(),
                }],
                cacheable: false,
                description: Some("Custom escalation description".to_owned()),
                params_preview: Some("Custom escalation preview".to_owned()),
            })
        }
        async fn execute(
            &self,
            _params: Value,
            _ctx: &ToolContext,
        ) -> baybo_tools::Result<ToolOutput> {
            self.ran.store(true, Ordering::SeqCst);
            Ok(ToolOutput::Text("ok".to_owned()))
        }
    }

    #[tokio::test]
    async fn resource_free_pre_execute_asks_require_a_decision() {
        struct ConfirmationTool {
            cacheable: bool,
            ran: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl baybo_tools::Tool for ConfirmationTool {
            fn name(&self) -> &str {
                "confirmation"
            }
            fn description(&self) -> String {
                "requires explicit confirmation".to_owned()
            }
            fn parameters_schema(&self) -> Value {
                json!({"type": "object"})
            }
            async fn pre_execute_ask(
                &self,
                _params: &Value,
                _ctx: &ToolContext,
                _check: PreExecuteCheck,
            ) -> Option<PreExecuteAsk> {
                Some(PreExecuteAsk {
                    accesses: Vec::new(),
                    cacheable: self.cacheable,
                    description: Some("Confirm this operation".to_owned()),
                    params_preview: None,
                })
            }
            async fn execute(
                &self,
                _params: Value,
                _ctx: &ToolContext,
            ) -> baybo_tools::Result<ToolOutput> {
                self.ran.store(true, Ordering::SeqCst);
                Ok(ToolOutput::Text("ok".to_owned()))
            }
        }

        for cacheable in [false, true] {
            for decision in [ApprovalDecision::Deny, ApprovalDecision::ApproveAlways] {
                for resumed in [false, true] {
                    let ran = Arc::new(AtomicBool::new(false));
                    let (gate, recorded) = RecordingGate::new(ApprovalOutcome::answered(decision));
                    let mut registry = ToolRegistry::new();
                    registry.register(
                        Arc::new(ConfirmationTool {
                            cacheable,
                            ran: Arc::clone(&ran),
                        }),
                        authorization_manifest("confirmation"),
                    );
                    let grants = vec![ApprovedResource::ExecCommand {
                        command: "echo elevated".to_owned(),
                    }];
                    let approved = Arc::new(Mutex::new(grants.clone()));
                    let mode = if resumed {
                        PreExecuteGate::PreDecided {
                            decision,
                            phase: AskPhase::BeforeExecute,
                            accesses: Vec::new(),
                        }
                    } else {
                        PreExecuteGate::Ask
                    };
                    let (executed, _) = execute_authorization_registration(
                        Arc::new(registry),
                        "confirmation".to_owned(),
                        gate,
                        Arc::clone(&approved),
                        true,
                        None,
                        mode,
                    )
                    .await;
                    assert_eq!(recorded.lock().len(), usize::from(!resumed));
                    assert_eq!(executed.approval, Some(decision));
                    assert_eq!(executed.output.is_ok(), decision != ApprovalDecision::Deny);
                    assert_eq!(
                        ran.load(Ordering::SeqCst),
                        decision != ApprovalDecision::Deny
                    );
                    assert_eq!(*approved.lock(), grants);
                }
            }
        }
    }

    #[tokio::test]
    async fn uncached_pre_execute_ask_ignores_cache_never_persists_and_uses_before_execute_phase() {
        let ran = Arc::new(AtomicBool::new(false));
        let (gate, recorded) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::ApproveAlways));
        let mut registry = ToolRegistry::new();
        registry.register(
            Arc::new(UncachedPreAskTool {
                ran: Arc::clone(&ran),
            }),
            authorization_manifest("uncached_pre_ask"),
        );
        // Pre-seed the exact same ExecCommand grant in session cache.
        let cached_entry = ApprovedResource::ExecCommand {
            command: "echo elevated".to_owned(),
        };
        let approved = Arc::new(Mutex::new(vec![cached_entry.clone()]));
        let (executed, _) = execute_authorization_registration(
            Arc::new(registry),
            "uncached_pre_ask".to_owned(),
            gate,
            Arc::clone(&approved),
            true,
            None,
            PreExecuteGate::Ask,
        )
        .await;

        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert!(ran.load(Ordering::SeqCst));
        {
            let asks = recorded.lock();
            assert_eq!(asks.len(), 1, "cached grant must not bypass uncached ask");
            assert_eq!(asks[0].1.phase, AskPhase::BeforeExecute);
            assert_eq!(
                asks[0].0.description.as_deref(),
                Some("Custom escalation description")
            );
            assert_eq!(asks[0].0.params_preview, "Custom escalation preview");
        }

        let ran2 = Arc::new(AtomicBool::new(false));
        let (gate2, recorded2) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::ApproveAlways));
        let mut registry2 = ToolRegistry::new();
        registry2.register(
            Arc::new(UncachedPreAskTool {
                ran: Arc::clone(&ran2),
            }),
            authorization_manifest("uncached_pre_ask"),
        );
        let empty_cache = Arc::new(Mutex::new(Vec::new()));
        let (executed2, _) = execute_authorization_registration(
            Arc::new(registry2),
            "uncached_pre_ask".to_owned(),
            gate2,
            Arc::clone(&empty_cache),
            true,
            None,
            PreExecuteGate::Ask,
        )
        .await;
        assert!(executed2.output.is_ok());
        assert_eq!(recorded2.lock().len(), 1);
        assert!(
            empty_cache.lock().is_empty(),
            "ApproveAlways on an uncached ask must never seed session grants"
        );

        // PreDecided(ApproveAlways) on an uncached ask must also not seed grants.
        let ran3 = Arc::new(AtomicBool::new(false));
        let (gate3, recorded3) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::Deny));
        let mut registry3 = ToolRegistry::new();
        registry3.register(
            Arc::new(UncachedPreAskTool {
                ran: Arc::clone(&ran3),
            }),
            authorization_manifest("uncached_pre_ask"),
        );
        let predecided_cache = Arc::new(Mutex::new(Vec::new()));
        let (executed3, _) = execute_authorization_registration(
            Arc::new(registry3),
            "uncached_pre_ask".to_owned(),
            gate3,
            Arc::clone(&predecided_cache),
            true,
            None,
            PreExecuteGate::PreDecided {
                decision: ApprovalDecision::ApproveAlways,
                phase: AskPhase::BeforeExecute,
                accesses: vec![ResourceAccess::ExecCommand {
                    command: "echo elevated".to_owned(),
                }],
            },
        )
        .await;
        assert!(executed3.output.is_ok());
        assert!(ran3.load(Ordering::SeqCst));
        assert!(recorded3.lock().is_empty(), "PreDecided never re-prompts");
        assert!(
            predecided_cache.lock().is_empty(),
            "PreDecided(ApproveAlways) on an uncached ask must not seed session grants"
        );
    }

    #[tokio::test]
    async fn bash_auto_risk_judge_runs_before_execute_flushes_events_and_skips_llm_on_pre_decided()
    {
        use baybo_llm::test_support::StubLlm;
        use baybo_llm::{LlmCompletion, LlmResponse, TokenUsage};
        use baybo_tools::builtin::bash::BashTool;
        use baybo_trace::{Span, SpanEvent, TraceStore};

        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        let bash = Arc::new(BashTool::new(
            paths.clone(),
            baybo_process::ProcessManager::transient(),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            bash,
            ToolManifest {
                name: "Bash".to_owned(),
                description: "bash".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object", "properties": {}}),
                capabilities: vec![ToolCapability::ExecCommand],
                channels: Vec::new(),
            },
        );

        let (gate, recorded) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::Deny));
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
        let executor = ToolExecutor::new(
            Arc::new(registry),
            Arc::new(gates),
            security_gateway,
            sandbox_root.clone(),
            paths,
            None,
            None,
            None,
            None,
            None,
            None,
            policy_deny_gate().1,
        );

        let stub_llm = Arc::new(StubLlm::new());
        stub_llm.push_response(LlmResponse {
            content: r#"{"risk":"risky","rationale":"deletes target"}"#.to_owned(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: None,
        });
        let bind_source = Arc::new(BillableLlm::passthrough(
            Arc::clone(&stub_llm) as Arc<dyn LlmCompletion>
        ));

        let trace_store = Arc::new(baybo_trace::test_support::MemoryTraceStore::new());
        let session_id = SessionId::from("sess-bash-auto");
        let recorder = Arc::new(SpanRecorder::new(
            session_id.clone(),
            "u1".to_owned(),
            Arc::clone(&trace_store) as Arc<dyn baybo_trace::TraceStore>,
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
        let target = sandbox_root.join("to-delete.txt");
        std::fs::write(&target, "keep").expect("write");
        let params = json!({
            "command": format!("rm {}", target.display()),
        });
        let approved = Arc::new(Mutex::new(Vec::new()));

        // 1. Live call: judge says risky -> raises BeforeExecute ask -> denied ->
        //    file untouched, risk_judge trace events flushed.
        let executed = executor
            .execute(
                "Bash",
                params.clone(),
                &session_id,
                &ordinary_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &approved,
                &recorder,
                &step,
                None,
                "call-bash-1".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                Some(&bind_source),
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::Ask,
            )
            .await;

        let err = executed.output.expect_err("denied by gate");
        assert!(matches!(
            err.downcast_ref::<ToolError>(),
            Some(ToolError::Denied { .. })
        ));
        assert!(target.exists(), "denied command must not run");
        assert_eq!(stub_llm.captured_requests().len(), 1);
        {
            let asks = recorded.lock();
            assert_eq!(asks.len(), 1);
            assert_eq!(asks[0].1.phase, AskPhase::BeforeExecute);
            assert!(asks[0].0.params_preview.contains("deletes target"));
        }

        // Verify risk_judge span events were flushed even though the call was denied.
        let spans = trace_store
            .list_spans_by_step(&step.step_id)
            .await
            .expect("list spans");
        let span = Span::from_row(spans[0].clone()).expect("decode span");
        let event_rows = trace_store
            .list_span_events(&span.id)
            .await
            .expect("list span events");
        let events: Vec<SpanEvent> = event_rows
            .into_iter()
            .map(|r| SpanEvent::from_row(r).expect("decode span event"))
            .collect();
        assert!(
            events.iter().any(|ev| matches!(
                &ev.kind,
                SpanEventKind::ToolEvent { action, payload: ToolEventPayload::LlmCall { .. } }
                    if action == "risk_judge"
            )),
            "risk_judge events must be flushed on pre-execute denial: {events:?}"
        );

        // 2. Resumed PreDecided(ApproveAlways): runs without calling the judge LLM
        //    again, without re-prompting, and seeds the session cache.
        let original_accesses = recorded.lock()[0].0.accesses.clone();
        let executed_resumed = executor
            .execute(
                "Bash",
                params,
                &session_id,
                &ordinary_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &approved,
                &recorder,
                &step,
                None,
                "call-bash-1".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                Some(&bind_source),
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::PreDecided {
                    decision: ApprovalDecision::ApproveAlways,
                    phase: AskPhase::BeforeExecute,
                    accesses: original_accesses,
                },
            )
            .await;

        assert!(
            executed_resumed.output.is_ok(),
            "{:?}",
            executed_resumed.output.err()
        );
        assert!(!target.exists(), "resumed approved command executed");
        assert_eq!(
            stub_llm.captured_requests().len(),
            1,
            "PreDecided must not call the risk judge LLM a second time"
        );
        assert_eq!(recorded.lock().len(), 1, "PreDecided must not re-prompt");
        assert_eq!(
            *approved.lock(),
            vec![ApprovedResource::ExecCommand {
                command: format!("rm {}", target.display()),
            }],
            "ApproveAlways on auto destructive ask seeds the session cache"
        );
    }

    #[tokio::test]
    async fn skill_env_approval_runs_before_execute_uncached_and_errors_first_when_missing() {
        use baybo_skills::{AlwaysPass, SkillDefinition, SkillRegistry, SkillRequirements};

        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let skill_dir = home.path().join("demo-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# Demo\n").expect("skill md");

        let make_skill = |name: &str, required_env: Vec<String>| SkillDefinition {
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            description: "demo".to_owned(),
            command: Some(name.to_owned()),
            agent_invocable: true,
            channels: vec![],
            argument_hint: None,
            prompt_template: "# Demo\n".to_owned(),
            allowed_tools: vec![],
            source: baybo_model::ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            requirements: SkillRequirements {
                required_bins: vec![],
                required_env,
                required_models: vec![],
            },
            token_budget_hint: 0,
            source_path: Some(skill_dir.clone()),
            linked_files: Default::default(),
        };

        let skill_registry = Arc::new(SkillRegistry::new());
        skill_registry.register(make_skill(
            "missing-env",
            vec!["BAYBO_TEST_DEFINITELY_UNSET_VAR".to_owned()],
        ));
        skill_registry.register(make_skill("present-env", vec!["PATH".to_owned()]));

        let (skill_tool, skill_manifest) =
            baybo_skills::tools::build(skill_registry, Arc::new(AlwaysPass));
        let mut registry = ToolRegistry::new();
        registry.register(skill_tool, skill_manifest);

        let (gate, recorded) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::ApproveAlways));
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
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");
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
            policy_deny_gate().1,
        );

        let session_id = SessionId::from("sess-skill-env");
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
        let agent_id = baybo_model::AgentProfileId::builtin();
        let approved = Arc::new(Mutex::new(Vec::new()));

        // 1. Missing env var errors without ever prompting.
        let missing_res = executor
            .execute(
                baybo_skills::tools::SKILL_TOOL_NAME,
                json!({ "skill": "missing-env" }),
                &session_id,
                &ordinary_trigger(),
                &agent_id,
                &user,
                &approved,
                &recorder,
                &step,
                None,
                "call-skill-missing".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::Ask,
            )
            .await;
        let err = missing_res.output.expect_err("missing env must fail");
        assert!(
            err.to_string().contains("BAYBO_TEST_DEFINITELY_UNSET_VAR"),
            "unexpected error: {err}"
        );
        assert!(
            recorded.lock().is_empty(),
            "missing env var must not raise an approval prompt"
        );

        // 2. Present env var raises an uncached BeforeExecute ask; ApproveAlways
        //    runs the skill and does not cache the env grant.
        let present_res = executor
            .execute(
                baybo_skills::tools::SKILL_TOOL_NAME,
                json!({ "skill": "present-env" }),
                &session_id,
                &ordinary_trigger(),
                &agent_id,
                &user,
                &approved,
                &recorder,
                &step,
                None,
                "call-skill-present".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::Ask,
            )
            .await;
        assert!(present_res.output.is_ok(), "{:?}", present_res.output.err());
        assert_eq!(recorded.lock().len(), 1);
        assert_eq!(recorded.lock()[0].1.phase, AskPhase::BeforeExecute);
        assert_eq!(
            recorded.lock()[0].0.accesses,
            vec![ResourceAccess::Env {
                vars: vec!["PATH".to_owned()],
            }]
        );
        assert!(
            approved.lock().is_empty(),
            "ApproveAlways on Skill env must not persist to session cache"
        );
    }

    #[tokio::test]
    async fn bash_require_escalated_runs_before_execute_uncached_and_merges_destructive_warning() {
        for decision in [ApprovalDecision::Deny, ApprovalDecision::ApproveAlways] {
            assert_masked_bash_escalation(decision).await;
        }
    }

    async fn assert_masked_bash_escalation(decision: ApprovalDecision) {
        use baybo_tools::builtin::bash::BashTool;
        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        let bash = Arc::new(BashTool::new(
            paths.clone(),
            baybo_process::ProcessManager::transient(),
        ));
        let mut registry = ToolRegistry::new();
        registry.register(
            bash,
            ToolManifest {
                name: "Bash".to_owned(),
                description: "bash".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object", "properties": {}}),
                capabilities: vec![ToolCapability::ExecCommand],
                channels: Vec::new(),
            },
        );

        let (gate, recorded) = RecordingGate::new(ApprovalOutcome::answered(decision));
        let gates = ApprovalGateMap::new();
        gates.insert(baybo_model::ChannelType::tui(), gate);
        let key = baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("key");
        let vault = Arc::new(baybo_security::SecretVault::new(
            key,
            Arc::new(baybo_security::test_support::MemorySecretStore::new()),
        ));
        let placeholder = "[{REDACTED_SECRET_0123456789abcdef01234567}]";
        vault
            .store_secret(placeholder, sandbox_root.to_str().unwrap().as_bytes())
            .await
            .unwrap();
        let security_gateway = Arc::new(SecurityGateway::new(
            Arc::new(baybo_security::LeakDetector::with_default_rules()),
            vault,
        ));
        let executor = ToolExecutor::new(
            Arc::new(registry),
            Arc::new(gates),
            security_gateway,
            sandbox_root.clone(),
            paths,
            None,
            None,
            None,
            None,
            None,
            None,
            policy_deny_gate().1,
        );

        let session_id = SessionId::from("sess-bash-esc");
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

        let target = sandbox_root.join("esc-delete.txt");
        std::fs::write(&target, "x").expect("write");
        let cmd = format!("rm {placeholder}/esc-delete.txt");
        let params = json!({
            "command": cmd,
            "cwd": placeholder,
            "sandbox_permissions": "require_escalated",
            "justification": "Need host rm",
        });

        // Pre-seed the sandboxed form of this exact command in the cache; it
        // must NOT authorize `require_escalated`.
        let pre_seeded = ApprovedResource::ExecCommand {
            command: cmd.clone(),
        };
        let approved = Arc::new(Mutex::new(vec![pre_seeded.clone()]));

        let executed = executor
            .execute(
                "Bash",
                params.clone(),
                &session_id,
                &ordinary_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &approved,
                &recorder,
                &step,
                None,
                "call-esc-1".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::Ask,
            )
            .await;

        assert_eq!(
            recorded.lock().len(),
            1,
            "masked cwd must not bypass approval"
        );
        let request_json = serde_json::to_string(&recorded.lock()[0].0).unwrap();
        assert!(request_json.contains(placeholder));
        assert!(!request_json.contains(sandbox_root.to_str().unwrap()));
        if decision == ApprovalDecision::Deny {
            assert!(executed.output.is_err());
            assert!(target.exists(), "denied masked command must not run");
            return;
        }
        assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        assert!(!target.exists(), "approved escalated command ran");
        {
            let asks = recorded.lock();
            assert_eq!(asks.len(), 1, "one merged ask, not two");
            assert_eq!(asks[0].1.phase, AskPhase::BeforeExecute);
            let desc = asks[0].0.description.as_deref().expect("description");
            assert!(
                desc.contains("Need host rm")
                    && desc.contains("without the OS sandbox")
                    && desc.contains("Destructive command"),
                "merged description must carry justification, sandbox warning, and destructive warning: {desc}"
            );
        }

        // Now verify PreDecided(ApproveAlways) runs without prompting and
        // never inserts into an empty session cache.
        std::fs::write(&target, "y").expect("write again");
        let empty_cache = Arc::new(Mutex::new(Vec::new()));
        let original_accesses = recorded.lock()[0].0.accesses.clone();
        let executed_resumed = executor
            .execute(
                "Bash",
                params,
                &session_id,
                &ordinary_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &empty_cache,
                &recorder,
                &step,
                None,
                "call-esc-2".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::PreDecided {
                    decision: ApprovalDecision::ApproveAlways,
                    phase: AskPhase::BeforeExecute,
                    accesses: original_accesses,
                },
            )
            .await;
        assert!(
            executed_resumed.output.is_ok(),
            "{:?}",
            executed_resumed.output.err()
        );
        assert!(!target.exists());
        assert_eq!(recorded.lock().len(), 1, "PreDecided must not re-prompt");
        assert!(
            empty_cache.lock().is_empty(),
            "PreDecided(ApproveAlways) on require_escalated must not cache"
        );
    }

    #[tokio::test]
    async fn retry_unsandboxed_skips_pre_execute_ask_does_not_cache_and_runs_unsandboxed() {
        use baybo_tools::builtin::bash::BashTool;
        use baybo_tools::builtin::permission::{LivePermissionMode, PermissionMode};

        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        // Manual mode would normally raise a BeforeExecute prompt if
        // `PreDecided { phase: DuringExecuteResumable }` consulted `pre_execute_ask`.
        let bash = Arc::new(
            BashTool::new(paths.clone(), baybo_process::ProcessManager::transient())
                .with_permission_handle(Arc::new(LivePermissionMode::new(PermissionMode::Manual))),
        );
        let mut registry = ToolRegistry::new();
        registry.register(
            bash,
            ToolManifest {
                name: "Bash".to_owned(),
                description: "bash".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object", "properties": {}}),
                capabilities: vec![ToolCapability::ExecCommand],
                channels: Vec::new(),
            },
        );

        let (gate, recorded) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::Deny));
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
            policy_deny_gate().1,
        );

        let session_id = SessionId::from("sess-retry-unsandboxed");
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
        let approved = Arc::new(Mutex::new(Vec::new()));

        let executed = executor
            .execute(
                "Bash",
                json!({ "command": "printf 'retried-ok\\n'" }),
                &session_id,
                &ordinary_trigger(),
                &baybo_model::AgentProfileId::generate(),
                &user,
                &approved,
                &recorder,
                &step,
                None,
                "call-retry-1".to_owned(),
                None,
                None,
                idle_tokens(),
                None,
                None,
                TurnBackground::NONE,
                true,
                ReadTracker::default(),
                None,
                false,
                PreExecuteGate::PreDecided {
                    decision: ApprovalDecision::ApproveAlways,
                    phase: AskPhase::DuringExecuteResumable,
                    accesses: Vec::new(),
                },
            )
            .await;

        let out = executed.output.expect("resumed unsandboxed retry runs");
        let ToolOutput::Json(v) = out else {
            panic!("expected Json")
        };
        assert_eq!(v["stdout"], "retried-ok\n");
        assert!(
            v["sandbox_escalation"].as_str().is_some(),
            "expected sandbox_escalation note: {v}"
        );
        assert_eq!(executed.approval, Some(ApprovalDecision::ApproveAlways));
        assert!(
            recorded.lock().is_empty(),
            "PreDecided(DuringExecuteResumable) must not re-prompt"
        );
        assert!(
            approved.lock().is_empty(),
            "ApproveAlways on a resumed during-execute retry must never seed session grants"
        );
    }

    #[tokio::test]
    async fn approval_handle_stamps_during_execute_phase_from_tool_capability() {
        use baybo_tools::{DuringExecuteResumer, Tool};

        struct PlainTool;

        #[async_trait::async_trait]
        impl Tool for PlainTool {
            fn name(&self) -> &str {
                "plain_tool"
            }
            fn description(&self) -> String {
                "asks non-resumable during execute".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                ctx: &ToolContext,
            ) -> baybo_tools::Result<ToolOutput> {
                let _ = ctx
                    .approval
                    .as_ref()
                    .expect("approval handle")
                    .request_uncached(
                        self.name(),
                        &ctx.session_id,
                        &ctx.user,
                        vec![ResourceAccess::WriteFile {
                            path: PathBuf::from("/tmp/probe"),
                        }],
                        "probe".into(),
                    )
                    .await;
                Ok(ToolOutput::Text("ok".into()))
            }
        }

        struct ResumableTool;

        #[async_trait::async_trait]
        impl Tool for ResumableTool {
            fn name(&self) -> &str {
                "resumable_tool"
            }
            fn description(&self) -> String {
                "asks both non-resumable and resumable during execute".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            fn during_execute_resumer(&self) -> Option<&dyn DuringExecuteResumer> {
                Some(self)
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                ctx: &ToolContext,
            ) -> baybo_tools::Result<ToolOutput> {
                let approval = ctx.approval.as_ref().expect("approval handle");
                let _ = approval
                    .request_uncached(
                        self.name(),
                        &ctx.session_id,
                        &ctx.user,
                        vec![ResourceAccess::WriteFile {
                            path: PathBuf::from("/tmp/probe-plain"),
                        }],
                        "plain".into(),
                    )
                    .await;
                let _ = approval
                    .request_resumable_uncached(
                        self,
                        self.name(),
                        &ctx.session_id,
                        &ctx.user,
                        vec![ResourceAccess::WriteFile {
                            path: PathBuf::from("/tmp/probe-resumable"),
                        }],
                        "resumable".into(),
                    )
                    .await;
                Ok(ToolOutput::Text("ok".into()))
            }
        }

        #[async_trait::async_trait]
        impl DuringExecuteResumer for ResumableTool {
            async fn resume_during_execute(
                &self,
                _params: serde_json::Value,
                _ctx: &ToolContext,
            ) -> baybo_tools::Result<ToolOutput> {
                Ok(ToolOutput::Text("resumed".into()))
            }
        }

        let home = tempfile::tempdir().expect("tempdir");
        let paths = baybo_workspace::WorkspacePaths::new(home.path().join(".baybo"));
        std::fs::create_dir_all(paths.work_dir()).expect("work dir");
        let sandbox_root = paths.work_dir().canonicalize().expect("canonical work dir");

        let mut registry = ToolRegistry::new();
        registry.register(
            Arc::new(PlainTool),
            ToolManifest {
                name: "plain_tool".to_owned(),
                description: "test".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object"}),
                capabilities: Vec::new(),
                channels: Vec::new(),
            },
        );
        registry.register(
            Arc::new(ResumableTool),
            ToolManifest {
                name: "resumable_tool".to_owned(),
                description: "test".to_owned(),
                trust_level: TrustLevel::Trusted,
                parameters_schema: json!({"type": "object"}),
                capabilities: Vec::new(),
                channels: Vec::new(),
            },
        );

        let (gate, recorded) =
            RecordingGate::new(ApprovalOutcome::answered(ApprovalDecision::Approve));
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
            policy_deny_gate().1,
        );

        let session_id = SessionId::from("sess-phase-stamp");
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
        let approved = Arc::new(Mutex::new(Vec::new()));

        for tool_name in ["plain_tool", "resumable_tool"] {
            let executed = executor
                .execute(
                    tool_name,
                    json!({}),
                    &session_id,
                    &ordinary_trigger(),
                    &baybo_model::AgentProfileId::generate(),
                    &user,
                    &approved,
                    &recorder,
                    &step,
                    None,
                    format!("call-{tool_name}"),
                    None,
                    None,
                    idle_tokens(),
                    None,
                    None,
                    TurnBackground::NONE,
                    true,
                    ReadTracker::default(),
                    None,
                    false,
                    PreExecuteGate::Ask,
                )
                .await;
            assert!(executed.output.is_ok(), "{:?}", executed.output.err());
        }

        let asks = recorded.lock();
        assert_eq!(asks.len(), 3);
        assert_eq!(asks[0].0.tool, "plain_tool");
        assert_eq!(asks[0].1.phase, AskPhase::DuringExecuteNonResumable);
        assert_eq!(asks[1].0.tool, "resumable_tool");
        assert_eq!(asks[1].0.params_preview, "plain");
        assert_eq!(asks[1].1.phase, AskPhase::DuringExecuteNonResumable);
        assert_eq!(asks[2].0.tool, "resumable_tool");
        assert_eq!(asks[2].0.params_preview, "resumable");
        assert_eq!(asks[2].1.phase, AskPhase::DuringExecuteResumable);
    }
}
