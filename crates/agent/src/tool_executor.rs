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
    LifecycleOutcome, SpanEventKind, SpanKind, SpanResult, StepHandle, ToolCallOrigin,
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

/// Executes tools with trust-level validation, approval gating, and
/// observability recording.
pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    default_timeout: Duration,
    gate_map: Arc<ApprovalGateMap>,
    security_gateway: Arc<SecurityGateway>,
    workspace_root: PathBuf,
    sandbox_runner: Option<Arc<dyn SandboxRunner>>,
}

impl ToolExecutor {
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        default_timeout: Duration,
        gate_map: Arc<ApprovalGateMap>,
        security_gateway: Arc<SecurityGateway>,
        workspace_root: PathBuf,
        sandbox_runner: Option<Arc<dyn SandboxRunner>>,
    ) -> Self {
        Self {
            tool_registry,
            default_timeout,
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
        parallel_group: Option<ParallelGroup>,
        _parent_job_for_log: Option<JobId>,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<ToolOutput> {
        debug!(tool = tool_name, "executing tool");

        if let Some(manifest) = self.tool_registry.get_manifest(tool_name) {
            self.validate_trust(tool_name, &manifest)?;
        }

        // Open the tool span up front so denials and approval failures
        // still appear in the trace tree. The handle carries the
        // begin-time kind, so close-time only supplies the result.
        let span_handle = recorder
            .begin_span(
                step,
                SpanKind::ToolCall {
                    tool_name: tool_name.to_string(),
                    // ToolManifest does not yet carry an artifact hash.
                    tool_artifact_hash: String::new(),
                    triggered_by: triggering_llm_span.map(|llm_span_id| ToolCallOrigin {
                        llm_span_id,
                        tool_use_id: String::new(),
                    }),
                    params: params.clone(),
                    output: Value::Null,
                    success: false,
                },
                parallel_group,
            )
            .await?;
        let mut event_seq: u32 = 0;

        // Approval gate: derive resource accesses from the tool and
        // check them against the session's cached approvals.
        let (accesses, call_label) = self
            .tool_registry
            .get(tool_name)
            .map(|tool| (tool.accessed_resources(&params), tool.call_label(&params)))
            .unwrap_or_default();

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
                    tool: tool_name.to_string(),
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
                    info!(tool = tool_name, "tool call approved once");
                }
                ApprovalDecision::ApproveAlways => {
                    info!(tool = tool_name, "tool call approved always");
                    let mut approved = approved_resources.lock();
                    for access in &uncovered {
                        let entry = access.to_approved();
                        if !approved.iter().any(|existing| existing == &entry) {
                            approved.push(entry);
                        }
                    }
                }
                ApprovalDecision::Deny => {
                    let reason = "user denied approval".to_string();
                    let _ = recorder
                        .end_span(
                            span_handle,
                            step.job_id,
                            SpanResult::ToolCall {
                                output: Value::Null,
                                success: false,
                            },
                            LifecycleOutcome::Failed {
                                reason: reason.clone(),
                            },
                        )
                        .await;
                    return Err(ToolError::Denied {
                        tool: tool_name.to_string(),
                        reason,
                    }
                    .into());
                }
            }
        }

        // Build per-call sandbox adapter for tools declaring ExecCommand.
        let sandbox: Option<Arc<dyn ExecSandbox>> = if let Some(manifest) =
            self.tool_registry.get_manifest(tool_name)
            && manifest.capabilities.contains(&ToolCapability::ExecCommand)
        {
            self.sandbox_runner.as_ref().map(|runner| {
                let home = std::env::var_os("HOME").map(PathBuf::from);
                let aura_state = std::env::var_os("AURA_HOME")
                    .map(PathBuf::from)
                    .or_else(|| home.as_ref().map(|h| h.join(".aura")));
                let extra_root = home.clone().unwrap_or_else(|| self.workspace_root.clone());
                let denied = default_sensitive_denylist(home.as_deref(), aura_state.as_deref());
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

        // Build tool context. The token comes from the agent loop's
        // per-job cancel tree — tripping it (via JobLifecycle::cancel
        // or a parent subagent's cascade) signals the running tool.
        let ctx = ToolContext {
            session_id: session_id.to_string(),
            user: user.clone(),
            timeout: self.default_timeout,
            cancellation_token: cancel_token,
            workspace_root: self.workspace_root.clone(),
            sandbox,
            approval: Some(approval),
        };

        // Reveal placeholders in the tool's arguments just before
        // execution. The pre-reveal `params` is what the trace + approval
        // surfaces saw; the tool itself receives plaintext for its API call.
        let mut params_revealed = params.clone();
        self.security_gateway
            .reveal_in_value(&mut params_revealed)
            .await?;

        // Execute with timeout enforcement.
        let outer_deadline = ctx.timeout + APPROVAL_HEADROOM;
        let result = tokio::time::timeout(
            outer_deadline,
            self.tool_registry.execute(tool_name, params_revealed, &ctx),
        )
        .await;

        match result {
            Ok(Ok(mut output)) => {
                // Defensive sanitize before result flows into trace /
                // memory / next LLM call.
                self.security_gateway
                    .sanitize_tool_output(&mut output)
                    .await?;
                let output_value = serde_json::to_value(&output).unwrap_or(Value::Null);
                let success = !matches!(output, ToolOutput::Error(_));
                let outcome = if success {
                    LifecycleOutcome::Ok
                } else {
                    LifecycleOutcome::Failed {
                        reason: "tool returned error output".into(),
                    }
                };
                recorder
                    .end_span(
                        span_handle,
                        step.job_id,
                        SpanResult::ToolCall {
                            output: output_value,
                            success,
                        },
                        outcome,
                    )
                    .await?;
                Ok(output)
            }
            Ok(Err(e)) => {
                let raw = e.to_string();
                let error_msg = self
                    .security_gateway
                    .sanitize_error(&raw)
                    .await
                    .unwrap_or(raw);
                recorder
                    .end_span(
                        span_handle,
                        step.job_id,
                        SpanResult::ToolCall {
                            output: Value::Null,
                            success: false,
                        },
                        LifecycleOutcome::Failed {
                            reason: error_msg.clone(),
                        },
                    )
                    .await?;
                Err(e.into())
            }
            Err(_) => {
                let error_msg =
                    format!("tool '{}' exceeded timeout ({:?})", tool_name, ctx.timeout);
                recorder
                    .end_span(
                        span_handle,
                        step.job_id,
                        SpanResult::ToolCall {
                            output: Value::Null,
                            success: false,
                        },
                        LifecycleOutcome::Failed {
                            reason: error_msg.clone(),
                        },
                    )
                    .await?;
                Err(anyhow::anyhow!(
                    "timeout: tool '{}' exceeded timeout",
                    tool_name
                ))
            }
        }
    }
}
