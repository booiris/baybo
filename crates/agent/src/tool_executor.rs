use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use aura_model::{JobId, ParallelGroup, SessionId, SpanId, TrustLevel, User};

use aura_sandbox::{NetworkPolicy, SandboxRunner, default_sensitive_denylist};
use aura_tools::{
    ApprovalDecision, ApprovalGateMap, ApprovalHandle, ApprovalRequest, ApprovedResource,
    ExecSandbox, ResourceAccess, ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput,
    ToolRegistry, approval::preview_params,
};
use aura_trace::{
    LifecycleOutcome, SpanEventKind, SpanFinalize, SpanKind, StepHandle, ToolCallBegin,
    ToolCallOrigin, ToolCallResult,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::sandbox::SandboxAdapter;
use crate::security::SecurityGateway;
use crate::trace::SpanRecorder;

/// Preview length used when rendering parameters inside an approval prompt.
const APPROVAL_PARAMS_PREVIEW_LEN: usize = 512;

/// Headroom added to the per-tool outer timeout so a slow user
/// approval (handled mid-execution via [`aura_tools::ApprovalHandle`])
/// cannot kill the tool while it is legitimately blocked on a modal.
/// Tracks the channel gate's own `APPROVAL_TIMEOUT` in
/// `crates/gateway/src/channel/adapter.rs`; if you change one, change
/// the other.
const APPROVAL_HEADROOM: Duration = Duration::from_secs(300);

/// Cap on the failure-reason string we copy out of a `ToolOutput::Error`
/// payload. The full text is still preserved verbatim in the span's
/// `result.output`; this bound only governs the row-level reason label.
const FAILURE_REASON_MAX_BYTES: usize = 512;

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
fn tool_output_to_trace_value(output: &ToolOutput) -> Value {
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

/// Executes tools with trust-level validation, approval gating, and
/// observability recording.
pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    gate_map: Arc<ApprovalGateMap>,
    security_gateway: Arc<SecurityGateway>,
    workspace_root: PathBuf,
    sandbox_runner: Option<Arc<dyn SandboxRunner>>,
}

impl ToolExecutor {
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        gate_map: Arc<ApprovalGateMap>,
        security_gateway: Arc<SecurityGateway>,
        workspace_root: PathBuf,
        sandbox_runner: Option<Arc<dyn SandboxRunner>>,
    ) -> Self {
        Self {
            tool_registry,
            gate_map,
            security_gateway,
            workspace_root,
            sandbox_runner,
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
        user: &User,
        approved_resources: &Arc<Mutex<Vec<ApprovedResource>>>,
        recorder: &Arc<SpanRecorder>,
        step: &StepHandle,
        triggering_llm_span: Option<SpanId>,
        tool_use_id: String,
        parallel_group: Option<ParallelGroup>,
        _parent_job_for_log: Option<JobId>,
        cancel_token: CancellationToken,
        notifier: Option<Arc<dyn aura_tools::SessionNotifier>>,
    ) -> anyhow::Result<ToolOutput> {
        debug!(tool = tool_name, "executing tool");

        if let Some(manifest) = self.tool_registry.get_manifest(tool_name) {
            self.validate_trust(tool_name, &manifest)?;
        }

        let job_id = step.job_id;
        let tool_name_owned = tool_name.to_string();
        // Reborrow the cancel token so the closure can move its
        // ownership into `ctx` while `with_span` keeps a borrow for
        // the cancel-aware close-on-Err path.
        let cancel_for_close = cancel_token.clone();

        // Open the tool span up front so denials and approval failures
        // still appear in the trace tree. The handle carries the
        // begin-time kind, so close-time only supplies the result.
        crate::scope::with_span(
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
            Some((&cancel_for_close, aura_job::CancelReason::ParentCancelled)),
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
                    let gate = self.gate_map.get(&user.channel, session_id.as_str());
                    let decision = gate
                        .request(ApprovalRequest {
                            call_id: Uuid::new_v4().to_string(),
                            session_id: session_id.to_string(),
                            user_id: user.id.clone(),
                            tool: tool_name_owned.clone(),
                            accesses: uncovered.clone(),
                            params_preview: preview_params(&params, APPROVAL_PARAMS_PREVIEW_LEN),
                            description: call_label.clone(),
                        })
                        .await;
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
                }

                // Build per-call sandbox adapter for tools declaring
                // ExecCommand.
                let sandbox: Option<Arc<dyn ExecSandbox>> = if let Some(manifest) =
                    self.tool_registry.get_manifest(&tool_name_owned)
                    && manifest.capabilities.contains(&ToolCapability::ExecCommand)
                {
                    self.sandbox_runner.as_ref().map(|runner| {
                        let home = std::env::var_os("HOME").map(PathBuf::from);
                        let aura_state = std::env::var_os("AURA_HOME")
                            .map(PathBuf::from)
                            .or_else(|| home.as_ref().map(|h| h.join(".aura")));
                        let extra_root =
                            home.clone().unwrap_or_else(|| self.workspace_root.clone());
                        let denied =
                            default_sensitive_denylist(home.as_deref(), aura_state.as_deref());
                        Arc::new(
                            SandboxAdapter::new(
                                Arc::clone(runner),
                                self.workspace_root.clone(),
                                NetworkPolicy::All,
                            )
                            .with_permissive_filesystem(extra_root, denied),
                        ) as Arc<dyn ExecSandbox>
                    })
                } else {
                    None
                };

                // Mid-execution approval handle.
                let approval_gate = self.gate_map.get(&user.channel, session_id.as_str());
                let approval = ApprovalHandle::new(approval_gate, Arc::clone(approved_resources));

                // Build tool context. The token comes from the agent
                // loop's per-job cancel tree — tripping it (via
                // JobLifecycle::cancel or a parent subagent's cascade)
                // signals the running tool.
                let ctx = ToolContext {
                    session_id: session_id.to_string(),
                    user: user.clone(),
                    timeout: effective_timeout,
                    cancellation_token: cancel_token,
                    workspace_root: self.workspace_root.clone(),
                    sandbox,
                    approval: Some(approval),
                    notifier: notifier.clone(),
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

                // Execute with timeout enforcement.
                let outer_deadline = ctx.timeout + APPROVAL_HEADROOM;
                let result = tokio::time::timeout(
                    outer_deadline,
                    self.tool_registry
                        .execute(&tool_name_owned, params_revealed, &ctx),
                )
                .await;

                match result {
                    Ok(Ok(mut output)) => {
                        // Defensive sanitize before result flows into
                        // trace / memory / next LLM call.
                        self.security_gateway
                            .sanitize_tool_output(&mut output)
                            .await
                            .map_err(|e| anyhow::anyhow!("sanitize_tool_output: {e}"))?;
                        let output_value = tool_output_to_trace_value(&output);
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
                        // text. Downcasts on this path aren't used (the
                        // `ToolError::Denied` downcast in `agent_loop`
                        // sees the approval-gate Err, which is built
                        // separately above).
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
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trace_value_preserves_text_payload() {
        let v = tool_output_to_trace_value(&ToolOutput::Text("hello world".into()));
        assert_eq!(v, json!({ "type": "text", "text": "hello world" }));
    }

    #[test]
    fn trace_value_preserves_error_payload() {
        let v = tool_output_to_trace_value(&ToolOutput::Error("read_page failed: …".into()));
        assert_eq!(
            v,
            json!({ "type": "error", "error": "read_page failed: …" })
        );
    }

    #[test]
    fn trace_value_preserves_json_object_shape() {
        // NowTool-shaped output: an object payload. The historical
        // shape — type tag flattened into the inner map — is what the
        // web UI is already showing for spans like Now, so changing
        // it would invalidate older traces.
        let v = tool_output_to_trace_value(&ToolOutput::Json(json!({
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
    }

    #[test]
    fn trace_value_wraps_non_object_json_payload() {
        let v = tool_output_to_trace_value(&ToolOutput::Json(json!("scalar")));
        assert_eq!(v, json!({ "type": "json", "value": "scalar" }));
    }
}
