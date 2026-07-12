use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use baybo_llm::{Attribution, BillableLlm, BilledChat};
use baybo_model::{JobId, ParallelGroup, SessionId, SpanId, TrustLevel, User};

use baybo_sandbox::{NetworkPolicy, SandboxRunner, default_sensitive_denylist};
use baybo_tools::{
    ApprovalDecision, ApprovalGateMap, ApprovalHandle, ApprovalRequest, ApprovedResource,
    ExecSandbox, ReadTracker, ResourceAccess, ToolCapability, ToolContext, ToolError, ToolManifest,
    ToolOutput, ToolRegistry, VirtualReadResolver, approval::preview_params,
};
use baybo_trace::{
    LifecycleOutcome, SpanEventKind, SpanFinalize, SpanKind, SpanRecorder, StepHandle,
    ToolCallBegin, ToolCallOrigin, ToolCallResult, ToolEventPayload,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::runtime::sandbox::SandboxAdapter;
use crate::security::SecurityGateway;

/// Preview length used when rendering parameters inside an approval prompt.
const APPROVAL_PARAMS_PREVIEW_LEN: usize = 512;

/// Headroom added to the per-tool outer timeout so a slow user
/// approval (handled mid-execution via [`baybo_tools::ApprovalHandle`])
/// cannot kill the tool while it is legitimately blocked on a modal.
/// Tracks the channel gate's own `APPROVAL_TIMEOUT` in
/// `crates/gateway/src/channel/adapter.rs`; if you change one, change
/// the other.
const APPROVAL_HEADROOM: Duration = Duration::from_secs(300);

/// Cap on the failure-reason string we copy out of a `ToolOutput::Error`
/// payload. This bound governs only the row-level reason label; the body
/// lands in the span's `result.output`, itself capped at
/// `MAX_TOOL_OUTPUT_BYTES` (see [`cap_trace_output`]).
const FAILURE_REASON_MAX_BYTES: usize = 512;

/// Hard cap on the total byte size of text fields carried inside the
/// drained payloads of a single tool call. Tools are expected to
/// truncate their own snippets/inputs/outputs before emitting, but
/// this cap protects `span_events` from a tool that emits one very
/// large payload (or many medium ones). Entries that would push the
/// running total past this cap are dropped silently — the trace is
/// best-effort, not load-bearing.
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
    fn emit(&self, action: &str, payload: ToolEventPayload) {
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
fn tool_output_to_trace_value(output: &ToolOutput) -> (Value, Option<usize>) {
    use serde_json::Map;
    let value = match output {
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
    };
    cap_trace_output(value)
}

/// Cap the span's copy of a tool result at the same byte budget the LLM
/// transcript uses. The transcript copy is capped *and* spilled to a file one
/// layer up (`AgentLoop::cap_tool_output`); the span kept the raw payload, so a
/// single 285 KB `Grep` result was stored verbatim forever — a third copy of
/// bytes the model never saw.
///
/// Capping happens here, after the variant-shaping match, so no variant can
/// bypass it: the struct variants (`WithAttachments` / `MultiModalText`) route
/// through `serde_json::to_value` and would slip past a per-variant cap.
/// Attachments themselves are `BlobRef` pointers, not bytes, so only the
/// text-bearing payload is ever large.
///
/// Returns the untruncated serialized length when it cut, for
/// `ToolCallResult::output_truncated_from`.
fn cap_trace_output(value: Value) -> (Value, Option<usize>) {
    use baybo_context::prompts::tool_output::MAX_TOOL_OUTPUT_BYTES;

    let full_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
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

    let mut cut = MAX_TOOL_OUTPUT_BYTES.min(body.len());
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = serde_json::Map::new();
    out.insert("type".into(), Value::String(tag));
    out.insert(field.into(), Value::String(body[..cut].to_string()));
    (Value::Object(out), Some(full_len))
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
    /// Runtime hook for backgrounding a slow command. Forwarded into a
    /// [`ToolContext`] **only when the call is `background_eligible`** (a
    /// user-facing session) — see [`Self::execute`]. `None` ⇒ no manager
    /// wired (argv-mode boots, tests), so commands keep kill-on-timeout.
    background_jobs: Option<Arc<dyn baybo_tools::BackgroundJobSink>>,
    /// Control surface for the `JobList` / `JobStop` tools. Gated like
    /// [`Self::background_jobs`] (same runtime component implements both).
    background_control: Option<Arc<dyn baybo_tools::BackgroundJobControl>>,
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
        }
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
    /// concurrent siblings together; `job_id` is the parent job (for
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
        // [`ToolContext::session_trigger`]; `CronCreate` reads it so a job
        // created inside a cron fire inherits the fire's origin conversation.
        session_trigger: &baybo_model::TriggerSource,
        user: &User,
        approved_resources: &Arc<Mutex<Vec<ApprovedResource>>>,
        recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        triggering_llm_span: Option<SpanId>,
        tool_use_id: String,
        parallel_group: Option<ParallelGroup>,
        _parent_job_for_log: Option<JobId>,
        cancel_token: CancellationToken,
        notifier: Option<Arc<dyn baybo_tools::SessionNotifier>>,
        // `None` ⇒ tool's `ctx.llm` is unset (argv-mode / older tests).
        bind_source: Option<&Arc<BillableLlm>>,
        // Whether this session may background work (user-facing session).
        // Gates whether the [`BackgroundJobSink`] reaches the tool's ctx.
        background_eligible: bool,
        // Per-session read-before-write tracker. `Read` records into it;
        // `Edit`/`Write` enforce against it. Cheap to clone (one `Arc`).
        read_tracker: ReadTracker,
    ) -> ExecutedTool {
        debug!(tool = tool_name, "executing tool");

        if let Some(manifest) = self.tool_registry.get_manifest(tool_name)
            && let Err(e) = self.validate_trust(tool_name, &manifest)
        {
            return ExecutedTool {
                output: Err(e),
                approval: None,
            };
        }

        let job_id = step.job_id;
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

        // Open the tool span up front so denials and approval failures
        // still appear in the trace tree. The handle carries the
        // begin-time kind, so close-time only supplies the result.
        let output = crate::runtime::scope::with_span(
            recorder.as_ref(),
            step,
            job_id,
            SpanKind::ToolCall {
                begin: ToolCallBegin {
                    tool_name: tool_name_owned.clone(),
                    // ToolManifest does not yet carry an artifact hash.
                    tool_artifact_hash: String::new(),
                    triggered_by: triggering_llm_span.map(|llm_span_id| ToolCallOrigin {
                        llm_span_id,
                        tool_use_id: tool_use_id.clone(),
                    }),
                    params: params.clone(),
                },
                result: None,
            },
            parallel_group,
            Some((&cancel_for_close, baybo_job::CancelReason::ParentCancelled)),
            |span_handle| async move {
                let mut event_seq: u32 = 0;

                // Approval gate: derive resource accesses from the tool
                // and check them against the session's cached approvals.
                // Also pull the tool's declared `max_timeout` (default
                // 30 s) so the executor uses the right deadline below.
                let (accesses, call_label, effective_timeout) = self
                    .tool_registry
                    .get(&tool_name_owned)
                    .map(|tool| {
                        (
                            tool.accessed_resources(&params),
                            tool.call_label(&params),
                            tool.max_timeout(),
                        )
                    })
                    .unwrap_or_else(|| (Vec::new(), None, Duration::from_secs(30)));

                let uncovered: Vec<ResourceAccess> = {
                    let approved = approved_resources.lock();
                    accesses
                        .iter()
                        .filter(|acc| {
                            if matches!(acc, ResourceAccess::ReadFile { .. }) {
                                return false;
                            }
                            !approved.iter().any(|ar| ar.covers(acc))
                        })
                        .cloned()
                        .collect()
                };

                if !uncovered.is_empty() {
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
                    let decision = tokio::select! {
                        d = request => d,
                        _ = cancel_for_gate.cancelled() => ApprovalDecision::Deny,
                    };
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
                                reason: "user denied approval".to_string(),
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
                let uses_exec_command = self
                    .tool_registry
                    .get_manifest(&tool_name_owned)
                    .is_some_and(|manifest| {
                        manifest.capabilities.contains(&ToolCapability::ExecCommand)
                    });
                let sandbox: Option<Arc<dyn ExecSandbox>> = if uses_exec_command {
                    self.sandbox_runner.as_ref().map(|runner| {
                        let home = std::env::var_os("HOME").map(PathBuf::from);
                        let baybo_state = std::env::var_os("BAYBO_HOME")
                            .map(PathBuf::from)
                            .or_else(|| home.as_ref().map(|h| h.join(".baybo")));
                        let extra_root =
                            home.clone().unwrap_or_else(|| self.workspace_root.clone());
                        let denied =
                            default_sensitive_denylist(home.as_deref(), baybo_state.as_deref());
                        Arc::new(
                            SandboxAdapter::new(
                                Arc::clone(runner),
                                self.workspace_root.clone(),
                                NetworkPolicy::All,
                            )
                            .with_permissive_filesystem(extra_root, denied)
                            .with_readable_paths(vec![self.workspace_paths.skills_dir()]),
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
                let approval_gate = self.gate_map.get(&user.channel, session_id);
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
                // loop's per-job cancel tree — tripping it (via
                // JobLifecycle::cancel or a parent subagent's cascade)
                // signals the running tool. The billed-LLM handle (if
                // any) is bound to this exact tool span so a tool that
                // makes a side-LLM call (e.g. `WebFetch`'s extraction)
                // sees its `cost_records` row attributed to the running
                // tool span, not a synthetic placeholder.
                let llm: Option<Arc<dyn BilledChat>> = bind_source.map(|guarded| {
                    let bound = guarded.bind(Attribution {
                        user_id: user.id.clone(),
                        session_id: session_id.clone(),
                        job_id,
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
                    job_id,
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
                    llm,
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
                    // Gate: the background-job sink reaches a tool only for
                    // user-facing sessions, so a command can convert to
                    // background on timeout only there.
                    background_jobs: if background_eligible {
                        self.background_jobs.clone()
                    } else {
                        None
                    },
                    background_control: if background_eligible {
                        self.background_control.clone()
                    } else {
                        None
                    },
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
                let result = tokio::time::timeout(
                    outer_deadline,
                    self.tool_registry
                        .execute(&tool_name_owned, params_revealed, &ctx),
                )
                .await;

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
                        self.security_gateway
                            .sanitize_tool_output(&mut output)
                            .await
                            .map_err(|e| anyhow::anyhow!("sanitize_tool_output: {e}"))?;
                        let (output_value, output_truncated_from) =
                            tool_output_to_trace_value(&output);
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
                        Ok((
                            SpanFinalize::ToolCall(ToolCallResult {
                                output: output_value,
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
    use super::*;
    use serde_json::json;

    use baybo_context::prompts::tool_output::MAX_TOOL_OUTPUT_BYTES;

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
}
