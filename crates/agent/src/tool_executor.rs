use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use aura_job::OperationKind;
use aura_model::TrustLevel;
use aura_model::User;

use aura_sandbox::{NetworkPolicy, SandboxRunner};
use aura_tools::{
    ApprovalDecision, ApprovalGateMap, ApprovalRequest, ApprovedResource, ExecSandbox,
    ResourceAccess, ToolCapability, ToolContext, ToolError, ToolManifest, ToolOutput, ToolRegistry,
    approval::preview_params,
};
use aura_trace::{ExecutionProvenance, SpanInput, SpanResult};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use uuid::Uuid;

use crate::observability::ObservabilityRecorder;
use crate::sandbox::SandboxAdapter;
use crate::security::SecurityGateway;

/// Preview length used when rendering parameters inside an approval prompt.
const APPROVAL_PARAMS_PREVIEW_LEN: usize = 512;

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
    ///
    /// - Untrusted tools are never auto-executed.
    /// - Installed tools cannot use `WriteFile` or `ExecCommand`.
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

    /// Execute a tool call with full observability and approval gating.
    ///
    /// `approved_resources` is a shared, mutable set of session-scoped
    /// approvals. The executor reads it to check coverage and writes to
    /// it on `ApproveAlways`, so concurrent tool calls within a turn see
    /// each other's grants immediately. The caller flushes the final
    /// contents back into `session.state` after all calls complete.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        tool_name: &str,
        params: Value,
        session_id: &str,
        user: &User,
        approved_resources: &Mutex<Vec<ApprovedResource>>,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
    ) -> anyhow::Result<ToolOutput> {
        debug!(tool = tool_name, "executing tool");

        if let Some(manifest) = self.tool_registry.get_manifest(tool_name) {
            self.validate_trust(tool_name, &manifest)?;
        }

        // Begin observability recording up front so denial / approval
        // failures still appear in the trace.
        let handle = recorder
            .begin(
                session_id,
                OperationKind::ToolExecution {
                    tool_name: tool_name.to_string(),
                },
                parent_job_id,
                ExecutionProvenance::default(),
                SpanInput::ToolExecution {
                    parameters: params.clone(),
                },
            )
            .await?;

        // Approval gate: derive resource accesses from the tool and check
        // them against the session's cached approvals. If any access is
        // uncovered, prompt the user via the gate.
        let accesses = self
            .tool_registry
            .get(tool_name)
            .map(|tool| tool.accessed_resources(&params))
            .unwrap_or_default();

        let uncovered: Vec<ResourceAccess> = {
            let approved = approved_resources.lock();
            accesses
                .iter()
                .filter(|acc| {
                    // Read-only accesses are always allowed without approval.
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
            let decision = gate
                .request(ApprovalRequest {
                    call_id: Uuid::new_v4().to_string(),
                    session_id: session_id.to_string(),
                    user_id: user.id.clone(),
                    tool: tool_name.to_string(),
                    accesses: uncovered.clone(),
                    params_preview: preview_params(&params, APPROVAL_PARAMS_PREVIEW_LEN),
                })
                .await;
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
                    recorder.fail(handle, &reason).await?;
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
            let policy = if manifest.capabilities.contains(&ToolCapability::Http) {
                NetworkPolicy::All
            } else {
                NetworkPolicy::None
            };
            self.sandbox_runner.as_ref().map(|runner| {
                Arc::new(SandboxAdapter::new(
                    Arc::clone(runner),
                    self.workspace_root.clone(),
                    policy,
                )) as Arc<dyn ExecSandbox>
            })
        } else {
            None
        };

        // Build tool context
        let ctx = ToolContext {
            session_id: session_id.to_string(),
            user: user.clone(),
            timeout: self.default_timeout,
            cancellation_token: CancellationToken::new(),
            workspace_root: self.workspace_root.clone(),
            sandbox,
        };

        // Reveal placeholders in the tool's arguments just before
        // execution. The pre-reveal `params` was already captured in
        // `SpanInput::ToolExecution` above and in the approval prompt, so
        // the trace / approval surfaces keep placeholder form while the
        // tool itself receives plaintext for its API call.
        let mut params_revealed = params.clone();
        self.security_gateway
            .reveal_in_value(&mut params_revealed)
            .await?;

        // Execute with timeout enforcement
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            ctx.timeout,
            self.tool_registry.execute(tool_name, params_revealed, &ctx),
        )
        .await;

        let elapsed = start.elapsed();

        match result {
            Ok(Ok(mut output)) => {
                // Defensive: if the tool echoed back any secret-looking
                // content, sanitize before the result flows into trace,
                // memory, or the next LLM call as a tool-result message.
                self.security_gateway
                    .sanitize_tool_output(&mut output)
                    .await?;
                let output_value = serde_json::to_value(&output).unwrap_or(Value::Null);
                let result = SpanResult::ToolResult {
                    output: output_value.clone(),
                    success: !matches!(output, ToolOutput::Error(_)),
                    latency: elapsed,
                };
                recorder.succeed(handle, output_value, result).await?;
                Ok(output)
            }
            Ok(Err(e)) => {
                let raw = e.to_string();
                let error_msg = self
                    .security_gateway
                    .sanitize_error(&raw)
                    .await
                    .unwrap_or(raw);
                recorder.fail(handle, &error_msg).await?;
                Err(e.into())
            }
            Err(_) => {
                let error_msg =
                    format!("tool '{}' exceeded timeout ({:?})", tool_name, ctx.timeout);
                recorder.fail(handle, &error_msg).await?;
                Err(anyhow::anyhow!(
                    "timeout: tool '{}' exceeded timeout",
                    tool_name
                ))
            }
        }
    }
}
