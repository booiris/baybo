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
use baybo_model::TrustLevel;
use parking_lot::Mutex;
use serde_json::{Value, json};

use baybo_llm::{BilledChat, BilledChatResponse};

use crate::{
    ApprovalDecision, ApprovalGate, ApprovalRequest, ExecSandbox, RunningChild, SandboxedOutput,
    SpawnOpts, Tool, ToolContext, ToolManifest, ToolOutput,
};

/// Binds a [`baybo_llm::BillableLlm`] to a throwaway system
/// [`Attribution`](baybo_llm::Attribution) and exposes it as a
/// [`BilledChat`] — bridges tests that drive `LlmCompletion` stubs into
/// the per-call `ctx.lite_llm` slot WebFetch (and any future tool) reads
/// from. Pass a [`BillableLlm::passthrough`](baybo_llm::BillableLlm::passthrough)
/// client so its no-op recorder reports `cost_micros: MicroUsd::ZERO`;
/// budget assertions belong in agent-crate tests.
pub fn unbilled_chat(inner: Arc<baybo_llm::BillableLlm>) -> Arc<dyn BilledChat> {
    struct UnbilledChat {
        inner: baybo_llm::BoundBilledLlm,
    }
    #[async_trait]
    impl BilledChat for UnbilledChat {
        fn model_info(&self) -> &baybo_llm::ModelInfo {
            self.inner.model_info()
        }
        async fn chat(
            &self,
            request: &baybo_llm::ChatRequest,
        ) -> std::result::Result<BilledChatResponse, String> {
            self.inner.chat(request).await.map_err(|e| e.to_string())
        }
    }
    Arc::new(UnbilledChat {
        inner: inner.bind(baybo_llm::Attribution::system("tool-test")),
    })
}

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
            channels: Vec::new(),
            deferred: false,
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> String {
        "Echo tool — returns its serialized parameters.".to_string()
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
    last_turn_id: Arc<Mutex<Option<baybo_model::TurnId>>>,
}

impl RecordingTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            invocations: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(ToolOutput::Text(String::new()))),
            last_turn_id: Arc::new(Mutex::new(None)),
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

    /// The `turn_id` of the most recent `execute()` call — lets a test
    /// reconstruct the per-turn cohort key
    /// (`BackgroundNotificationGroup::cohort_key`) that a
    /// grouped `spawn_subagent` running in that turn would have produced.
    pub fn last_turn_id(&self) -> Option<baybo_model::TurnId> {
        *self.last_turn_id.lock()
    }

    pub fn manifest(&self) -> ToolManifest {
        ToolManifest {
            name: self.name.clone(),
            description: "Recording tool — captures invocation params.".into(),
            trust_level: TrustLevel::Trusted,
            parameters_schema: json!({"type": "object", "additionalProperties": true}),
            capabilities: vec![],
            channels: Vec::new(),
            deferred: false,
        }
    }
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> String {
        "Recording tool — captures invocation params.".to_string()
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "additionalProperties": true})
    }
    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        self.invocations.lock().push(params);
        *self.last_turn_id.lock() = Some(ctx.turn_id);
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
    pub extra_env: Vec<(String, String)>,
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
        opts: SpawnOpts,
    ) -> crate::Result<SandboxedOutput> {
        self.calls.lock().push(FakeSandboxCall {
            program: program.to_path_buf(),
            args: args.to_vec(),
            cwd: opts.cwd.clone(),
            stdin: opts.stdin.clone(),
            extra_env: opts.extra_env.clone(),
            timeout: opts.timeout,
        });
        Ok(self.response.lock().clone())
    }

    /// Spawn the command directly (no real sandbox) and hand back a live child,
    /// so tests can exercise `run_detached`'s completion / background handoff
    /// without bwrap.
    ///
    /// Delegating to the production spawn is the whole point: a hand-rolled
    /// `Command` here would omit `process_group(0)`, and then `start_kill()`
    /// would signal only the direct child. A `sh` that *forks* (dash does; bash
    /// only execs when the script is a single trailing command) would leave the
    /// grandchild alive holding both pipe write ends — the test hangs until the
    /// orphan exits, on the machines where `/bin/sh` is dash and nowhere else.
    /// It would also report no process group, silently skipping the ledger
    /// branch in `BackgroundJobManager::detach_command`.
    async fn spawn_command_detached(
        &self,
        program: &Path,
        args: &[String],
        opts: SpawnOpts,
    ) -> crate::Result<Box<dyn RunningChild>> {
        crate::builtin::bash::spawn_unsandboxed_detached(
            &program.to_string_lossy(),
            args,
            opts.cwd.as_deref(),
            &opts.extra_env,
        )
    }
}

/// `ApprovalGate` that records every request and returns a configurable
/// decision. Test harness for mid-execute approval flows like the
/// Bash-tool unsandboxed-retry path.
pub struct FakeApprovalGate {
    requests: Arc<Mutex<Vec<ApprovalRequest>>>,
    decision: Arc<Mutex<ApprovalDecision>>,
}

impl FakeApprovalGate {
    pub fn new(decision: ApprovalDecision) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            decision: Arc::new(Mutex::new(decision)),
        }
    }

    pub fn set_decision(&self, decision: ApprovalDecision) {
        *self.decision.lock() = decision;
    }

    pub fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests.lock().clone()
    }
}

#[async_trait]
impl ApprovalGate for FakeApprovalGate {
    async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
        self.requests.lock().push(req);
        *self.decision.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChannelType, User};

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "s1".into(),
            user: User {
                id: "u1".into(),
                name: Some("tester".into()),
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            workspace_root: PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            ..ToolContext::for_test()
        }
    }

    #[tokio::test]
    async fn echo_returns_serialized_params() {
        let tool = EchoTool::new("echo");
        let out = tool.execute(json!({"k": "v"}), &ctx()).await.unwrap();
        assert!(matches!(out, ToolOutput::Text(s) if s == "{\"k\":\"v\"}"));
    }

    /// The fake's detached child has to die as a whole tree, or every test that
    /// kills one inherits a stall as long as whatever the shell forked.
    ///
    /// The script is built so the failure is deterministic on any shell rather
    /// than a property of which one `/bin/sh` is. `sleep 47 &` forks — no shell
    /// execs a backgrounded command — and the grandchild inherits both pipe
    /// write ends, so signalling only the direct child leaves them open until
    /// the sleep ends. `echo ready` is the barrier: reading it proves the fork
    /// already happened, without which the kill races the shell and lands
    /// before there is any grandchild to orphan — passing for the wrong reason.
    #[tokio::test]
    async fn detached_fake_child_dies_as_a_whole_process_group() {
        use tokio::io::AsyncReadExt;

        let sandbox = FakeExecSandbox::new();
        let mut child = sandbox
            .spawn_command_detached(
                Path::new("sh"),
                &["-c".into(), "sleep 47 & echo ready; wait".into()],
                SpawnOpts {
                    cwd: None,
                    stdin: None,
                    extra_env: Vec::new(),
                    timeout: Duration::from_secs(1),
                },
            )
            .await
            .expect("fake detached spawn");
        let mut stdout = child.take_stdout().expect("stdout pipe");
        assert!(
            child.process_group_id().is_some(),
            "without a pgid the runtime silently skips the crash-safe reaping ledger"
        );

        let mut ready = [0u8; 6];
        tokio::time::timeout(Duration::from_secs(10), stdout.read_exact(&mut ready))
            .await
            .expect("the shell must reach its barrier")
            .expect("read the readiness marker");
        assert_eq!(&ready, b"ready\n");

        child.start_kill();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_end(&mut rest))
            .await
            .expect("killing the group must close every write end of the pipe")
            .expect("read stdout to EOF");
        child.wait().await;
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
