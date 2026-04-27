//! Test stub `Tool` impls for downstream tests.
//!
//! `EchoTool` returns the JSON-stringified params as text — useful when
//! a test only needs the tool path to fire and produce deterministic
//! output. `RecordingTool` captures every call's parameters and returns
//! a configurable response, enabling assertions on what the agent
//! actually handed to the tool (critical for tool-arg reveal/sanitize
//! boundary tests).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_model::TrustLevel;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{ExecSandbox, SandboxedOutput, Tool, ToolContext, ToolManifest, ToolOutput};

/// `Tool` that returns its serialized parameters as text. Trust level
/// is `Trusted` by default so the tool isn't blocked by approval gates
/// in integration tests; override the manifest if a test needs to
/// exercise approval flow.
pub struct EchoTool {
    name: String,
}

impl EchoTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Returns a manifest that pairs with this tool.
    pub fn manifest(&self) -> ToolManifest {
        ToolManifest {
            name: self.name.clone(),
            description: "Echo tool — returns its serialized parameters.".into(),
            trust_level: TrustLevel::Trusted,
            parameters_schema: json!({"type": "object", "additionalProperties": true}),
            capabilities: vec![],
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Echo tool — returns its serialized parameters."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "additionalProperties": true})
    }
    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        Ok(ToolOutput::Text(params.to_string()))
    }
}

/// `Tool` that records every invocation and returns a configurable
/// response. The recording captures the *post-reveal* parameters (i.e.
/// what the tool actually receives), which is the data tests need to
/// verify the reveal boundary in `ToolExecutor::execute`.
#[derive(Clone)]
pub struct RecordingTool {
    name: String,
    invocations: Arc<Mutex<Vec<Value>>>,
    response: Arc<Mutex<ToolOutput>>,
}

impl RecordingTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            invocations: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(ToolOutput::Text(String::new()))),
        }
    }

    /// Override what the next (and any subsequent) `execute()` call
    /// returns. Idempotent — keeps the latest setting.
    pub fn set_response(&self, output: ToolOutput) {
        *self.response.lock() = output;
    }

    /// All `params` values seen so far, in arrival order.
    pub fn invocations(&self) -> Vec<Value> {
        self.invocations.lock().to_vec()
    }

    pub fn manifest(&self) -> ToolManifest {
        ToolManifest {
            name: self.name.clone(),
            description: "Recording tool — captures invocation params.".into(),
            trust_level: TrustLevel::Trusted,
            parameters_schema: json!({"type": "object", "additionalProperties": true}),
            capabilities: vec![],
        }
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Recording tool — captures invocation params."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "additionalProperties": true})
    }
    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        self.invocations.lock().push(params);
        Ok(self.response.lock().clone())
    }
}

/// `ExecSandbox` that records calls and returns canned output. Used by
/// downstream tests that want to exercise tools declaring `ExecCommand`
/// capability without requiring a real backend binary on the host.
#[derive(Clone, Default)]
pub struct FakeExecSandbox {
    calls: Arc<Mutex<Vec<FakeSandboxCall>>>,
    response: Arc<Mutex<SandboxedOutput>>,
}

#[derive(Debug, Clone)]
pub struct FakeSandboxCall {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl FakeExecSandbox {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(SandboxedOutput {
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: false,
            })),
        }
    }

    pub fn set_response(&self, output: SandboxedOutput) {
        *self.response.lock() = output;
    }

    pub fn calls(&self) -> Vec<FakeSandboxCall> {
        self.calls.lock().clone()
    }
}

#[async_trait]
impl ExecSandbox for FakeExecSandbox {
    async fn spawn_command(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> crate::Result<SandboxedOutput> {
        self.calls.lock().push(FakeSandboxCall {
            program: program.to_path_buf(),
            args: args.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
            stdin: stdin.map(<[u8]>::to_vec),
            timeout,
        });
        Ok(self.response.lock().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, User};

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            user: User {
                id: "u1".into(),
                name: Some("tester".into()),
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            workspace_root: PathBuf::from("/tmp"),
            sandbox: None,
            approval: None,
            subagent: None,
            parent_job_id: None,
        }
    }

    #[tokio::test]
    async fn echo_returns_serialized_params() {
        let tool = EchoTool::new("echo");
        let out = tool.execute(json!({"k": "v"}), &ctx()).await.unwrap();
        assert!(matches!(out, ToolOutput::Text(s) if s == "{\"k\":\"v\"}"));
    }

    #[tokio::test]
    async fn recording_captures_and_returns_configured() {
        let tool = RecordingTool::new("rec");
        tool.set_response(ToolOutput::Text("done".into()));
        let _ = tool.execute(json!({"a": 1}), &ctx()).await.unwrap();
        let _ = tool.execute(json!({"a": 2}), &ctx()).await.unwrap();
        let calls = tool.invocations();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], json!({"a": 1}));
        assert_eq!(calls[1], json!({"a": 2}));
    }
}
