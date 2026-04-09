use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aura_job::OperationKind;
use aura_registry::TrustLevel;
use aura_sandbox::{NetworkPolicy, SandboxPolicy};
use aura_session::User;

use crate::security::SecretVault;
use aura_tools::{
    SecretValue, ToolCapability, ToolContext, ToolManifest, ToolOutput, ToolRegistry,
};
use aura_trace::{ExecutionProvenance, SpanInput, SpanResult};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::observability::ObservabilityRecorder;

/// Executes tools with secret injection and observability recording.
pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    secret_vault: Arc<SecretVault>,
    default_timeout: Duration,
}

impl ToolExecutor {
    pub fn new(
        tool_registry: Arc<ToolRegistry>,
        secret_vault: Arc<SecretVault>,
        default_timeout: Duration,
    ) -> Self {
        Self {
            tool_registry,
            secret_vault,
            default_timeout,
        }
    }

    /// Resolve sandbox and network policies based on tool capabilities and trust level.
    ///
    /// Built-in tools get `WasmOnly` with empty network policy.
    /// WASM tools are examined for capabilities to determine the appropriate policy.
    fn resolve_policy(&self, tool_name: &str) -> (SandboxPolicy, NetworkPolicy) {
        let manifest = match self.tool_registry.get_manifest(tool_name) {
            Some(m) => m,
            None => {
                // Built-in tool: default restrictive policy
                debug!(tool = tool_name, "built-in tool, using WasmOnly policy");
                return (
                    SandboxPolicy::WasmOnly,
                    NetworkPolicy {
                        allowed_domains: vec![],
                        allow_loopback: false,
                    },
                );
            }
        };

        let mut sandbox = SandboxPolicy::WasmOnly;
        let mut allowed_domains = Vec::new();
        let mut allow_loopback = false;

        for cap in &manifest.capabilities {
            match cap {
                ToolCapability::SpawnProcess | ToolCapability::BrowserAutomation => {
                    sandbox = SandboxPolicy::ContainerRestricted;
                }
                ToolCapability::WriteWorkspace => {
                    if manifest.trust_level == TrustLevel::Trusted {
                        // Only upgrade to WorkspaceWrite if not already ContainerRestricted
                        if sandbox == SandboxPolicy::WasmOnly {
                            sandbox = SandboxPolicy::WorkspaceWrite;
                        }
                    }
                    // For non-Trusted, WriteWorkspace is denied at the trust validation step
                }
                ToolCapability::Http(domains) => {
                    allowed_domains.extend(domains.iter().cloned());
                }
                ToolCapability::ReadWorkspace => {
                    // ReadWorkspace does not escalate sandbox policy
                }
            }
        }

        // Allow loopback only in container modes (for local service access)
        if sandbox == SandboxPolicy::ContainerRestricted
            || sandbox == SandboxPolicy::ContainerElevated
        {
            allow_loopback = true;
        }

        (
            sandbox,
            NetworkPolicy {
                allowed_domains,
                allow_loopback,
            },
        )
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

        // Validate trust level for WASM tools before any execution
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

        // Inject secrets
        let secrets = self.inject_secrets(tool_name).await?;

        // Resolve capability-based sandbox and network policies
        let (sandbox_policy, network_policy) = self.resolve_policy(tool_name);

        debug!(
            tool = tool_name,
            sandbox = ?sandbox_policy,
            allowed_domains = ?network_policy.allowed_domains,
            allow_loopback = network_policy.allow_loopback,
            "resolved execution policy"
        );

        // Build tool context
        let ctx = ToolContext {
            session_id: session_id.to_string(),
            user: user.clone(),
            timeout: self.default_timeout,
            cancellation_token: CancellationToken::new(),
            secrets,
            sandbox_policy,
            network_policy,
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

    async fn inject_secrets(
        &self,
        tool_name: &str,
    ) -> anyhow::Result<HashMap<String, SecretValue>> {
        let tool = self.tool_registry.get(tool_name);
        let required = tool.map(|t| t.required_secrets()).unwrap_or_default();
        if required.is_empty() {
            return Ok(HashMap::new());
        }

        let vault_secrets = self
            .secret_vault
            .get_secrets_for_tool(tool_name, &required)
            .await?;

        let mut result = HashMap::new();
        for (k, v) in vault_secrets {
            let s = v
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|_| String::from_utf8_lossy(v.as_bytes()).to_string());
            result.insert(k, SecretValue::new(s));
        }
        Ok(result)
    }
}
