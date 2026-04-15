use std::sync::Arc;
use std::time::Duration;

use aura_job::OperationKind;
use aura_model::{ChannelType, User};
use aura_registry::TrustLevel;

use aura_tools::{ToolCapability, ToolContext, ToolManifest, ToolOutput, ToolRegistry};
use aura_trace::{ExecutionProvenance, SpanInput, SpanResult};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::observability::ObservabilityRecorder;

/// Executes tools with trust-level validation and observability recording.
pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    default_timeout: Duration,
}

impl ToolExecutor {
    pub fn new(tool_registry: Arc<ToolRegistry>, default_timeout: Duration) -> Self {
        Self {
            tool_registry,
            default_timeout,
        }
    }

    /// Validate that the tool's trust level permits execution with its declared capabilities.
    ///
    /// - Untrusted tools are never auto-executed.
    /// - Installed tools cannot use WriteWorkspace or SpawnProcess.
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
                        ToolCapability::WriteWorkspace => {
                            anyhow::bail!(
                                "security: tool '{}' is Installed but declares WriteWorkspace capability; \
                                 requires Trusted level",
                                tool_name
                            );
                        }
                        ToolCapability::SpawnProcess => {
                            anyhow::bail!(
                                "security: tool '{}' is Installed but declares SpawnProcess capability; \
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

    /// Execute a tool call with full observability recording.
    pub async fn execute(
        &self,
        tool_name: &str,
        params: Value,
        session_id: &str,
        user: &User,
        recorder: &ObservabilityRecorder,
        parent_job_id: Option<&str>,
    ) -> anyhow::Result<ToolOutput> {
        debug!(tool = tool_name, "executing tool");

        if let Some(manifest) = self.tool_registry.get_manifest(tool_name) {
            self.validate_trust(tool_name, manifest)?;
        }

        // Begin observability recording
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

        // Build tool context
        let ctx = ToolContext {
            session_id: session_id.to_string(),
            user: user.clone(),
            timeout: self.default_timeout,
            cancellation_token: CancellationToken::new(),
        };

        // Execute with timeout enforcement
        let start = std::time::Instant::now();
        let result = tokio::time::timeout(
            ctx.timeout,
            self.tool_registry.execute(tool_name, params, &ctx),
        )
        .await;

        let elapsed = start.elapsed();

        match result {
            Ok(Ok(output)) => {
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
                let error_msg = e.to_string();
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

    /// Run a tool outside an agent turn, for operator-driven testing via
    /// `aura tools test`. Routes through the same observability path as a live
    /// turn so the attempt shows up in trace and cost records; the session id
    /// is synthetic so the execution does not attach to any real chat session.
    pub async fn test_execute(
        &self,
        tool_name: &str,
        params: Value,
        recorder: &ObservabilityRecorder,
    ) -> anyhow::Result<ToolOutput> {
        let session_id = format!("cli-test-{}", uuid::Uuid::new_v4());
        let user = User {
            id: "cli-operator".into(),
            name: Some("operator".into()),
            channel: ChannelType::Tui,
        };
        self.execute(tool_name, params, &session_id, &user, recorder, None)
            .await
    }
}
