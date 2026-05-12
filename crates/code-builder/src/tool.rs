use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aura_llm::{ChatRequest, GuardedLlm};
use aura_model::{ChatMessage, ContentBlock, ResourceAccess, Role};
use aura_sandbox::{NetworkPolicy, SandboxRunner};
use aura_security::{LeakDetector, PlaceholderMinter, SecretVault};
use aura_tools::{ApprovalDecision, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::CodeBuilderError;
use crate::parse::parse_plan;
use crate::plan::{CallerCaps, EffectivePlan, HardCaps, WritableIntent, WritableKind, project};
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
    llm: Arc<GuardedLlm>,
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
    #[serde(default)]
    extra_writable_paths: Vec<String>,
}

impl CodeBuilderTool {
    pub fn new(
        llm: Arc<GuardedLlm>,
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
                from_user: false,
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
                },
                "extra_writable_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Absolute paths outside the script's CWD the script may write to. Each entry the LLM declares (intersected with this list) triggers a per-path approval prompt before the sandbox starts. The LLM cannot widen this list."
                }
            },
            "required": ["task"]
        })
    }

    fn max_timeout(&self) -> Duration {
        // The sandboxed program is hard-capped at
        // `HardCaps::wall_clock_seconds` (120s); add headroom for the
        // planner LLM round-trip + uv setup + per-path approval
        // prompts. The executor's APPROVAL_HEADROOM is layered on top
        // of this for the mid-execution gate.
        Duration::from_secs(180)
    }

    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        // Approval for CodeBuilder happens AFTER the planning LLM has
        // drafted the program, in `execute()` below — by then the gate
        // can show real resources (concrete network host with the
        // LLM's reason, real out-of-scratch write paths) rather than
        // an opaque task hash. Returning empty here intentionally
        // skips the executor's pre-execute gate.
        Vec::new()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let params_for_record = params.clone();
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if p.task.trim().is_empty() {
            return Err(ToolError::InvalidParams("`task` must be non-empty".into()));
        }

        let extra_paths: Vec<PathBuf> = p.extra_readable_paths.iter().map(PathBuf::from).collect();
        let extra_write_paths: Vec<PathBuf> =
            p.extra_writable_paths.iter().map(PathBuf::from).collect();
        let caps = CallerCaps {
            max_runtime_seconds: p.max_runtime_seconds,
            max_memory_mb: p.max_memory_mb,
            allow_network: p.allow_network,
            extra_readable_paths: extra_paths,
            extra_writable_paths: extra_write_paths,
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

        // Post-plan approval gate. By now we know which network host
        // (if any) the script wants to reach with what reason, and
        // which out-of-scratch paths it wants to write to. Surface
        // those concrete resources to the user; deny short-circuits
        // before the sandbox starts and before we persist `script.py`.
        // The pre-execute gate in `tool_executor` is intentionally a
        // no-op for CodeBuilder (`accessed_resources` returns empty).
        let raw_accesses = build_approval_accesses(&plan, &run_dir.root);
        if !raw_accesses.is_empty() {
            // Re-mint anything string-shaped against the leak detector
            // before it reaches the gate. ToolExecutor::reveal_in_value
            // unmasks placeholders before we get the params, so `p.task`
            // and any caller-supplied write path can carry plaintext
            // secrets at this point. `build_safe_messages` (above)
            // does the same for the planning LLM call; without this,
            // a credential-bearing task whose plan needs the network
            // or an out-of-scratch write would leak through the
            // approval prompt to the channel UI.
            let approval_accesses = self.sanitize_accesses(raw_accesses).await?;
            let preview = self.sanitize_preview(&p.task, &plan).await?;
            let decision = match &ctx.approval {
                Some(handle) => {
                    handle
                        .request(
                            "CodeBuilder",
                            &ctx.session_id,
                            &ctx.user,
                            approval_accesses,
                            preview,
                        )
                        .await
                }
                // No gate wired in: fail-closed. Production builds
                // always inject one via `ToolExecutor`; only ad-hoc
                // test harnesses that bypass the agent layer hit this.
                None => ApprovalDecision::Deny,
            };
            if decision == ApprovalDecision::Deny {
                return Err(ToolError::Denied {
                    tool: "CodeBuilder".into(),
                    reason: "user denied approval".into(),
                });
            }
        }

        // Approval landed (or wasn't needed). Resolve the bind targets:
        // for each LLM-declared writable path, ensure the immediate
        // parent directory exists (mkdir -p), then re-validate that
        // the canonical parent is still inside one of the caller's
        // canonical writable dirs. This step is **post-approval** by
        // design — the user agreed to the LLM's specific declared
        // path; the bind is the narrowest dir we can realistically
        // mount (the parent of the file or the path itself when the
        // LLM declared a directory equal to a caller grant).
        let writable_bind_targets =
            resolve_writable_bind_targets(&plan.writable_paths, &plan.canonical_caller_writes)?;

        run_dir.write_script(&plan.code)?;

        let spec = build_sandbox_spec(&plan, &run_dir, &uv_path, &writable_bind_targets);

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

        let output_value = json!({
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
        });

        if let Err(e) = self
            .persist_tool_call_record(&run_dir, &params_for_record, &output_value)
            .await
        {
            tracing::warn!(error = %e, "failed to persist tool_call.json record");
        }

        Ok(ToolOutput::Json(output_value))
    }
}

impl CodeBuilderTool {
    async fn sanitize_run_output(&self, body: &str) -> Result<String, CodeBuilderError> {
        rescan_for_llm(body, &self.leak_detector, &self.minter, &self.secret_vault).await
    }

    /// Re-mint any leak-pattern-shaped plaintext in a single text leaf
    /// using the same boundary as `build_safe_messages`. Used for the
    /// `params_preview` of an approval request and for path strings in
    /// the access list, both of which travel out to channel UIs.
    async fn sanitize_for_approval(&self, body: &str) -> Result<String, CodeBuilderError> {
        rescan_for_llm(body, &self.leak_detector, &self.minter, &self.secret_vault).await
    }

    /// Build the `params_preview` shown to the user, with `task`,
    /// `rationale`, and `network_reason` each rescanned for placeholder
    /// re-minting. Plaintext secrets that survived
    /// `ToolExecutor::reveal_in_value` cannot escape through the
    /// approval channel.
    async fn sanitize_preview(
        &self,
        task: &str,
        plan: &EffectivePlan,
    ) -> Result<String, CodeBuilderError> {
        let task_safe = self.sanitize_for_approval(task).await?;
        let rationale_safe = self.sanitize_for_approval(&plan.rationale).await?;
        let network_reason_safe = match &plan.network_reason {
            Some(r) => Some(self.sanitize_for_approval(r).await?),
            None => None,
        };
        let mut paths_safe: Vec<String> = Vec::with_capacity(plan.writable_paths.len());
        for intent in &plan.writable_paths {
            // Display the dir-intent flavour with a trailing slash so
            // the user sees in the prompt that the LLM intends to
            // write *inside* that dir versus replacing a single file.
            let display = match intent.kind {
                crate::plan::WritableKind::Dir => format!("{}/", intent.path.display()),
                crate::plan::WritableKind::File => intent.path.display().to_string(),
            };
            paths_safe.push(self.sanitize_for_approval(&display).await?);
        }
        let mut sanitized_plan = plan.clone();
        sanitized_plan.rationale = rationale_safe;
        sanitized_plan.network_reason = network_reason_safe;
        Ok(approval_preview(&task_safe, &sanitized_plan, &paths_safe))
    }

    /// Re-mint any path strings carried inside the approval access list
    /// (`WriteFile { path }`). `Http { host: "*" }` has no caller-
    /// supplied content so it passes through untouched.
    async fn sanitize_accesses(
        &self,
        accesses: Vec<ResourceAccess>,
    ) -> Result<Vec<ResourceAccess>, CodeBuilderError> {
        let mut out = Vec::with_capacity(accesses.len());
        for acc in accesses {
            match acc {
                ResourceAccess::WriteFile { path } => {
                    let s = path.to_string_lossy();
                    let safe = self.sanitize_for_approval(&s).await?;
                    out.push(ResourceAccess::WriteFile {
                        path: PathBuf::from(safe),
                    });
                }
                other => out.push(other),
            }
        }
        Ok(out)
    }

    async fn persist_tool_call_record(
        &self,
        run_dir: &RunDir,
        input: &Value,
        output: &Value,
    ) -> Result<(), CodeBuilderError> {
        let record = json!({
            "input": input,
            "output": output,
        });
        let serialized = serde_json::to_string_pretty(&record)
            .map_err(|e| CodeBuilderError::Scratch(format!("serialise tool_call record: {e}")))?;
        let sanitised = self.sanitize_run_output(&serialized).await?;
        run_dir.write_overflow(&run_dir.tool_call_path(), &sanitised)
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

/// Take the LLM's declared writable intents and produce the actual
/// host-side bind targets the sandbox should mount RW.
///
/// The bind target depends on the intent's kind (which encodes the
/// LLM's trailing-slash declaration):
///
/// - Intent kind `Dir` (`/foo/bar/`): bind the dir itself, `mkdir -p`'d
///   if missing. Siblings of the dir on the host stay unmapped.
/// - Intent kind `File` (`/foo/bar/file`): bind the **immediate
///   parent** of the file, `mkdir -p`'d if missing. The script can
///   `open()` and write the declared file (and, transitively, anything
///   else inside the parent — that's the cost of file-granularity
///   binding without `--bind file file` support, which would require
///   pre-touching empty files in every backend).
///
/// Either way, when the intent equals exactly one of the caller's
/// allowlisted dirs, we bind that dir straight through (no parent
/// climb) — the caller has explicitly approved the whole tree.
///
/// The chosen bind target is then `mkdir -p`'d, canonicalized, and
/// re-validated against `canonical_caller_writes` as a TOCTOU defense:
/// a symlink planted between the projection's lexical containment
/// check and this point would surface here and the call refuses.
///
/// Bind targets are deduplicated so multiple intents in the same
/// parent (or under the same dir) produce one mount entry.
fn resolve_writable_bind_targets(
    intents: &[WritableIntent],
    canonical_caller_writes: &[PathBuf],
) -> Result<Vec<PathBuf>, CodeBuilderError> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(intents.len());
    for intent in intents {
        // Bind dir is determined by the LLM's kind plus a special
        // short-circuit when intent equals a caller grant.
        //
        // Dir kind → bind = intent (mkdir as a directory).
        // File kind → bind = intent.parent() (mkdir parent), so
        //   `open(intent, 'w')` lands on a regular file under a real
        //   host directory. Note: this widens the bind dir to the
        //   parent — siblings of the file are also writable inside
        //   the sandbox. The caller accepted that scope by passing a
        //   writable allowlist that contains the file (or its
        //   parent).
        let bind_candidate = match intent.kind {
            WritableKind::Dir => intent.path.clone(),
            WritableKind::File => intent
                .path
                .parent()
                .ok_or_else(|| {
                    CodeBuilderError::LlmPlanRejected(format!(
                        "writable_paths entry has no parent directory: {:?}",
                        intent.path
                    ))
                })?
                .to_path_buf(),
        };

        // Idempotently ensure the bind target exists on the host.
        // `create_dir_all` is a no-op when it already exists; if a
        // non-directory file sits at that path we surface the error
        // with the offending path so the LLM/caller can fix the plan.
        std::fs::create_dir_all(&bind_candidate).map_err(|e| {
            CodeBuilderError::Scratch(format!(
                "create writable bind target {bind_candidate:?}: {e}"
            ))
        })?;

        // TOCTOU defense: canonicalize the dir we're about to mount
        // and verify it sits in the same path lineage as the caller's
        // grant. The bind may be:
        //   - **inside or equal to** a caller canonical (typical: the
        //     caller granted a directory and the bind is that dir or a
        //     subdir) — `canonical_bind.starts_with(c)`
        //   - **a parent of** a caller canonical (typical: the caller
        //     granted a file slot like `/foo/bar/baz` and the bind is
        //     the parent dir `/foo/bar`) — `c.starts_with(canonical_bind)`
        // A symlink planted between projection and `mkdir` that
        // redirected the bind elsewhere would fail both checks.
        let canonical_bind = std::fs::canonicalize(&bind_candidate).map_err(|e| {
            CodeBuilderError::Scratch(format!(
                "canonicalize writable bind target {bind_candidate:?}: {e}"
            ))
        })?;
        let still_in_allowlist = canonical_caller_writes.iter().any(|c| {
            canonical_bind == *c || canonical_bind.starts_with(c) || c.starts_with(&canonical_bind)
        });
        if !still_in_allowlist {
            return Err(CodeBuilderError::LlmPlanRejected(format!(
                "writable bind target {canonical_bind:?} resolves outside extra_writable_paths"
            )));
        }

        if !out.contains(&canonical_bind) {
            out.push(canonical_bind);
        }
    }
    Ok(out)
}

/// Build the approval list shown to the user before the sandbox starts.
/// Each consent-worthy decision in `EffectivePlan` becomes its own
/// `ResourceAccess` line in the prompt:
///
/// - `Http { host: "*" }` when the program needs the network. The
///   LLM-supplied `network_reason` is rendered separately in the
///   `params_preview` so the user can see *why* network is needed.
/// - `WriteFile { path }` for every writable intent that resolves
///   *outside* the per-run scratch root. The scratch root is the only
///   tree the sandbox RW-binds by default (`SandboxSpec.workspace_root`
///   is set to `scratch.root` in `crate::run::build_sandbox_spec`), so
///   paths under it are auto-writable and need no consent. Anything
///   else — including paths that merely sit under the agent's host
///   workspace dir but outside the per-run scratch — must be approved.
fn build_approval_accesses(plan: &EffectivePlan, scratch_root: &Path) -> Vec<ResourceAccess> {
    let mut accesses: Vec<ResourceAccess> = Vec::new();
    if plan.network_policy == NetworkPolicy::All {
        accesses.push(ResourceAccess::Http { host: "*".into() });
    }
    // `scratch_root` was just `mkdir`'d by `RunDir::create`, so
    // canonicalize basically always succeeds. We keep the literal
    // form for the lexical-fallback branch only as a defence against
    // pathological cases (filesystem error, race on cleanup) — not
    // as a way to widen "inside" to the agent's host workspace.
    let canonical_scratch = std::fs::canonicalize(scratch_root).ok();
    for intent in &plan.writable_paths {
        let p = &intent.path;
        let canon = std::fs::canonicalize(p).ok();
        let inside = match (&canonical_scratch, &canon) {
            (Some(root), Some(c)) => c.starts_with(root),
            // Intent path doesn't exist yet (typical — script will
            // create it): compare lexically against both the literal
            // `scratch_root` and its canonical form. The LLM intent
            // has already been lexically validated as clean
            // (no `..`/`.`) by `plan::project`, so `starts_with` is
            // sound here.
            (Some(canon_root), None) => p.starts_with(scratch_root) || p.starts_with(canon_root),
            // canonicalize failed — fall back to a pure lexical check
            // against `scratch_root` only. NEVER fall back to a wider
            // path (the previous "or workspace_root" was a
            // mis-feature: workspace_root is the agent host root, not
            // the per-call writable scope, and treating "inside
            // workspace_root" as auto-RW lets non-existent intent
            // paths sneak past approval).
            (None, _) => p.starts_with(scratch_root),
        };
        if !inside {
            accesses.push(ResourceAccess::WriteFile { path: p.clone() });
        }
    }
    accesses
}

/// Render the `params_preview` shown alongside the approval list. The
/// access list above already names every resource the script will
/// touch, and the LLM-authored task/rationale are noisy duplicates of
/// the assistant turn that triggered the call — so they are dropped on
/// purpose. The only information preserved is `network_reason` when
/// the plan requires network, since `ResourceAccess::Http` carries no
/// reason field of its own. Returns an empty string for the common
/// write-only case so the channel UI skips the preview block entirely.
/// Caller is responsible for rescanning every string-shaped argument
/// against the leak detector (see `sanitize_preview`).
fn approval_preview(_task: &str, plan: &EffectivePlan, _writable_paths: &[String]) -> String {
    const REASON_PREVIEW_BYTES: usize = 240;

    if plan.network_policy != NetworkPolicy::All {
        return String::new();
    }
    let Some(reason) = plan.network_reason.as_deref() else {
        return String::new();
    };
    let reason = reason.trim();
    if reason.is_empty() {
        return String::new();
    }
    format!(
        "network reason: {}",
        truncate_for_preview(reason, REASON_PREVIEW_BYTES)
    )
}

fn truncate_for_preview(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
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
            workspace_paths: aura_workspace::WorkspacePaths::new(workspace_root.clone()),
            workspace_root,
            sandbox: None,
            approval: None,
            notifier: None,
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
        let llm = GuardedLlm::passthrough(stub as Arc<dyn aura_llm::LlmCompletion>);
        let tool = CodeBuilderTool::new(llm, runner, detector, Arc::clone(&vault));
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
    fn accessed_resources_returns_empty() {
        // The pre-execute approval gate is intentionally a no-op for
        // CodeBuilder. Real consent happens after the planning LLM
        // call, in `execute()`, where the prompt can show concrete
        // network host + reason and concrete out-of-scratch write
        // paths. Returning empty here is what makes the executor's
        // pre-execute gate skip CodeBuilder.
        let stub = Arc::new(StubLlm::new());
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake;
        let tool = make_tool(stub, runner);

        assert!(
            tool.accessed_resources(&json!({"task": "compute things"}))
                .is_empty()
        );
        assert!(
            tool.accessed_resources(&json!({"task": "fetch", "allow_network": true}))
                .is_empty()
        );
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
        assert!(
            !run_root
                .join(aura_workspace::paths::CODE_BUILDER_UV_CACHE_SUBDIR)
                .exists()
        );
        assert!(
            !run_root
                .join(aura_workspace::paths::CODE_BUILDER_WORKDIR_SUBDIR)
                .exists()
        );
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

        let runs_root = tmp.path().join(aura_workspace::paths::CODE_BUILDER_SUBDIR);
        if runs_root.exists() {
            let count = std::fs::read_dir(&runs_root).unwrap().count();
            assert_eq!(count, 0, "failed run must be cleaned up");
        }
    }

    #[tokio::test]
    async fn tool_call_record_persisted_alongside_script() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(7)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"7\n".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(11),
            timed_out: false,
        });
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path().to_path_buf());
        let v = match tool
            .execute(json!({"task": "print 7"}), &ctx)
            .await
            .unwrap()
        {
            ToolOutput::Json(v) => v,
            other => panic!("expected JSON, got {other:?}"),
        };

        let script_path = PathBuf::from(v["script_path"].as_str().unwrap());
        let record_path = script_path.parent().unwrap().join("tool_call.json");
        assert!(
            record_path.exists(),
            "tool_call.json must sit next to script.py"
        );

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&record_path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "tool_call.json must be 0600");

        let body = std::fs::read_to_string(&record_path).unwrap();
        let parsed: Value = serde_json::from_str(&body).expect("tool_call.json must be valid JSON");
        assert_eq!(parsed["input"]["task"], "print 7");
        assert_eq!(parsed["output"]["exit_code"], 0);
        assert_eq!(parsed["output"]["stdout"]["inline"], "7\n");
    }

    // ---------------------------------------------------------------
    // Approval gate flow: post-plan, pre-sandbox prompt
    // ---------------------------------------------------------------

    use aura_tools::{
        ApprovalDecision, ApprovalGate, ApprovalHandle, ApprovalRequest, ApprovedResource,
    };
    use parking_lot::Mutex as PlMutex;

    /// Fake gate that captures every request and returns a configured
    /// decision. Captured requests let tests assert on the rendered
    /// resources and `params_preview`.
    struct StubGate {
        decision: ApprovalDecision,
        captured: Arc<PlMutex<Vec<ApprovalRequest>>>,
    }

    impl StubGate {
        fn new(decision: ApprovalDecision) -> (Arc<Self>, Arc<PlMutex<Vec<ApprovalRequest>>>) {
            let captured = Arc::new(PlMutex::new(Vec::new()));
            let gate = Arc::new(Self {
                decision,
                captured: Arc::clone(&captured),
            });
            (gate, captured)
        }
    }

    #[async_trait]
    impl ApprovalGate for StubGate {
        async fn request(&self, req: ApprovalRequest) -> ApprovalDecision {
            self.captured.lock().push(req);
            self.decision
        }
    }

    type CapturedRequests = Arc<PlMutex<Vec<ApprovalRequest>>>;
    type ApprovalCache = Arc<PlMutex<Vec<ApprovedResource>>>;

    fn handle_with(
        decision: ApprovalDecision,
    ) -> (ApprovalHandle, CapturedRequests, ApprovalCache) {
        handle_with_cache(decision, Arc::new(PlMutex::new(Vec::new())))
    }

    fn handle_with_cache(
        decision: ApprovalDecision,
        cache: ApprovalCache,
    ) -> (ApprovalHandle, CapturedRequests, ApprovalCache) {
        let (gate, captured) = StubGate::new(decision);
        let handle = ApprovalHandle::new(gate as Arc<dyn ApprovalGate>, Arc::clone(&cache));
        (handle, captured, cache)
    }

    fn make_ctx_with_handle(workspace_root: PathBuf, handle: ApprovalHandle) -> ToolContext {
        ToolContext {
            session_id: "test".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: aura_model::ChannelType::tui(),
            },
            timeout: Duration::from_secs(30),
            cancellation_token: CancellationToken::new(),
            workspace_paths: aura_workspace::WorkspacePaths::new(workspace_root.clone()),
            workspace_root,
            sandbox: None,
            approval: Some(handle),
            notifier: None,
        }
    }

    #[tokio::test]
    async fn no_approval_when_plan_has_no_consent_worthy_resources() {
        // Plain plan: no network, no out-of-scratch writes. The gate
        // must not be hit. Run with a deny-all gate to prove it.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":false,"readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"x"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(20),
            timed_out: false,
        });
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let (handle, captured, _) = handle_with(ApprovalDecision::Deny);
        let ctx = make_ctx_with_handle(tmp.path().to_path_buf(), handle);
        let _ = tool.execute(json!({"task": "x"}), &ctx).await.unwrap();
        assert!(
            captured.lock().is_empty(),
            "gate must not be invoked for trivial plans"
        );
    }

    #[tokio::test]
    async fn network_required_prompts_with_reason_in_preview() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":true,"network_reason":"call api.example.com for inventory","readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}"#,
        ));
        let fake = FakeSandboxRunner::new(SandboxOutput {
            exit_code: 0,
            stdout: b"".to_vec(),
            stderr: b"".to_vec(),
            elapsed: Duration::from_millis(20),
            timed_out: false,
        });
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let (handle, captured, _) = handle_with(ApprovalDecision::Approve);
        let ctx = make_ctx_with_handle(tmp.path().to_path_buf(), handle);
        let _ = tool
            .execute(
                json!({"task": "fetch inventory", "allow_network": true}),
                &ctx,
            )
            .await
            .unwrap();

        let reqs = captured.lock();
        assert_eq!(reqs.len(), 1, "exactly one approval request expected");
        let req = &reqs[0];
        assert_eq!(req.tool, "CodeBuilder");
        let host_count = req
            .accesses
            .iter()
            .filter(|a| matches!(a, ResourceAccess::Http { .. }))
            .count();
        assert_eq!(host_count, 1, "one Http access for network policy");
        assert!(
            req.params_preview
                .contains("call api.example.com for inventory"),
            "preview must include the LLM's network_reason: {}",
            req.params_preview
        );
    }

    #[tokio::test]
    async fn deny_returns_tool_error_denied_and_skips_sandbox() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":true,"network_reason":"need to fetch","readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}"#,
        ));
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake.clone();
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        let (handle, _captured, _) = handle_with(ApprovalDecision::Deny);
        let ctx = make_ctx_with_handle(tmp.path().to_path_buf(), handle);
        let err = tool
            .execute(json!({"task": "fetch", "allow_network": true}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "deny must surface as ToolError::Denied, got: {err:?}"
        );
        assert!(
            fake.captured().is_empty(),
            "sandbox must not run after denial"
        );
        // Run dir is cleaned up by Drop on early return; nothing left
        // under code-builder/.
        let runs_root = tmp.path().join(aura_workspace::paths::CODE_BUILDER_SUBDIR);
        if runs_root.exists() {
            let count = std::fs::read_dir(&runs_root).unwrap().count();
            assert_eq!(count, 0, "denied run must be cleaned up by Drop");
        }
    }

    #[tokio::test]
    async fn approve_always_persists_into_session_cache() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":true,"network_reason":"fetch","readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}"#,
        ));
        let fake = FakeSandboxRunner::new(empty_output());
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let (handle, _captured, cache) = handle_with(ApprovalDecision::ApproveAlways);
        let ctx = make_ctx_with_handle(tmp.path().to_path_buf(), handle);
        let _ = tool
            .execute(json!({"task": "fetch", "allow_network": true}), &ctx)
            .await
            .unwrap();
        let p = cache.lock();
        assert_eq!(
            p.len(),
            1,
            "ApproveAlways must persist the access into the shared cache exactly once"
        );
    }

    #[tokio::test]
    async fn already_cached_access_does_not_re_prompt() {
        // Regression for codex review P2 #3: a follow-up call whose
        // accesses are already covered by the session's
        // `approved_resources` cache (e.g. after a prior
        // `ApproveAlways` or a cron pre-approval) must short-circuit
        // without invoking the gate, matching the pre-execute gate's
        // semantics in `aura-agent`'s `ToolExecutor::execute`.
        use aura_model::HostPattern;

        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":true,"network_reason":"fetch","readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}"#,
        ));
        let fake = FakeSandboxRunner::new(empty_output());
        let tool = make_tool(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        // Pre-populate the cache with `Http { host: "*" }`. This is
        // the exact entry an earlier `ApproveAlways` for the same
        // wildcard would have produced (`to_approved` lower-cases
        // and wraps the host string in `HostPattern::Exact`, and
        // CodeBuilder always emits the literal `"*"` host).
        let cache: ApprovalCache = Arc::new(PlMutex::new(vec![ApprovedResource::Http {
            host: HostPattern::Exact("*".into()),
        }]));
        // Deny-all gate; the test asserts it is *not* invoked.
        let (handle, captured, _cache) =
            handle_with_cache(ApprovalDecision::Deny, Arc::clone(&cache));
        let ctx = make_ctx_with_handle(tmp.path().to_path_buf(), handle);

        let out = tool
            .execute(json!({"task": "fetch", "allow_network": true}), &ctx)
            .await;
        assert!(
            out.is_ok(),
            "covered access must not produce a deny — got {out:?}"
        );
        assert!(
            captured.lock().is_empty(),
            "gate must NOT be invoked when the cache already covers every access"
        );
    }

    #[tokio::test]
    async fn missing_approval_handle_fails_closed() {
        // Production always wires an `ApprovalHandle` via the agent
        // executor; a tool that bypasses the agent (e.g. unit tests
        // that build `ToolContext` by hand) must default to deny so
        // we don't silently let a network-needing plan through.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":true,"network_reason":"fetch","readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}"#,
        ));
        let fake = FakeSandboxRunner::new(empty_output());
        let runner: Arc<dyn SandboxRunner> = fake.clone();
        let tool = make_tool(stub, runner);

        let tmp = tempfile::tempdir().unwrap();
        // make_ctx() leaves approval = None.
        let ctx = make_ctx(tmp.path().to_path_buf());
        let err = tool
            .execute(json!({"task": "fetch", "allow_network": true}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
        assert!(fake.captured().is_empty());
    }

    // ---------------------------------------------------------------
    // Pure-helper unit tests
    // ---------------------------------------------------------------

    use crate::plan::EffectivePlan;
    use std::path::Path;

    fn intent_file(p: PathBuf) -> WritableIntent {
        WritableIntent {
            path: p,
            kind: WritableKind::File,
        }
    }

    fn intent_dir(p: PathBuf) -> WritableIntent {
        WritableIntent {
            path: p,
            kind: WritableKind::Dir,
        }
    }

    fn plan_with(net: NetworkPolicy, writes: Vec<WritableIntent>) -> EffectivePlan {
        EffectivePlan {
            code: "x".into(),
            network_policy: net,
            network_reason: if net == NetworkPolicy::All {
                Some("reason".into())
            } else {
                None
            },
            readable_paths: vec![],
            writable_paths: writes,
            canonical_caller_writes: vec![],
            wall_clock_seconds: 30,
            memory_max_bytes: 256 * 1024 * 1024,
            pids_max: 64,
            rationale: "r".into(),
        }
    }

    #[test]
    fn build_accesses_empty_for_trivial_plan() {
        let scratch = Path::new("/tmp/aura/work/code-builder/uuid");
        let acc = super::build_approval_accesses(&plan_with(NetworkPolicy::None, vec![]), scratch);
        assert!(acc.is_empty());
    }

    #[test]
    fn build_accesses_emits_http_when_network_on() {
        let scratch = Path::new("/tmp/aura/work/code-builder/uuid");
        let acc = super::build_approval_accesses(&plan_with(NetworkPolicy::All, vec![]), scratch);
        assert_eq!(acc.len(), 1);
        assert!(matches!(acc[0], ResourceAccess::Http { .. }));
    }

    #[test]
    fn build_accesses_strips_writes_inside_scratch() {
        // A real run: the path must exist for canonicalize() to succeed.
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp
            .path()
            .join(aura_workspace::paths::CODE_BUILDER_SUBDIR)
            .join("uuid");
        std::fs::create_dir_all(&scratch).unwrap();
        let inside = scratch.join("workdir");
        std::fs::create_dir_all(&inside).unwrap();
        let acc = super::build_approval_accesses(
            &plan_with(NetworkPolicy::None, vec![intent_dir(inside)]),
            &scratch,
        );
        assert!(
            acc.is_empty(),
            "writable path inside scratch must not surface in approval list"
        );
    }

    #[test]
    fn build_accesses_includes_writes_outside_scratch() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp
            .path()
            .join(aura_workspace::paths::CODE_BUILDER_SUBDIR)
            .join("uuid");
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = tmp.path().join("project").join("output");
        std::fs::create_dir_all(&outside).unwrap();
        let acc = super::build_approval_accesses(
            &plan_with(NetworkPolicy::None, vec![intent_dir(outside.clone())]),
            &scratch,
        );
        assert_eq!(acc.len(), 1);
        match &acc[0] {
            ResourceAccess::WriteFile { path } => {
                assert_eq!(path, &outside);
            }
            other => panic!("expected WriteFile, got {other:?}"),
        }
    }

    #[test]
    fn build_accesses_does_not_treat_nonexistent_path_under_workspace_root_as_inside() {
        // Regression for codex review P1 #2: a write target that
        // does not exist on the host but happens to lexically live
        // under the agent's workspace root (NOT under the per-call
        // scratch) used to be silently classified as "inside" and
        // skipped the approval prompt — even though the sandbox
        // only RW-binds the scratch root, so
        // `resolve_writable_bind_targets` would later mkdir + bind
        // an unauthorised host directory. The fix tightens the
        // check: only the scratch root counts as auto-RW; anything
        // else must surface a WriteFile prompt.
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        let scratch = workspace
            .join(aura_workspace::paths::CODE_BUILDER_SUBDIR)
            .join("uuid");
        std::fs::create_dir_all(&scratch).unwrap();
        // Path that is under the workspace root but **not** under
        // the scratch root, and **does not exist** on the host.
        // This is the typical "the script will create it" case.
        let nonexistent = workspace.join("agent_state").join("secrets.json");
        assert!(!nonexistent.exists());
        let acc = super::build_approval_accesses(
            &plan_with(NetworkPolicy::None, vec![intent_file(nonexistent.clone())]),
            &scratch,
        );
        assert_eq!(
            acc.len(),
            1,
            "non-existent intent under workspace_root but outside scratch must trigger WriteFile approval"
        );
        assert!(matches!(&acc[0], ResourceAccess::WriteFile { path } if path == &nonexistent));
    }

    #[test]
    fn resolve_bind_targets_uses_parent_for_file_intent() {
        // LLM declares a not-yet-existing file inside a caller-granted
        // dir. Bind target is the parent (the caller dir itself in
        // this case — file's parent equals the grant). mkdir is a
        // no-op since the dir already exists.
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("out");
        std::fs::create_dir_all(&allowed).unwrap();
        let intent_path = allowed.join("today.csv");

        let targets = super::resolve_writable_bind_targets(
            std::slice::from_ref(&intent_file(intent_path.clone())),
            std::slice::from_ref(&allowed),
        )
        .unwrap();
        assert_eq!(targets, vec![allowed]);
        // File itself was NOT created (the script will do that).
        assert!(!intent_path.exists());
    }

    #[test]
    fn resolve_bind_targets_dir_intent_binds_self_not_parent() {
        // LLM declared a sub-directory of caller's grant with trailing
        // slash → dir intent. Bind = the dir itself (after mkdir),
        // NOT the parent. This is the whole point of trailing-slash
        // detection: keep the authorised scope narrow when the LLM
        // means "I'm going to write inside this dir".
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("output");
        std::fs::create_dir_all(&allowed).unwrap();
        let sub = allowed.join("sub");

        let targets = super::resolve_writable_bind_targets(
            std::slice::from_ref(&intent_dir(sub.clone())),
            std::slice::from_ref(&allowed),
        )
        .unwrap();
        assert_eq!(targets, vec![sub.clone()]);
        assert!(sub.exists() && sub.is_dir());
    }

    #[test]
    fn resolve_bind_targets_creates_missing_intermediate_dirs() {
        // Deep nested file intent under caller grant. mkdir -p creates
        // the missing ancestors and the bind target is the *immediate*
        // parent of the intent, not the caller's broader allowlist.
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("output");
        std::fs::create_dir_all(&allowed).unwrap();
        let intent_path = allowed.join("sub").join("deep").join("file.csv");
        let expected_bind = allowed.join("sub").join("deep");

        let targets = super::resolve_writable_bind_targets(
            std::slice::from_ref(&intent_file(intent_path)),
            std::slice::from_ref(&allowed),
        )
        .unwrap();
        assert_eq!(
            targets,
            vec![expected_bind.clone()],
            "bind target must be the immediate parent dir, not the caller's broader grant"
        );
        assert!(expected_bind.exists());
        assert!(expected_bind.is_dir());
    }

    #[test]
    fn resolve_bind_targets_intent_equal_to_caller_grant_binds_grant_itself() {
        // LLM declared the exact caller grant as a directory intent.
        // Bind = grant itself.
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("output");
        std::fs::create_dir_all(&allowed).unwrap();

        let targets = super::resolve_writable_bind_targets(
            std::slice::from_ref(&intent_dir(allowed.clone())),
            std::slice::from_ref(&allowed),
        )
        .unwrap();
        assert_eq!(targets, vec![allowed]);
    }

    #[test]
    fn caller_grant_is_a_file_slot_binds_parent_dir() {
        // Real user scenario: the caller passes a file path (not a
        // dir) as the writable allowlist entry, the LLM declares the
        // same file path with no trailing slash (file intent). The
        // post-approval step must bind the file's parent dir so the
        // script's `open(file, 'w')` lands on a regular file. Without
        // this, the previous logic mkdir'd the file path itself —
        // turning it into a directory and breaking the script.
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().canonicalize().unwrap().join("test_output");
        // Note: we deliberately do NOT create the parent dir on the
        // host — projection's caller-side walk-up canonicalize must
        // tolerate that, and resolve_writable_bind_targets must
        // mkdir it.
        let file_slot = parent.join("hello");

        let targets = super::resolve_writable_bind_targets(
            std::slice::from_ref(&intent_file(file_slot.clone())),
            std::slice::from_ref(&file_slot),
        )
        .unwrap();
        assert_eq!(targets, vec![parent.clone()]);
        assert!(parent.is_dir(), "parent dir must be created by mkdir -p");
        assert!(!file_slot.exists(), "the file itself is not pre-created");
    }

    #[test]
    fn projection_accepts_caller_writable_path_that_does_not_exist() {
        // Real user scenario from the conversation: the caller passes
        // a path the script will create. The old `canonical_for_check`
        // failed with "cannot canonicalize: No such file or directory".
        // The new walk-up canonicalize on the caller side accepts it
        // and the LLM's matching intent goes through.
        use crate::parse::RawPlan;
        use crate::plan::{CallerCaps, HardCaps};

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().canonicalize().unwrap();
        let nonexistent = workspace.join("test_output").join("hello");

        let raw = RawPlan {
            code: "print(1)".into(),
            network_required: false,
            network_reason: None,
            readable_paths: vec![],
            writable_paths: vec![nonexistent.display().to_string()],
            estimated_runtime_seconds: Some(1),
            estimated_memory_mb: Some(64),
            rationale: "r".into(),
        };
        let caps = CallerCaps {
            max_runtime_seconds: None,
            max_memory_mb: None,
            allow_network: false,
            extra_readable_paths: vec![],
            extra_writable_paths: vec![nonexistent.clone()],
        };
        let plan = crate::plan::project(raw, &caps, &HardCaps::defaults()).unwrap();
        assert_eq!(plan.writable_paths.len(), 1);
        assert_eq!(plan.writable_paths[0].path, nonexistent);
        assert_eq!(plan.writable_paths[0].kind, WritableKind::File);
        assert_eq!(plan.canonical_caller_writes, vec![nonexistent]);
    }

    #[test]
    fn resolve_bind_targets_dedups_intents_in_same_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("output");
        std::fs::create_dir_all(&allowed).unwrap();
        let a = intent_file(allowed.join("a.csv"));
        let b = intent_file(allowed.join("b.csv"));

        let targets =
            super::resolve_writable_bind_targets(&[a, b], std::slice::from_ref(&allowed)).unwrap();
        assert_eq!(
            targets,
            vec![allowed],
            "two file intents in the same parent must dedup to one bind target"
        );
    }

    #[test]
    fn resolve_bind_targets_rejects_symlink_escape_after_mkdir() {
        // TOCTOU defense: between projection's lexical containment
        // check and the bind, a symlink could redirect the parent dir
        // outside the allowlist. After `mkdir -p` and `canonicalize`,
        // we re-check that the resolved bind target is still under a
        // caller grant; symlink escapes are refused.
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().canonicalize().unwrap().join("allowed");
        let elsewhere = tmp.path().canonicalize().unwrap().join("elsewhere");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        // `allowed/redirect` is a symlink to elsewhere/. The lexical
        // check would have accepted `allowed/redirect/file.csv` as
        // "starts_with allowed", but canonicalize resolves the
        // symlink to elsewhere/file.csv — which is outside the
        // allowlist.
        std::os::unix::fs::symlink(&elsewhere, allowed.join("redirect")).unwrap();
        let intent = intent_file(allowed.join("redirect").join("file.csv"));

        let err = super::resolve_writable_bind_targets(
            std::slice::from_ref(&intent),
            std::slice::from_ref(&allowed),
        )
        .unwrap_err();
        assert!(
            matches!(err, CodeBuilderError::LlmPlanRejected(ref m) if m.contains("outside extra_writable_paths")),
            "got: {err:?}"
        );
    }

    #[test]
    fn approval_preview_emits_only_network_reason_when_network_required() {
        let plan = plan_with(NetworkPolicy::All, vec![]);
        let writable: Vec<String> = vec![];
        let preview = super::approval_preview("write a CSV from /data/in.csv", &plan, &writable);
        assert_eq!(preview, "network reason: reason", "preview: {preview}");
        assert!(
            !preview.contains("write a CSV"),
            "task should not appear in preview: {preview}"
        );
        assert!(
            !preview.contains("rationale") && !preview.contains("\nwhy"),
            "rationale should be dropped: {preview}"
        );
    }

    #[test]
    fn approval_preview_is_empty_when_network_policy_none() {
        let plan = plan_with(NetworkPolicy::None, vec![]);
        let writable: Vec<String> = vec![];
        let preview = super::approval_preview("local-only task", &plan, &writable);
        assert!(
            preview.is_empty(),
            "write-only plan should produce an empty preview so the channel UI skips the params block: {preview}"
        );
    }

    #[tokio::test]
    async fn approval_preview_re_mints_plaintext_secret_in_task() {
        // Regression for codex adversarial review #1. ToolExecutor
        // reveals placeholders before invoking the tool, so a vaulted
        // AWS key arrives as plaintext in `p.task`. The pre-gate
        // sanitization must re-mint it back into placeholder form
        // before the request travels out to the channel UI.
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(
            r#"{"code":"print(1)","network_required":true,"network_reason":"reach api","readable_paths":[],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}"#,
        ));
        let fake = FakeSandboxRunner::new(empty_output());
        let (tool, vault) = make_tool_with_vault(stub, fake as Arc<dyn SandboxRunner>);

        let tmp = tempfile::tempdir().unwrap();
        let (handle, captured, _) = handle_with(ApprovalDecision::Approve);
        let ctx = make_ctx_with_handle(tmp.path().to_path_buf(), handle);
        let _ = tool
            .execute(
                json!({
                    "task": "use AKIAIOSFODNN7EXAMPLE to call api.example.com",
                    "allow_network": true,
                }),
                &ctx,
            )
            .await
            .unwrap();

        let preview_text = {
            let reqs = captured.lock();
            assert_eq!(reqs.len(), 1);
            reqs[0].params_preview.clone()
        };
        assert!(
            !preview_text.contains("AKIAIOSFODNN7EXAMPLE"),
            "plaintext AWS key leaked into approval preview: {preview_text}"
        );
        // The preview no longer carries the task itself (only the
        // network reason, when applicable). The original plaintext
        // must still be re-minted into the vault under its placeholder
        // so the script body keeps using the vaulted form.
        let minter = PlaceholderMinter::from_master_key(vault.master_key());
        let placeholder = minter.mint(b"AKIAIOSFODNN7EXAMPLE");
        let stored = vault.get_secret(&placeholder).await.unwrap().unwrap();
        let bytes: &[u8] = stored.as_bytes();
        assert_eq!(bytes, b"AKIAIOSFODNN7EXAMPLE");
    }

    #[tokio::test]
    async fn approval_preview_re_mints_plaintext_secret_in_writable_path() {
        // Even a writable path traveling through the access list
        // must be rescanned: a caller that templated a plaintext
        // secret into `extra_writable_paths` would otherwise leak
        // it through `ResourceAccess::WriteFile { path }`.
        let tmp = tempfile::tempdir().unwrap();
        // Path containing a leak-pattern-shaped substring. Must exist
        // because project() canonicalizes caller-side write paths.
        let leak_dir = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("AKIAIOSFODNN7EXAMPLE")
            .join("out");
        std::fs::create_dir_all(&leak_dir).unwrap();

        let llm_path = leak_dir.display().to_string();
        let response_json = format!(
            r#"{{"code":"print(1)","network_required":false,"readable_paths":[],"writable_paths":["{}"],"estimated_runtime_seconds":1,"estimated_memory_mb":64,"rationale":"r"}}"#,
            llm_path.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let stub = Arc::new(StubLlm::new());
        stub.push_response(ok_response(&response_json));
        let fake = FakeSandboxRunner::new(empty_output());
        let (tool, vault) = make_tool_with_vault(stub, fake as Arc<dyn SandboxRunner>);

        let workspace = tempfile::tempdir().unwrap();
        let (handle, captured, _) = handle_with(ApprovalDecision::Approve);
        let ctx = make_ctx_with_handle(workspace.path().to_path_buf(), handle);
        let _ = tool
            .execute(
                json!({
                    "task": "render a CSV",
                    "extra_writable_paths": [llm_path],
                }),
                &ctx,
            )
            .await
            .unwrap();

        let (accesses, preview_text) = {
            let reqs = captured.lock();
            assert_eq!(reqs.len(), 1);
            (reqs[0].accesses.clone(), reqs[0].params_preview.clone())
        };
        for acc in &accesses {
            if let ResourceAccess::WriteFile { path } = acc {
                let s = path.to_string_lossy();
                assert!(
                    !s.contains("AKIAIOSFODNN7EXAMPLE"),
                    "plaintext secret leaked through WriteFile path: {s}"
                );
            }
        }
        // The preview's path list also must not carry the plaintext.
        assert!(
            !preview_text.contains("AKIAIOSFODNN7EXAMPLE"),
            "plaintext secret leaked through preview: {preview_text}"
        );
        // Original is preserved in the vault under the minted placeholder.
        let minter = PlaceholderMinter::from_master_key(vault.master_key());
        let placeholder = minter.mint(b"AKIAIOSFODNN7EXAMPLE");
        let stored = vault.get_secret(&placeholder).await.unwrap();
        assert!(stored.is_some(), "original must be persisted to vault");
    }
}
