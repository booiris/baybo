use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use async_trait::async_trait;
use aura_llm::{ChatRequest, LlmCompletion};
use aura_model::{ChatMessage, ContentBlock, ResourceAccess, Role};
use aura_sandbox::{NetworkPolicy, SandboxRunner};
use aura_security::{LeakDetector, PlaceholderMinter, SecretVault};
use aura_tools::{Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::error::CodeBuilderError;
use crate::parse::parse_plan;
use crate::plan::{CallerCaps, EffectivePlan, HardCaps, project};
use crate::prompt::{build_messages, build_retry_messages};
use crate::run::{build_sandbox_spec, execute, truncate_utf8};
use crate::sanitize::rescan_for_llm;
use crate::scratch::RunDir;

/// stdout/stderr larger than this many bytes (after leak-pattern
/// sanitization) is written to a 0600 file under the run directory and
/// the tool result returns only a short preview + path. Keeps the
/// outer agent's context window from getting flooded by long script
/// output.
const INLINE_OUTPUT_THRESHOLD: usize = 4 * 1024;
const PREVIEW_BYTES: usize = 512;

pub struct CodeBuilderTool {
    llm: Arc<dyn LlmCompletion>,
    sandbox_runner: Arc<dyn SandboxRunner>,
    leak_detector: Arc<LeakDetector>,
    minter: Arc<PlaceholderMinter>,
    secret_vault: Arc<SecretVault>,
    uv_path: OnceLock<PathBuf>,
    hard_caps: HardCaps,
}

#[derive(Debug, Deserialize)]
struct Params {
    task: String,
    #[serde(default)]
    max_runtime_seconds: Option<u64>,
    #[serde(default)]
    max_memory_mb: Option<u64>,
    #[serde(default)]
    allow_network: bool,
    #[serde(default)]
    extra_readable_paths: Vec<String>,
}

impl CodeBuilderTool {
    pub fn new(
        llm: Arc<dyn LlmCompletion>,
        sandbox_runner: Arc<dyn SandboxRunner>,
        leak_detector: Arc<LeakDetector>,
        secret_vault: Arc<SecretVault>,
    ) -> Self {
        let minter = Arc::new(PlaceholderMinter::from_master_key(
            secret_vault.master_key(),
        ));
        Self {
            llm,
            sandbox_runner,
            leak_detector,
            minter,
            secret_vault,
            uv_path: OnceLock::new(),
            hard_caps: HardCaps::defaults(),
        }
    }

    fn resolve_uv(&self) -> Result<&Path, CodeBuilderError> {
        if let Some(p) = self.uv_path.get() {
            return Ok(p.as_path());
        }
        let resolved = locate_on_path("uv").ok_or(CodeBuilderError::UvNotFound)?;
        let _ = self.uv_path.set(resolved);
        Ok(self.uv_path.get().expect("just set").as_path())
    }

    async fn fetch_plan(
        &self,
        task: &str,
        caps: &CallerCaps,
    ) -> Result<EffectivePlan, CodeBuilderError> {
        let messages = self.build_safe_messages(task, caps).await?;
        let request = ChatRequest {
            messages: messages.clone(),
            temperature: Some(0.0),
            tools: vec![],
        };
        let first = self.llm.chat(&request).await?;
        let parsed = match parse_plan(&first.content) {
            Ok(p) => p,
            Err(_) => {
                let retry_messages = build_retry_messages(messages, &first.content);
                let retry_request = ChatRequest {
                    messages: retry_messages,
                    temperature: Some(0.0),
                    tools: vec![],
                };
                let second = self.llm.chat(&retry_request).await?;
                parse_plan(&second.content)?
            }
        };
        project(parsed, caps, &self.hard_caps)
    }

    /// Build the planner-LLM message list with every text leaf re-scanned
    /// against `LeakDetector` and any matches re-minted into placeholders.
    /// Closes the boundary breach where `ToolExecutor::reveal_in_value`
    /// hands us plaintext that we'd otherwise forward to a sub-LLM.
    async fn build_safe_messages(
        &self,
        task: &str,
        caps: &CallerCaps,
    ) -> Result<Vec<ChatMessage>, CodeBuilderError> {
        let raw = build_messages(task, caps);
        let mut out = Vec::with_capacity(raw.len());
        for msg in raw {
            let mut content = Vec::with_capacity(msg.content.len());
            for block in msg.content {
                match block {
                    ContentBlock::Text(text) => {
                        let sanitized = rescan_for_llm(
                            &text,
                            &self.leak_detector,
                            &self.minter,
                            &self.secret_vault,
                        )
                        .await?;
                        content.push(ContentBlock::Text(sanitized));
                    }
                    other => content.push(other),
                }
            }
            out.push(ChatMessage {
                role: msg.role,
                content,
            });
        }
        // Belt-and-suspenders: log if any message text still smells like
        // the common credential shapes after rescan, so we surface
        // detector gaps loudly.
        for msg in &out {
            for block in &msg.content {
                if let ContentBlock::Text(t) = block
                    && matches!(msg.role, Role::User)
                {
                    let post = self.leak_detector.scan_text(t);
                    if !post.matches.is_empty() {
                        tracing::warn!(
                            count = post.matches.len(),
                            "rescan left LeakDetector matches in planner prompt"
                        );
                    }
                }
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl Tool for CodeBuilderTool {
    fn name(&self) -> &str {
        "CodeBuilder"
    }

    fn description(&self) -> &str {
        "Generate and execute a one-shot Python program for a given task. \
         Uses an LLM to write code + a permissions plan, runs it under uv \
         in the OS sandbox with strict file/network/CPU/memory caps. The \
         program prints its result to stdout. To consume external data, \
         list the absolute paths in `extra_readable_paths` and have the \
         generated script `open()` them. Caller-set caps and \
         `allow_network` are upper bounds the LLM cannot widen."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Plain-language description of what the Python program should do."
                },
                "max_runtime_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 120,
                    "description": "Caller cap on wall-clock seconds; effective is min(this, LLM-estimated, 120)."
                },
                "max_memory_mb": {
                    "type": "integer",
                    "minimum": 64,
                    "maximum": 1024,
                    "description": "Caller cap on memory in MiB; effective is min(this, LLM-estimated, 1024)."
                },
                "allow_network": {
                    "type": "boolean",
                    "default": false,
                    "description": "Hard-disables network if false; LLM cannot widen this."
                },
                "extra_readable_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Absolute paths the script may read in addition to its scratch dir. The LLM cannot widen this list."
                }
            },
            "required": ["task"]
        })
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        let task = params
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("(missing)");
        let mut hasher = Sha256::new();
        hasher.update(task.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let mut out = vec![ResourceAccess::ExecCommand {
            command: format!("CodeBuilder: {}", &hash[..16]),
        }];
        if params
            .get("allow_network")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            out.push(ResourceAccess::Http { host: "*".into() });
        }
        out
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if p.task.trim().is_empty() {
            return Err(ToolError::InvalidParams("`task` must be non-empty".into()));
        }

        let extra_paths: Vec<PathBuf> = p.extra_readable_paths.iter().map(PathBuf::from).collect();
        let caps = CallerCaps {
            max_runtime_seconds: p.max_runtime_seconds,
            max_memory_mb: p.max_memory_mb,
            allow_network: p.allow_network,
            extra_readable_paths: extra_paths,
        };

        if ctx.cancellation_token.is_cancelled() {
            return Err(CodeBuilderError::Cancelled.into());
        }

        let uv_path = self.resolve_uv().map_err(ToolError::from)?.to_path_buf();

        let plan_start = Instant::now();
        let plan = tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                return Err(CodeBuilderError::Cancelled.into());
            }
            res = self.fetch_plan(&p.task, &caps) => res?,
        };
        let planning_ms = plan_start.elapsed().as_millis() as u64;

        let mut run_dir = RunDir::create(&ctx.workspace_root)?;
        run_dir.write_script(&plan.code)?;

        let spec = build_sandbox_spec(&plan, &run_dir, &uv_path);

        let runner = Arc::clone(&self.sandbox_runner);
        let cancel = ctx.cancellation_token.clone();
        let out = match execute(runner, spec, cancel).await {
            Ok(o) => o,
            Err(CodeBuilderError::RunTimeout(d)) => {
                return Err(ToolError::Timeout(format!("CodeBuilder exceeded {d:?}")));
            }
            Err(e) => return Err(e.into()),
        };
        let execution_ms = out.elapsed.as_millis() as u64;

        let stdout_raw = truncate_utf8(&out.stdout, self.hard_caps.stdout_bytes);
        let stderr_raw = truncate_utf8(&out.stderr, self.hard_caps.stderr_bytes);
        let stdout_safe = self.sanitize_run_output(&stdout_raw).await?;
        let stderr_safe = self.sanitize_run_output(&stderr_raw).await?;

        let stdout_field =
            self.materialise_output(&stdout_safe, &run_dir, run_dir.stdout_path())?;
        let stderr_field =
            self.materialise_output(&stderr_safe, &run_dir, run_dir.stderr_path())?;

        run_dir.keep();

        Ok(ToolOutput::Json(json!({
            "script_path": run_dir.script_path.display().to_string(),
            "exit_code": out.exit_code,
            "stdout": stdout_field,
            "stderr": stderr_field,
            "timed_out": out.timed_out,
            "planning_ms": planning_ms,
            "execution_ms": execution_ms,
            "effective": {
                "wall_clock_seconds": plan.wall_clock_seconds,
                "memory_max_bytes": plan.memory_max_bytes,
                "pids_max": plan.pids_max,
                "network_policy": match plan.network_policy {
                    NetworkPolicy::None => "none",
                    NetworkPolicy::All => "all",
                },
                "readable_paths": plan
                    .readable_paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>(),
            },
            "rationale": plan.rationale,
        })))
    }
}

impl CodeBuilderTool {
    async fn sanitize_run_output(&self, body: &str) -> Result<String, CodeBuilderError> {
        rescan_for_llm(body, &self.leak_detector, &self.minter, &self.secret_vault).await
    }

    /// Decide between inline (short) and on-disk (long) output. The
    /// on-disk path is taken when the sanitised body exceeds
    /// `INLINE_OUTPUT_THRESHOLD`; in that case we write a 0600 file
    /// under the run dir and the JSON value carries a short preview +
    /// the absolute path so the LLM can read more via `Read`.
    fn materialise_output(
        &self,
        body: &str,
        run_dir: &RunDir,
        path: PathBuf,
    ) -> Result<Value, CodeBuilderError> {
        if body.len() <= INLINE_OUTPUT_THRESHOLD {
            return Ok(json!({ "inline": body }));
        }
        run_dir.write_overflow(&path, body)?;
        let mut cut = PREVIEW_BYTES.min(body.len());
        while cut > 0 && !body.is_char_boundary(cut) {
            cut -= 1;
        }
        Ok(json!({
            "path": path.display().to_string(),
            "preview": body[..cut],
            "total_bytes": body.len(),
            "truncated": true,
        }))
    }
}

fn locate_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&candidate)
                    && meta.permissions().mode() & 0o111 != 0
                {
                    return Some(candidate);
                }
            }
            #[cfg(not(unix))]
            {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeSandboxRunner;
    use aura_llm::test_support::StubLlm;
    use aura_llm::{LlmResponse, TokenUsage};
    use aura_model::User;
    use aura_sandbox::SandboxOutput;
    use aura_security::EncryptionKey;
    use aura_storage::test_support::MemorySecretStore;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn make_ctx(workspace_root: PathBuf) -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: aura_model::ChannelType::tui(),
            },
            timeout: Duration::from_secs(30),
            cancellation_token: CancellationToken::new(),
            workspace_root,
            sandbox: None,
        }
    }

    fn empty_output() -> SandboxOutput {
        SandboxOutput {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            elapsed: Duration::from_millis(0),
            timed_out: false,
        }
    }

    fn ok_response(content: &str) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: None,
        }
    }

    fn make_tool(stub: Arc<StubLlm>, runner: Arc<dyn SandboxRunner>) -> CodeBuilderTool {
        let (tool, _vault) = make_tool_with_vault(stub, runner);
        tool
    }

    fn make_tool_with_vault(
        stub: Arc<StubLlm>,
        runner: Arc<dyn SandboxRunner>,
    ) -> (CodeBuilderTool, Arc<SecretVault>) {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let store = Arc::new(MemorySecretStore::new());
        let vault = Arc::new(SecretVault::new(key, store));
        let detector = Arc::new(LeakDetector::with_default_rules());
        let tool = CodeBuilderTool::new(stub, runner, detector, Arc::clone(&vault));
        tool.uv_path
            .set(PathBuf::from("/usr/local/bin/uv"))
            .unwrap();
        (tool, vault)
    }

    #[tokio::test]
    async fn surfaces_non_zero_exit_as_json() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"raise SystemExit(5)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 5,
            stdout: b"".to_vec(),
            stderr: b"oops".to_vec(),
            elapsed: Duration::from_millis(123),
            timed_out: false,
        });
        let runner: Arc<dyn SandboxRunner> = fake.clone();
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let out = tool.execute(json!({"task": "exit 5"}), &ctx).await.unwrap();
        match out {
            ToolOutput::Json(v) => {
                assert_eq!(v["exit_code"], 5);
                assert_eq!(v["timed_out"], false);
                assert_eq!(v["effective"]["network_policy"], "none");
                assert_eq!(v["execution_ms"], 123);
                assert!(v["planning_ms"].is_u64());
            }
            other => panic!("expected JSON, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn result_separates_planning_and_execution_timing() {
        // execution_ms must be the sandbox's reported `elapsed`, NOT
        // wall-clock-since-execute-started (which would include the
        // planning LLM call). Both fields must be present in every
        // successful result.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(250),
            timed_out: false,
        });
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let v = match tool.execute(json!({"task": "x"}), &ctx).await.unwrap() {
            ToolOutput::Json(v) => v,
            _ => panic!(),
        };
        assert_eq!(v["execution_ms"], 250);
        assert!(v["planning_ms"].is_u64(), "planning_ms must be present");
        // No `elapsed_ms` — the ambiguous-name field was renamed.
        assert!(
            v.get("elapsed_ms").is_none(),
            "old `elapsed_ms` field must be gone"
        );
    }

    #[tokio::test]
    async fn timed_out_returns_timeout_error() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"import time; time.sleep(99)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::with_error(aura_sandbox::SandboxError::Timeout(
            Duration::from_secs(30),
        ));
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let err = tool
            .execute(json!({"task": "loop forever"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn passes_correct_argv_to_sandbox() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r##"{"code":"# /// script\n# dependencies = [\"pandas==2.0.0\"]\n# ///\nprint(42)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":2,"estimated_memory_mb":128,"rationale":"x"}"##,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"42\n".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(50),
            timed_out: false,
        });
        let runner: Arc<dyn SandboxRunner> = fake.clone();
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let _ = tool
            .execute(json!({"task": "print 42"}), &ctx)
            .await
            .unwrap();

        let captured = fake.captured();
        let spec = captured.last().expect("at least one spawn");
        assert_eq!(spec.program, PathBuf::from("/usr/bin/env"));
        assert!(spec.args.iter().any(|a| a == "--isolated"));
        assert!(spec.args.iter().any(|a| a == "--no-project"));
        assert!(spec.args.iter().any(|a| a == "--script"));
        assert!(spec.args.iter().any(|a| a.starts_with("UV_CACHE_DIR=")));
    }

    #[tokio::test]
    async fn cancellation_token_pre_llm_returns_immediately() {
        let stub = Arc::new(StubLlm::new());
        // Don't push anything: if execute() reaches the LLM, it will fail
        // with "queue empty"; we want to assert it never gets there.
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        ctx.cancellation_token.cancel();

        let err = tool.execute(json!({"task": "x"}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(s) if s == "cancelled"));
    }

    #[tokio::test]
    async fn rejects_non_json_response_after_retry() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response("definitely not json"));
        stub.push_response(ok_response("still not json"));
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let err = tool.execute(json!({"task": "x"}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn empty_task_rejected() {
        let stub = Arc::new(StubLlm::new());
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let err = tool.execute(json!({"task": "  "}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn accessed_resources_emits_exec_command_synthetic() {
        let stub = Arc::new(StubLlm::new());
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        let resources = tool.accessed_resources(&json!({"task": "compute things"}));
        assert!(matches!(
            resources.first(),
            Some(ResourceAccess::ExecCommand { .. })
        ));
        assert_eq!(resources.len(), 1);

        let with_net = tool.accessed_resources(&json!({"task": "fetch", "allow_network": true}));
        assert_eq!(with_net.len(), 2);
        assert!(matches!(with_net[1], ResourceAccess::Http { .. }));
    }

    #[tokio::test]
    async fn leak_pattern_in_task_is_re_minted_before_llm_sees_it() {
        // Adversarial review #1 regression: ToolExecutor reveals
        // placeholders before invoking the tool, so a previously
        // tokenized secret arrives as plaintext in `task`. The tool
        // must re-mint that plaintext before it reaches its planning
        // LLM. We give the StubLlm a hardcoded valid response and
        // assert that no chat queue entry was scanned with the
        // original AWS key visible — done by spying via the vault:
        // a successful re-mint stores the original under a
        // placeholder, so we can recover it from the vault.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake;
        let (tool, vault) = make_tool_with_vault(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());

        let _ = tool
            .execute(
                json!({"task": "use the key AKIAIOSFODNN7EXAMPLE in the script"}),
                &ctx,
            )
            .await
            .unwrap();

        // The leak detector picks up `AKIA…` keys; rescan_for_llm mints a
        // placeholder and persists the original to the vault. Verify the
        // vault now holds the original keyed under a freshly-minted
        // placeholder — this is what makes the LLM-facing prompt safe.
        let minter = PlaceholderMinter::from_master_key(vault.master_key());
        let placeholder = minter.mint(b"AKIAIOSFODNN7EXAMPLE");
        let stored = vault.get_secret(&placeholder).await.unwrap();
        let stored = stored.expect("placeholder must be persisted to vault");
        let bytes: &[u8] = stored.as_bytes();
        assert_eq!(bytes, b"AKIAIOSFODNN7EXAMPLE");
    }

    #[tokio::test]
    async fn run_dir_keeps_script_and_returns_path_not_code() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"1\n".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(20),
            timed_out: false,
        });
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let out = tool
            .execute(json!({"task": "print 1"}), &ctx)
            .await
            .unwrap();

        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected JSON, got {other:?}"),
        };
        assert!(v.get("code").is_none(), "code must not be inlined");
        let script_path = v["script_path"]
            .as_str()
            .expect("script_path must be a string");
        let script_path = PathBuf::from(script_path);
        assert!(script_path.exists(), "script.py must persist past Drop");
        assert_eq!(std::fs::read_to_string(&script_path).unwrap(), "print(1)");

        // Ephemeral subdirs trimmed.
        let run_root = script_path.parent().unwrap();
        assert!(!run_root.join("uv-cache").exists());
        assert!(!run_root.join("workdir").exists());
        assert!(!run_root.join("inputs.json").exists());
    }

    #[tokio::test]
    async fn short_stdout_is_inlined() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(20),
            timed_out: false,
        });
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let v = match tool.execute(json!({"task": "x"}), &ctx).await.unwrap() {
            ToolOutput::Json(v) => v,
            _ => panic!(),
        };
        assert_eq!(v["stdout"]["inline"], "hello\n");
        assert!(v["stdout"].get("path").is_none());
    }

    #[tokio::test]
    async fn long_stdout_is_written_to_file_with_preview_and_path() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let big = "A".repeat(8 * 1024); // > 4 KiB inline threshold
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: big.as_bytes().to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(20),
            timed_out: false,
        });
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let v = match tool.execute(json!({"task": "x"}), &ctx).await.unwrap() {
            ToolOutput::Json(v) => v,
            _ => panic!(),
        };
        assert!(v["stdout"].get("inline").is_none());
        let path = PathBuf::from(v["stdout"]["path"].as_str().unwrap());
        assert!(path.exists(), "stdout overflow file must exist");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, big);
        assert_eq!(v["stdout"]["total_bytes"], 8 * 1024);
        assert!(
            v["stdout"]["preview"].as_str().unwrap().len() <= 512,
            "preview must be bounded"
        );
    }

    #[tokio::test]
    async fn stdout_secrets_are_re_minted_before_writing_to_file() {
        // Long stdout containing an AWS key. The on-disk file must
        // hold the placeholder, NEVER the plaintext key, because the
        // outer SecurityGateway::sanitize_tool_output only walks the
        // ToolOutput JSON value — it can't see file bodies.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let mut body = "X".repeat(8 * 1024);
        body.push_str(" AKIAIOSFODNN7EXAMPLE\n");
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: body.as_bytes().to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(20),
            timed_out: false,
        });
        let (tool, vault) = make_tool_with_vault(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let v = match tool.execute(json!({"task": "x"}), &ctx).await.unwrap() {
            ToolOutput::Json(v) => v,
            _ => panic!(),
        };
        let path = PathBuf::from(v["stdout"]["path"].as_str().unwrap());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("AKIAIOSFODNN7EXAMPLE"),
            "plaintext leak in stdout overflow file"
        );

        let minter = PlaceholderMinter::from_master_key(vault.master_key());
        let placeholder = minter.mint(b"AKIAIOSFODNN7EXAMPLE");
        assert!(
            on_disk.contains(&placeholder),
            "expected placeholder in on-disk stdout"
        );
        let stored = vault.get_secret(&placeholder).await.unwrap().unwrap();
        let bytes: &[u8] = stored.as_bytes();
        assert_eq!(bytes, b"AKIAIOSFODNN7EXAMPLE");
    }

    #[tokio::test]
    async fn run_dir_removed_on_failure_path() {
        // If execute fails (e.g. timeout) before keep() is called,
        // the run dir must Drop the whole tree, not leave it behind.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::with_error(aura_sandbox::SandboxError::Timeout(
            Duration::from_secs(30),
        ));
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let _ = tool.execute(json!({"task": "x"}), &ctx).await.unwrap_err();

        let runs_root = tmp.path().join(".aura").join("code-builder").join("runs");
        if runs_root.exists() {
            let count = std::fs::read_dir(&runs_root).unwrap().count();
            assert_eq!(count, 0, "failed run must be cleaned up");
        }
    }
}
