pub mod approval;
pub mod builtin;
pub mod error;
pub mod mcp;
pub mod registry;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_model::User;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use approval::{
    ApprovalDecision, ApprovalGate, ApprovalGateMap, ApprovalQueue, ApprovalRequest,
    ApprovedResource, AutoDenyGate, ChannelApprovalGate, HostPattern, ResourceAccess,
};
pub use error::ToolError;

pub type Result<T> = std::result::Result<T, ToolError>;

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

/// Tool trait — the unified interface for all tool implementations.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
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

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput>;
}

/// Context injected into tool execution by the agent layer.
pub struct ToolContext {
    pub session_id: String,
    pub user: User,
    pub timeout: Duration,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub workspace_root: PathBuf,
    pub sandbox: Option<Arc<dyn ExecSandbox>>,
    /// Mid-execution approval handle. Tools that decide which resources
    /// they will touch only after some internal work (e.g. CodeBuilder
    /// runs an LLM to draft the program before knowing what files it
    /// will read or whether it needs network) prompt the user through
    /// this handle. `None` means the executor did not wire one in;
    /// callers must fail-closed.
    pub approval: Option<ApprovalHandle>,
    /// Sub-agent spawn handle. The `Agent` tool delegates to this to
    /// run an isolated child agent loop in a freshly-minted session
    /// (`Session.parent_link.kind = SubAgent`) and synchronously wait
    /// for its terminal output. `None` means the executor did not
    /// wire one in; the tool returns `NotImplemented` in that case.
    /// Defined as a trait + injected handle (rather than holding the
    /// `SessionManager` / `JobManager` directly) so `aura-tools`
    /// doesn't need to take a dep on the agent crate.
    pub subagent: Option<Arc<dyn SubAgentSpawner>>,
    /// Id of the parent job that owns this tool call, if any. Sub-
    /// agent dispatch threads it through to set the child Job's
    /// `parent_job_id` so the parent/child relationship survives
    /// crash recovery.
    pub parent_job_id: Option<String>,
}

/// Capability injected into [`ToolContext`] so the `Agent` tool can
/// spawn an isolated child agent loop in its own session and block
/// for the result. Implemented by the agent crate.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    /// Spawn a sub-agent for `prompt` under the given parent
    /// (`parent_session_id`, `parent_job_id`) and synchronously wait
    /// until the child reaches a terminal state.
    ///
    /// Returns the child's final response text on success. Errors
    /// surface back to the parent agent loop as a tool failure.
    async fn spawn(
        &self,
        prompt: String,
        parent_session_id: &str,
        parent_job_id: Option<&str>,
        user: &User,
    ) -> Result<SubAgentOutput>;
}

/// Result of [`SubAgentSpawner::spawn`].
#[derive(Debug, Clone)]
pub struct SubAgentOutput {
    /// Id of the freshly-minted child session.
    pub child_session_id: String,
    /// Id of the child Job dispatched into that session.
    pub child_job_id: String,
    /// Final reply text the child produced.
    pub final_text: String,
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
        session_id: &str,
        user: &User,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
    ) -> ApprovalDecision {
        let req = ApprovalRequest {
            call_id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
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
        session_id: &str,
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
            session_id: session_id.to_string(),
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
        cwd: Option<&Path>,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> crate::Result<SandboxedOutput>;
}

#[derive(Debug, Clone, Default)]
pub struct SandboxedOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
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
