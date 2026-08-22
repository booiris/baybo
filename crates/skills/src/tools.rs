//! `Skill` tool — the LLM-facing entry point for declarative skills.
//!
//! Env-var values are never templated into the response: skill bodies
//! are untrusted prompt content and the LLM context is not the right
//! exfiltration boundary for secrets. The body is expected to
//! instruct downstream tool calls on how to read them in-process.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_model::TrustLevel;
use baybo_tools::{
    ApprovalDecision, NoticeLevel, ResourceAccess as ToolResourceAccess, Tool, ToolCapability,
    ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput, ToolTriggerScope,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{SkillDefinition, SkillGate, SkillRegistry, SkillRiskCheck};

const MAX_SUBFILE_BYTES: u64 = 256 * 1024;
const MAX_ARGS_BYTES: usize = 4 * 1024;

/// Wire-level name of the Skill tool. Single source of truth for any
/// caller that needs to recognise a `ContentBlock::ToolUse { name, .. }`
/// as a skill invocation (compression trailer, slash-command synthesis,
/// trace filters).
pub const SKILL_TOOL_NAME: &str = "Skill";

/// JSON input field name carrying the skill identifier inside a
/// `ContentBlock::ToolUse { input, .. }` for the Skill tool. Shared
/// across the agent loop's slash synthesis, the compression-trailer
/// scanner, and this tool's own param parsing so all four sites stay
/// in lockstep.
pub const SKILL_INPUT_NAME_FIELD: &str = "skill";

pub fn build(
    registry: Arc<SkillRegistry>,
    risk_check: Arc<dyn SkillRiskCheck>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let tool: Arc<dyn Tool> = Arc::new(SkillTool {
        registry,
        risk_check,
    });
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
        channels: Vec::new(),
    };
    (tool, manifest)
}

struct SkillTool {
    registry: Arc<SkillRegistry>,
    risk_check: Arc<dyn SkillRiskCheck>,
}

#[derive(Debug, Deserialize)]
struct SkillParams {
    skill: String,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        SKILL_TOOL_NAME
    }

    /// Reads skill metadata/files (the risk-assessor cache write is
    /// idempotent) — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "Load a registered skill so its instructions enter the conversation. \
         Available skills are listed in a system reminder each turn — invoke \
         this tool with `skill: \"<name>\"` to pull one in. Pass `args` to \
         forward free-form arguments. Pass `file_path` to fetch a sub-file \
         (relative path inside the skill's directory) referenced from the \
         main SKILL.md. Skills the operator marked untrusted or marked \
         `disable-model-invocation: true` are not callable here."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                SKILL_INPUT_NAME_FIELD: {
                    "type": "string",
                    "description": "Name of the skill to load — must match an entry from the system-reminder skill list."
                },
                "args": {
                    "type": "string",
                    "description": "Optional free-form arguments passed alongside the skill body. Returned as a top-level JSON field for skill authors who key off it."
                },
                "file_path": {
                    "type": "string",
                    "description": "Optional sub-file path relative to the skill's directory (e.g. \"references/dataset-formats.md\"). When set, the tool returns that file's contents instead of SKILL.md plus the linked-file inventory."
                }
            },
            "required": [SKILL_INPUT_NAME_FIELD]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        let skill = params.get(SKILL_INPUT_NAME_FIELD).and_then(Value::as_str)?;
        let args = params
            .get("args")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let label = if args.is_empty() {
            skill.to_string()
        } else {
            format!("{skill} {args}")
        };
        baybo_tools::progress::preview_arg(&label)
    }

    fn max_timeout(&self) -> Duration {
        // The risk assessor is an LLM call on every invocation and
        // routinely takes 10-20 s; the trait-default 30 s leaves no
        // headroom for the file read that follows. 60 s covers a
        // slow assessor turn plus the SKILL.md / sub-file load.
        Duration::from_secs(60)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let mut p: SkillParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        // Strict-JSON-schema LLMs sometimes emit `""` instead of omitting
        // the key. Without this filter, `file_path: ""` would reach
        // `resolve_subpath` and hard-error, trapping the model in a retry
        // loop instead of falling back to the main SKILL.md body.
        p.file_path = p.file_path.filter(|s| !s.trim().is_empty());
        p.args = p.args.filter(|s| !s.trim().is_empty());

        if let Some(args) = &p.args
            && args.len() > MAX_ARGS_BYTES
        {
            return Err(ToolError::InvalidParams(format!(
                "`args` exceeds {} bytes",
                MAX_ARGS_BYTES
            )));
        }

        // Scoped lookup: this agent's own directory first, then the builtins
        // its scope admits. A name that exists only in another agent's
        // directory is simply not found — the miss must not reveal that it
        // exists elsewhere.
        let skill = self
            .registry
            .get_scoped(Some(&ctx.agent_id), &p.skill)
            .ok_or_else(|| ToolError::NotFound(format!("skill '{}'", p.skill)))?;

        if !skill.agent_invocable {
            return Err(ToolError::NotFound(format!(
                "skill '{}' is not invocable by the model",
                p.skill
            )));
        }
        if matches!(skill.trust_level, TrustLevel::Untrusted) {
            return Err(ToolError::NotFound(format!(
                "skill '{}' is untrusted and cannot be invoked",
                p.skill
            )));
        }
        if !skill.channels.is_empty() && !skill.channels.contains(&ctx.user.channel) {
            return Err(ToolError::NotFound(format!(
                "skill '{}' is not available on this channel",
                p.skill
            )));
        }

        let warning = match self.risk_check.assess(&skill).await {
            SkillGate::Pass => None,
            SkillGate::PassWithWarning { rationale } => {
                emit_skill_notice(
                    ctx,
                    NoticeLevel::Warn,
                    &skill.name,
                    "rated suspicious",
                    &rationale,
                );
                Some(rationale)
            }
            SkillGate::Block { rationale } => {
                emit_skill_notice(ctx, NoticeLevel::Error, &skill.name, "blocked", &rationale);
                return Err(ToolError::Denied {
                    tool: self.name().to_string(),
                    reason: format!(
                        "skill '{}' blocked by risk assessor: {rationale}",
                        skill.name
                    ),
                });
            }
        };

        if !skill.requirements.required_env.is_empty() {
            check_env_or_prompt(self.name(), &skill, ctx).await?;
        }

        let mut out = match p.file_path.as_deref() {
            None => render_main(&skill, p.args.as_deref(), ctx.session_id.as_ref())?,
            Some(rel) => render_subfile(&skill, rel).await?,
        };
        if let Some(warn) = warning.as_deref() {
            match &mut out {
                ToolOutput::Json(v) => {
                    v["risk_warning"] = serde_json::Value::String(warn.to_string());
                }
                _ => debug_assert!(
                    false,
                    "Skill tool must return Json so risk_warning can attach"
                ),
            }
        }
        Ok(out)
    }
}

fn emit_skill_notice(
    ctx: &ToolContext,
    level: NoticeLevel,
    skill_name: &str,
    headline: &str,
    rationale: &str,
) {
    let Some(notifier) = ctx.notifier.as_ref() else {
        return;
    };
    notifier.emit(
        level,
        &format!("Skill '{skill_name}' {headline}"),
        rationale,
    );
}

async fn check_env_or_prompt(
    tool_name: &str,
    skill: &SkillDefinition,
    ctx: &ToolContext,
) -> baybo_tools::Result<()> {
    let required = &skill.requirements.required_env;

    let missing: Vec<String> = required
        .iter()
        .filter(|name| std::env::var(name).is_err())
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::Execution(format!(
            "skill '{}' requires env vars not set on host: {}",
            skill.name,
            missing.join(", ")
        )));
    }

    let approval = ctx.approval.as_ref().ok_or_else(|| {
        ToolError::Execution(format!(
            "skill '{}' requires env-var approval but no approval handle is configured",
            skill.name
        ))
    })?;

    let preview = format!("Skill '{}' env vars: {}", skill.name, required.join(", "));
    let outcome = approval
        .request_uncached(
            tool_name,
            &ctx.session_id,
            &ctx.user,
            vec![ToolResourceAccess::Env {
                vars: required.clone(),
            }],
            preview,
        )
        .await;
    match outcome.decision {
        ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => Ok(()),
        ApprovalDecision::Deny => Err(ToolError::Denied {
            tool: tool_name.to_string(),
            reason: format!(
                "env-var access required by skill '{}' was not approved — {}",
                skill.name,
                baybo_tools::refusal_reason(outcome.resolution)
            ),
        }),
    }
}

/// Token in a built-in SKILL.md body that gets substituted with the
/// running session's id at render time. Only the baybo-cli skill uses
/// it today; the substitution is a no-op for skill bodies that don't
/// contain the token, so every skill goes through the same path.
const SESSION_ID_TOKEN: &str = "{{session_id}}";

/// Render a skill's prompt body for injection: substitute the running
/// session's id into [`SESSION_ID_TOKEN`]. Shared by [`render_main`] (the
/// `Skill` tool's output) and `baybo-context`'s slash-command expansion, so the
/// substitution rule lives in one place rather than being copied per caller.
pub fn render_skill_body(skill: &SkillDefinition, session_id: &str) -> String {
    skill.prompt_template.replace(SESSION_ID_TOKEN, session_id)
}

/// Appended to a slash-injected skill body when the skill ships linked
/// files, so a `/command` invocation can discover and pull sub-files just
/// like an LLM-issued `Skill` call. [`render_main`] advertises the same
/// affordance as JSON (`linked_files` + `usage_hint`); the slash path
/// injects plain text, so the equivalent rendering lives here. The
/// `{{skill_name}}` / `{{tool_name}}` / `{{file_list}}` markers are
/// substituted at render time.
const SLASH_LINKED_FILES_HINT: &str = r#"<system-reminder>
The "{{skill_name}}" skill ships linked files. To read one, call the {{tool_name}} tool with `skill` set to "{{skill_name}}" and `file_path` set to one of:
{{file_list}}
</system-reminder>"#;

/// Render a skill's body for slash injection (`{{session_id}}` substituted),
/// plus — when the skill ships linked sub-files — a text inventory + usage
/// hint matching what [`render_main`] exposes to the LLM `Skill` tool, so the
/// two invocation routes don't diverge. The sub-file fetch itself still goes
/// through the gated `Skill` tool. Shared with `baybo-context`'s slash
/// expansion.
pub fn render_skill_for_slash(skill: &SkillDefinition, session_id: &str) -> String {
    let mut out = render_skill_body(skill, session_id);
    if let Some(hint) = render_linked_files_hint(skill) {
        out.push_str("\n\n");
        out.push_str(&hint);
    }
    out
}

fn render_linked_files_hint(skill: &SkillDefinition) -> Option<String> {
    if skill.linked_files.is_empty() {
        return None;
    }
    let file_list = skill
        .linked_files
        .iter_paths()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(
        SLASH_LINKED_FILES_HINT
            .replace("{{skill_name}}", &skill.name)
            .replace("{{tool_name}}", SKILL_TOOL_NAME)
            .replace("{{file_list}}", &file_list),
    )
}

fn render_main(
    skill: &SkillDefinition,
    args: Option<&str>,
    session_id: &str,
) -> baybo_tools::Result<ToolOutput> {
    let dir = skill.source_path.as_deref();
    let path = dir.map(|d| d.join("SKILL.md"));

    let content = render_skill_body(skill, session_id);

    let mut out = json!({
        "name": skill.name,
        "version": skill.version,
        "description": skill.description,
        "content": content,
        "skill_dir": dir.map(path_to_string),
        "path": path.as_deref().map(path_to_string),
        "linked_files": skill.linked_files.to_json(),
        "usage_hint": "To pull in a linked file, call this tool again with `file_path` set to a relative path returned in `linked_files`.",
    });

    if let Some(a) = args {
        out["args"] = Value::String(a.to_string());
    }

    Ok(ToolOutput::Json(out))
}

async fn render_subfile(
    skill: &SkillDefinition,
    file_path: &str,
) -> baybo_tools::Result<ToolOutput> {
    let dir = skill.source_path.as_deref().ok_or_else(|| {
        ToolError::Execution(format!(
            "skill '{}' has no on-disk directory; cannot read sub-files",
            skill.name
        ))
    })?;

    let resolved = resolve_subpath(dir, file_path)
        .await
        .map_err(ToolError::InvalidParams)?;

    let metadata = tokio::fs::metadata(&resolved)
        .await
        .map_err(|e| ToolError::Execution(format!("stat {file_path}: {e}")))?;
    if !metadata.is_file() {
        return Err(ToolError::InvalidParams(format!(
            "`{file_path}` is not a regular file"
        )));
    }
    if metadata.len() > MAX_SUBFILE_BYTES {
        return Err(ToolError::InvalidParams(format!(
            "`{file_path}` is {} bytes; max is {}",
            metadata.len(),
            MAX_SUBFILE_BYTES
        )));
    }

    let bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|e| ToolError::Execution(format!("read {file_path}: {e}")))?;
    let content = String::from_utf8(bytes).map_err(|e| {
        ToolError::Execution(format!(
            "skill sub-file `{file_path}` is not valid UTF-8: {e}"
        ))
    })?;

    let file_type = resolved
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();

    Ok(ToolOutput::Json(json!({
        "name": skill.name,
        "file": file_path,
        "content": content,
        "file_type": file_type,
    })))
}

async fn resolve_subpath(skill_dir: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("`file_path` must not be empty".into());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(format!("`file_path` must be relative, got `{rel}`"));
    }
    for component in p.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(format!("`file_path` may not contain `..` (got `{rel}`)"));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!("`file_path` must be relative (got `{rel}`)"));
            }
        }
    }

    let candidate = skill_dir.join(p);
    let real_dir = tokio::fs::canonicalize(skill_dir)
        .await
        .map_err(|e| format!("canonicalize skill dir: {e}"))?;
    let real_target = tokio::fs::canonicalize(&candidate)
        .await
        .map_err(|e| format!("canonicalize `{rel}`: {e}"))?;
    if !real_target.starts_with(&real_dir) {
        return Err(format!(
            "`file_path` `{rel}` resolves outside the skill directory"
        ));
    }
    Ok(real_target)
}

fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The skill directory this session installs into and removes from: its
/// agent's own, at `personas/<id>/skills/`.
///
/// Derived per call rather than wired once at startup, because that is what
/// makes **"install what you can see"** hold — the destination has to follow
/// the caller, and a process-wide root cannot. It lands on exactly the
/// directory [`SkillRegistry::get_scoped`] reads first for this scope, so an
/// install is visible to the agent that asked for it and to nobody else.
fn scope_skills_dir(ctx: &ToolContext) -> PathBuf {
    ctx.agent_id.skills_dir(&ctx.workspace_paths)
}

pub fn build_install_tool(
    registry: Arc<SkillRegistry>,
    risk_check: Arc<dyn SkillRiskCheck>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let tool: Arc<dyn Tool> = Arc::new(SkillInstallTool {
        registry,
        risk_check,
    });
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![ToolCapability::WriteFile],
        channels: Vec::new(),
    };
    (tool, manifest)
}

struct SkillInstallTool {
    registry: Arc<SkillRegistry>,
    risk_check: Arc<dyn SkillRiskCheck>,
}

#[derive(Debug, Deserialize)]
struct InstallParams {
    source_dir: String,
}

#[async_trait]
impl Tool for SkillInstallTool {
    fn name(&self) -> &str {
        "SkillInstall"
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::SharedWorkspace
    }

    fn description(&self) -> String {
        "Install a skill from an on-disk directory into this session's own \
         skills folder. Every agent has one of its own, so the skill lands in \
         exactly the set this session can see: it shows up in the next turn's \
         listing and in no other agent's. The source directory must contain a \
         valid SKILL.md; the skill is run through the risk assessor \
         (Dangerous verdicts abort the install) and the registry is \
         hot-reloaded. Refuses to overwrite an existing installation; the old \
         copy must be removed first."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source_dir": {
                    "type": "string",
                    "description": "Absolute path to the skill source directory (must contain SKILL.md)."
                }
            },
            "required": ["source_dir"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("source_dir")
            .and_then(Value::as_str)
            .map(|s| baybo_tools::progress::preview_path(Path::new(s)))
    }

    fn max_timeout(&self) -> Duration {
        // The pipeline is risk-assessor LLM call + recursive directory
        // copy + registry hot-reload. A bigger skill bundle (templates
        // + scripts + reference docs) plus a slow assessor turn
        // routinely exceeds 30 s; 120 s is the comfortable upper bound.
        Duration::from_secs(120)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: InstallParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let source_dir = PathBuf::from(&p.source_dir);
        let source_dir = tokio::fs::canonicalize(&source_dir)
            .await
            .map_err(|e| ToolError::InvalidParams(format!("source_dir `{}`: {e}", p.source_dir)))?;
        let meta = tokio::fs::metadata(&source_dir)
            .await
            .map_err(|e| ToolError::InvalidParams(format!("stat source_dir: {e}")))?;
        if !meta.is_dir() {
            return Err(ToolError::InvalidParams(format!(
                "`{}` is not a directory",
                source_dir.display()
            )));
        }

        let dest_root = scope_skills_dir(ctx);

        // Reject before parsing if the source resolves into the destination
        // tree — caller pointed at a skill it already has installed. Only the
        // destination: a source under *another* agent's folder is allowed by
        // design, because refusing it would not withhold the capability (see
        // docs/modules/skills.md, "Skill installation").
        if let Ok(canonical_dest_root) = tokio::fs::canonicalize(&dest_root).await
            && source_dir.starts_with(&canonical_dest_root)
        {
            return Err(ToolError::InvalidParams(format!(
                "source_dir `{}` is already inside this session's skills directory",
                source_dir.display()
            )));
        }

        let skill = crate::loader::load_skill_from_dir(&source_dir)
            .map_err(|e| ToolError::InvalidParams(format!("invalid skill source: {e}")))?;

        let dest_dir = dest_root.join(&skill.name);
        if tokio::fs::try_exists(&dest_dir).await.unwrap_or(false) {
            return Err(ToolError::Execution(format!(
                "skill '{}' already installed at {}; remove it first",
                skill.name,
                dest_dir.display()
            )));
        }

        let warning = match self.risk_check.assess(&skill).await {
            SkillGate::Pass => None,
            SkillGate::PassWithWarning { rationale } => {
                emit_skill_notice(
                    ctx,
                    NoticeLevel::Warn,
                    &skill.name,
                    "rated suspicious",
                    &rationale,
                );
                Some(rationale)
            }
            SkillGate::Block { rationale } => {
                emit_skill_notice(ctx, NoticeLevel::Error, &skill.name, "blocked", &rationale);
                return Err(ToolError::Denied {
                    tool: self.name().to_string(),
                    reason: format!(
                        "skill '{}' blocked by risk assessor: {rationale}",
                        skill.name
                    ),
                });
            }
        };

        // Stage at depth 2 (`<dest_root>/.staging/<uuid>/`) rather than
        // alongside `<dest_root>/<name>/`. Both of the registry's scanners
        // inspect only depth-1 children for `SKILL.md`, so a leaked staging
        // tree (process killed between copy and rename) won't be loaded as a
        // phantom skill on the next reload. Staging inside the destination
        // is also what keeps the rename same-filesystem, hence atomic.
        let staging_root = dest_root.join(".staging");
        tokio::fs::create_dir_all(&staging_root)
            .await
            .map_err(|e| ToolError::Execution(format!("ensure staging dir: {e}")))?;
        let staging = staging_root.join(uuid::Uuid::new_v4().to_string());
        if let Err(e) = copy_dir_recursive(&source_dir, &staging).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(ToolError::Execution(format!("copy skill files: {e}")));
        }
        if let Err(e) = tokio::fs::rename(&staging, &dest_dir).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(ToolError::Execution(format!(
                "rename {} -> {}: {e}",
                staging.display(),
                dest_dir.display()
            )));
        }

        // An overlay the agent did not have before this install is absent from
        // `agent_dirs`, and `reload` replays exactly that list — so without
        // registering the freshly created directory first, the reload below
        // would drop the skill again and the install would be invisible for
        // the life of the process. A no-op for the built-in and for an
        // overlay already scanned.
        self.registry
            .ensure_agent_skills(&ctx.agent_id, &ctx.workspace_paths);
        self.registry.reload();

        let mut out = json!({
            "name": skill.name,
            "version": skill.version,
            "installed_at": path_to_string(&dest_dir),
            "skills_in_scope": self.registry.summaries_for(Some(&ctx.agent_id)).len(),
            "linked_files": skill.linked_files.to_json(),
        });
        if let Some(w) = warning {
            out["risk_warning"] = Value::String(w);
        }
        Ok(ToolOutput::Json(out))
    }
}

pub fn build_uninstall_tool(registry: Arc<SkillRegistry>) -> (Arc<dyn Tool>, ToolManifest) {
    let tool: Arc<dyn Tool> = Arc::new(SkillUninstallTool { registry });
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![ToolCapability::WriteFile],
        channels: Vec::new(),
    };
    (tool, manifest)
}

struct SkillUninstallTool {
    registry: Arc<SkillRegistry>,
}

#[derive(Debug, Deserialize)]
struct UninstallParams {
    name: String,
}

#[async_trait]
impl Tool for SkillUninstallTool {
    fn name(&self) -> &str {
        "SkillUninstall"
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::SharedWorkspace
    }

    fn description(&self) -> String {
        "Remove an installed skill from this session's own skills folder. \
         Only skills this session can see are addressable by name, and of \
         those only the ones living under its own folder are removable — a \
         skill reached from anywhere else (one compiled into the binary, \
         another agent's folder, a registry-only or third-party-mounted \
         skill) is refused rather than deleted. The registry is hot-reloaded so the skill disappears \
         from the next turn's listing."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the registered skill to remove."
                }
            },
            "required": ["name"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("name")
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: UninstallParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        // Scoped lookup, same rule as the `Skill` tool: a name this session
        // cannot see is an ordinary miss, never a refusal that would reveal
        // another agent's inventory.
        let skill = self
            .registry
            .get_scoped(Some(&ctx.agent_id), &p.name)
            .ok_or_else(|| ToolError::NotFound(format!("skill '{}'", p.name)))?;

        let source_path = skill.source_path.as_deref().ok_or_else(|| {
            ToolError::Execution(format!(
                "skill '{}' has no on-disk directory; cannot uninstall",
                p.name
            ))
        })?;

        // Canonicalize both sides before the prefix check — symlinks
        // could otherwise let an attacker register a skill whose
        // source_path resolves outside the skills dir but compares
        // equal at the lexical level.
        let real_source = tokio::fs::canonicalize(source_path).await.map_err(|e| {
            ToolError::Execution(format!("canonicalize {}: {e}", source_path.display()))
        })?;
        // Containment is against the caller's OWN tree, not "some skills
        // tree": seeing a skill and owning it are different things. A custom
        // agent reaches a `UNIVERSAL_SKILLS` entry among the builtins and
        // must still not be able to remove it — and a session whose own
        // directory does not exist yet owns nothing at all, which is why a
        // root that fails to canonicalize refuses rather than errors.
        let scope_root = scope_skills_dir(ctx);
        let contained = match tokio::fs::canonicalize(&scope_root).await {
            // Equality, not just the prefix: a skill that somehow registered
            // with `source_path` = the root itself must not take the whole
            // tree with it.
            Ok(real_root) => real_source.starts_with(&real_root) && real_source != real_root,
            Err(_) => false,
        };
        if !contained {
            return Err(ToolError::Denied {
                tool: self.name().to_string(),
                reason: format!(
                    "skill '{}' lives outside this session's skills directory ({}); refusing to delete {}",
                    p.name,
                    scope_root.display(),
                    real_source.display()
                ),
            });
        }

        tokio::fs::remove_dir_all(&real_source)
            .await
            .map_err(|e| ToolError::Execution(format!("remove {}: {e}", real_source.display())))?;

        self.registry.reload();

        Ok(ToolOutput::Json(json!({
            "name": skill.name,
            "removed_from": path_to_string(&real_source),
            "skills_in_scope": self.registry.summaries_for(Some(&ctx.agent_id)).len(),
        })))
    }
}

async fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    let mut total_files: usize = 0;
    let mut total_bytes: u64 = 0;
    tokio::fs::create_dir_all(dst)
        .await
        .map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let entries = walkdir::WalkDir::new(src).follow_links(false).min_depth(1);
    for entry in entries {
        let entry = entry.map_err(|e| format!("walk {}: {e}", src.display()))?;
        let rel = entry.path().strip_prefix(src).map_err(|e| e.to_string())?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            tokio::fs::create_dir_all(&target)
                .await
                .map_err(|e| format!("mkdir {}: {e}", target.display()))?;
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        total_files += 1;
        if total_files > crate::MAX_SKILL_DIR_FILES {
            return Err(format!(
                "skill directory contains more than {} files",
                crate::MAX_SKILL_DIR_FILES
            ));
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total_bytes = total_bytes.saturating_add(len);
        if total_bytes > crate::MAX_SKILL_DIR_BYTES {
            return Err(format!(
                "skill directory exceeds {} bytes",
                crate::MAX_SKILL_DIR_BYTES
            ));
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        tokio::fs::copy(entry.path(), &target).await.map_err(|e| {
            format!(
                "copy {} -> {}: {e}",
                entry.path().display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use baybo_model::{ArtifactSource, ChannelType, TrustLevel, User};
    use baybo_tools::{ApprovalHandle, AutoDenyGate};
    use parking_lot::Mutex;
    use tempfile::tempdir;

    use super::*;
    use crate::{AlwaysPass, SkillRequirements, linked_files};

    fn mk_skill(dir: &Path, name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            version: "1.0.0".into(),
            description: "test".into(),
            command: Some(name.into()),
            agent_invocable: true,
            channels: vec![],
            argument_hint: None,
            prompt_template: "# Body\n".into(),
            allowed_tools: vec![],
            source: ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            requirements: SkillRequirements::default(),
            token_budget_hint: 0,
            source_path: Some(dir.to_path_buf()),
            linked_files: linked_files::enumerate(dir).unwrap_or_default(),
        }
    }

    fn mk_ctx() -> ToolContext {
        ToolContext {
            session_id: "s".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: std::time::Duration::from_secs(10),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            approval: Some(ApprovalHandle::new(
                Arc::new(AutoDenyGate),
                Arc::new(Mutex::new(Vec::new())),
                None,
            )),
            ..ToolContext::for_test()
        }
    }

    #[tokio::test]
    async fn mode1_returns_skillmd_and_linked_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "# Body\n").unwrap();
        fs::create_dir(root.join("references")).unwrap();
        fs::write(root.join("references/a.md"), "ref a").unwrap();
        fs::create_dir(root.join("templates")).unwrap();
        fs::write(root.join("templates/cfg.yaml"), "x: 1").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/s.sh"), "#!/bin/sh\n").unwrap();
        fs::write(root.join("misc.txt"), "stray").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(root, "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let out = tool
            .execute(json!({"skill": "demo"}), &mk_ctx())
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["name"], "demo");
        assert_eq!(v["content"], "# Body\n");
        let lf = &v["linked_files"];
        assert_eq!(lf["references"], json!(["references/a.md"]));
        assert_eq!(lf["templates"], json!(["templates/cfg.yaml"]));
        assert_eq!(lf["scripts"], json!(["scripts/s.sh"]));
        assert_eq!(lf["other"], json!(["misc.txt"]));
    }

    /// A `channels:`-restricted skill refuses the `Skill` tool from a
    /// session on any other channel (the ctx fixture runs on `tui`),
    /// and loads normally when the restriction admits the channel.
    #[tokio::test]
    async fn channel_restricted_skill_refuses_other_channels() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill(dir.path(), "demo");
        skill.channels = vec![baybo_model::ChannelType::owner()];
        registry.register(skill);
        let tool = SkillTool {
            registry: Arc::clone(&registry),
            risk_check: Arc::new(AlwaysPass),
        };

        let err = tool
            .execute(json!({"skill": "demo"}), &mk_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "got {err:?}");

        let mut owner_ctx = mk_ctx();
        owner_ctx.user.channel = ChannelType::owner();
        tool.execute(json!({"skill": "demo"}), &owner_ctx)
            .await
            .expect("owner channel loads the skill");
    }

    /// The slash route must surface the same sub-file affordance the LLM
    /// `Skill` tool advertises: body first, then an inventory naming the
    /// tool, the skill, and every linked path the model can fetch.
    #[test]
    fn render_skill_for_slash_appends_linked_file_inventory() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "# Body\n").unwrap();
        fs::create_dir(root.join("references")).unwrap();
        fs::write(root.join("references/a.md"), "ref a").unwrap();
        fs::create_dir(root.join("scripts")).unwrap();
        fs::write(root.join("scripts/s.sh"), "#!/bin/sh\n").unwrap();

        let out = render_skill_for_slash(&mk_skill(root, "demo"), "sess-1");

        assert!(out.starts_with("# Body\n"), "body comes first: {out}");
        assert!(out.contains(SKILL_TOOL_NAME), "names the tool to call");
        assert!(
            out.contains("\"demo\""),
            "names the skill for the fetch call"
        );
        assert!(out.contains("- references/a.md"));
        assert!(out.contains("- scripts/s.sh"));
    }

    /// A single-file skill gets no inventory — just the body, byte-for-byte.
    #[test]
    fn render_skill_for_slash_body_only_when_no_linked_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "# Body\n").unwrap();

        let mut skill = mk_skill(root, "demo");
        skill.prompt_template = "BODY ONLY".into();
        assert_eq!(render_skill_for_slash(&skill, "sess-1"), "BODY ONLY");
    }

    #[tokio::test]
    async fn session_id_token_is_substituted_in_rendered_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "current session is {{session_id}}\n").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill(root, "session-aware");
        skill.prompt_template = "current session is {{session_id}}\n".into();
        registry.register(skill);
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let ctx = mk_ctx();
        let out = tool
            .execute(json!({"skill": "session-aware"}), &ctx)
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["content"], format!("current session is {}\n", "s"));
        assert!(
            !v["content"].as_str().unwrap().contains(SESSION_ID_TOKEN),
            "raw placeholder must not survive substitution: {}",
            v["content"]
        );
    }

    #[tokio::test]
    async fn mode2_returns_subfile_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "# Body\n").unwrap();
        fs::create_dir(root.join("references")).unwrap();
        fs::write(root.join("references/a.md"), "hello").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(root, "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let out = tool
            .execute(
                json!({"skill": "demo", "file_path": "references/a.md"}),
                &mk_ctx(),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["file"], "references/a.md");
        assert_eq!(v["content"], "hello");
        assert_eq!(v["file_type"], ".md");
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "# Body\n").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(root, "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        for bad in ["../etc/passwd", "/etc/passwd", "a/../../b"] {
            let err = tool
                .execute(json!({"skill": "demo", "file_path": bad}), &mk_ctx())
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidParams(_)), "{bad}: {err:?}");
        }
    }

    #[tokio::test]
    async fn blank_optional_fields_are_treated_as_unset() {
        // Strict-JSON-schema LLMs (e.g. gpt-5.5) sometimes emit every
        // property they have seen, including `"file_path": ""` and
        // `"args": ""` instead of omitting the keys. Both must transparently
        // fall back to "not provided" — otherwise file_path reaches
        // resolve_subpath and traps the model in a retry loop.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("SKILL.md"), "# Body\n").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(root, "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        for blank in ["", "   ", "\t"] {
            let out = tool
                .execute(
                    json!({"skill": "demo", "file_path": blank, "args": blank}),
                    &mk_ctx(),
                )
                .await
                .unwrap_or_else(|e| panic!("blank optionals {blank:?} should succeed: {e:?}"));
            let v = match out {
                ToolOutput::Json(v) => v,
                other => panic!("expected json, got {other:?}"),
            };
            assert_eq!(v["content"], "# Body\n", "blank file_path {blank:?}");
            assert!(
                v.get("args").is_none(),
                "blank args {blank:?} must not surface in output: {v}"
            );
        }
    }

    #[tokio::test]
    async fn agent_invocable_false_returns_not_found() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        let mut s = mk_skill(dir.path(), "demo");
        s.agent_invocable = false;

        let registry = Arc::new(SkillRegistry::new());
        registry.register(s);
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let err = tool
            .execute(json!({"skill": "demo"}), &mk_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn untrusted_returns_not_found() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        let mut s = mk_skill(dir.path(), "demo");
        s.trust_level = TrustLevel::Untrusted;

        let registry = Arc::new(SkillRegistry::new());
        registry.register(s);
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let err = tool
            .execute(json!({"skill": "demo"}), &mk_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn missing_env_short_circuits_before_approval() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        let mut s = mk_skill(dir.path(), "demo");
        // Pick a name that would never be set in CI.
        s.requirements.required_env = vec!["BAYBO_TEST_DEFINITELY_UNSET_VAR".into()];

        let registry = Arc::new(SkillRegistry::new());
        registry.register(s);
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let err = tool
            .execute(json!({"skill": "demo"}), &mk_ctx())
            .await
            .unwrap_err();
        match err {
            ToolError::Execution(msg) => {
                assert!(msg.contains("BAYBO_TEST_DEFINITELY_UNSET_VAR"));
            }
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn args_propagated_into_response() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(dir.path(), "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let out = tool
            .execute(
                json!({"skill": "demo", "args": "rollback to v3"}),
                &mk_ctx(),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["args"], "rollback to v3");
    }

    #[test]
    fn manifest_is_trusted_with_no_capabilities() {
        let registry = Arc::new(SkillRegistry::new());
        let (tool, manifest) = build(registry, Arc::new(AlwaysPass));
        assert_eq!(tool.name(), SKILL_TOOL_NAME);
        assert!(matches!(manifest.trust_level, TrustLevel::Trusted));
        assert!(manifest.capabilities.is_empty());
    }

    #[test]
    fn block_verdict_does_not_panic_when_notifier_absent() {
        // Sanity: the assess() Block branch is reached and the
        // notifier-None path falls through without panicking.
        struct AlwaysBlock;
        #[async_trait]
        impl SkillRiskCheck for AlwaysBlock {
            async fn assess(&self, _: &SkillDefinition) -> SkillGate {
                SkillGate::Block {
                    rationale: "test rationale".into(),
                }
            }
        }

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(dir.path(), "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysBlock),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(tool.execute(json!({"skill": "demo"}), &mk_ctx()))
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
    }

    fn write_minimal_skill(dir: &Path, name: &str) {
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: t\n---\nbody\n"),
        )
        .unwrap();
    }

    /// A skill directory under `root`, ready for a scan to find.
    fn write_installed_skill(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        write_minimal_skill(&dir, name);
        dir
    }

    fn workspace_paths(root: &Path) -> baybo_workspace::WorkspacePaths {
        baybo_workspace::WorkspacePaths::new(root.to_path_buf())
    }

    fn builtin() -> baybo_model::AgentProfileId {
        baybo_model::AgentProfileId::builtin()
    }

    /// The directory a default (built-in / unbound) session installs into.
    fn builtin_skills_dir(root: &Path) -> PathBuf {
        builtin().skills_dir(&workspace_paths(root))
    }

    /// A tool context anchored at a real workspace root and running as
    /// `agent` — which is what decides the tree install/uninstall address.
    fn ctx_at(root: &Path, agent: &baybo_model::AgentProfileId) -> ToolContext {
        ToolContext {
            workspace_paths: workspace_paths(root),
            agent_id: agent.clone(),
            ..mk_ctx()
        }
    }

    fn custom_agent() -> baybo_model::AgentProfileId {
        baybo_model::AgentProfileId::parse("01JCUSTOM").expect("valid id")
    }

    fn install_tool(
        registry: Arc<SkillRegistry>,
        risk: Arc<dyn SkillRiskCheck>,
    ) -> SkillInstallTool {
        SkillInstallTool {
            registry,
            risk_check: risk,
        }
    }

    #[tokio::test]
    async fn install_copies_files_and_reloads() {
        let workspace = tempdir().unwrap();
        let skills_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "axolotl");
        fs::create_dir(source.path().join("references")).unwrap();
        fs::write(source.path().join("references/a.md"), "ref").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.ensure_agent_skills(&builtin(), &workspace_paths(workspace.path()));
        let tool = install_tool(Arc::clone(&registry), Arc::new(AlwaysPass));

        let out = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["name"], "axolotl");
        assert_eq!(v["skills_in_scope"], 1);
        assert!(skills_dir.join("axolotl/SKILL.md").exists());
        assert!(skills_dir.join("axolotl/references/a.md").exists());
        // Reachable from the default scope by either spelling — an unbound
        // session resolves to the same directory the built-in reads.
        assert!(registry.get_scoped(Some(&builtin()), "axolotl").is_some());
        assert!(registry.get_scoped(None, "axolotl").is_some());
        // Invisible to anyone else.
        assert!(
            registry
                .get_scoped(Some(&custom_agent()), "axolotl")
                .is_none()
        );
    }

    #[tokio::test]
    async fn install_refuses_overwrite() {
        let workspace = tempdir().unwrap();
        let skills_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&skills_dir).unwrap();
        write_installed_skill(&skills_dir, "axolotl");

        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "axolotl");

        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(registry, Arc::new(AlwaysPass));

        let err = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(msg) if msg.contains("already installed")),
            "expected already-installed error",
        );
    }

    #[tokio::test]
    async fn install_with_suspicious_verdict_succeeds_with_warning() {
        struct AlwaysSuspicious;
        #[async_trait]
        impl SkillRiskCheck for AlwaysSuspicious {
            async fn assess(&self, _: &SkillDefinition) -> SkillGate {
                SkillGate::PassWithWarning {
                    rationale: "minor concern".into(),
                }
            }
        }
        let workspace = tempdir().unwrap();
        let skills_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "demo");

        let registry = Arc::new(SkillRegistry::new());
        registry.ensure_agent_skills(&builtin(), &workspace_paths(workspace.path()));
        let tool = install_tool(Arc::clone(&registry), Arc::new(AlwaysSuspicious));

        let out = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["risk_warning"], "minor concern");
        assert!(skills_dir.join("demo/SKILL.md").exists());
    }

    #[tokio::test]
    async fn mode2_rejects_symlink_escape() {
        let secrets = tempdir().unwrap();
        fs::write(secrets.path().join("victim.txt"), "leaked").unwrap();

        let dir = tempdir().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# Body\n").unwrap();
        fs::create_dir(dir.path().join("references")).unwrap();
        std::os::unix::fs::symlink(
            secrets.path().join("victim.txt"),
            dir.path().join("references/escape"),
        )
        .unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(dir.path(), "demo"));
        let tool = SkillTool {
            registry,
            risk_check: Arc::new(AlwaysPass),
        };

        let err = tool
            .execute(
                json!({"skill": "demo", "file_path": "references/escape"}),
                &mk_ctx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(msg) if msg.contains("outside")),
            "expected outside-skill-directory error",
        );
    }

    #[tokio::test]
    async fn install_blocks_on_dangerous_verdict() {
        struct AlwaysBlock;
        #[async_trait]
        impl SkillRiskCheck for AlwaysBlock {
            async fn assess(&self, _: &SkillDefinition) -> SkillGate {
                SkillGate::Block {
                    rationale: "smells like exfiltration".into(),
                }
            }
        }

        let workspace = tempdir().unwrap();
        let skills_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "demo");

        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(registry, Arc::new(AlwaysBlock));

        let err = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
        assert!(!skills_dir.join("demo").exists());
    }

    #[tokio::test]
    async fn install_invalid_source_returns_invalid_params() {
        let workspace = tempdir().unwrap();
        let source = tempdir().unwrap();
        // No SKILL.md in source.
        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(registry, Arc::new(AlwaysPass));

        let err = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn leaked_staging_dir_is_not_loaded_by_registry() {
        // Simulates a crash mid-install: a fully-copied skill tree
        // sits under `<skills_dir>/.staging/<uuid>/` because the
        // rename never happened. `SkillRegistry::reload()` must NOT
        // pick it up (only depth-1 children of skills_dir get
        // scanned for SKILL.md).
        let workspace = tempdir().unwrap();
        let skills_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&skills_dir).unwrap();
        let staging = skills_dir.join(".staging").join("abc-123");
        fs::create_dir_all(&staging).unwrap();
        write_minimal_skill(&staging, "phantom");

        let registry = Arc::new(SkillRegistry::new());
        let loaded = registry.ensure_agent_skills(&builtin(), &workspace_paths(workspace.path()));
        assert_eq!(loaded, 0, "leaked staging tree must not register");
        assert!(registry.get_scoped(Some(&builtin()), "phantom").is_none());
    }

    /// "Already inside" is measured against the tree the *caller* installs
    /// into, so it catches the mistake for every scope — the default one and
    /// a custom agent's alike.
    #[tokio::test]
    async fn install_rejects_source_inside_the_callers_own_skills_dir() {
        for agent in [builtin(), custom_agent()] {
            let workspace = tempdir().unwrap();
            let ctx = ctx_at(workspace.path(), &agent);
            // Through the production seam, so the test cannot drift from the
            // rule it is checking.
            let root = scope_skills_dir(&ctx);
            fs::create_dir_all(&root).unwrap();
            let source = write_installed_skill(&root, "foo");

            let registry = Arc::new(SkillRegistry::new());
            let tool = install_tool(registry, Arc::new(AlwaysPass));

            let err = tool
                .execute(json!({"source_dir": source.to_str().unwrap()}), &ctx)
                .await
                .unwrap_err();
            assert!(
                matches!(err, ToolError::InvalidParams(_)),
                "{agent}: {err:?}"
            );
        }
    }

    /// A custom agent installs into its own folder and nowhere else: the
    /// skill is in that agent's scope and invisible to every other session,
    /// the default one included.
    #[tokio::test]
    async fn install_lands_in_the_calling_agents_own_folder() {
        let workspace = tempdir().unwrap();
        let paths = workspace_paths(workspace.path());
        let agent = custom_agent();
        let default_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&default_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "private-recipe");

        let registry = Arc::new(SkillRegistry::new());
        registry.ensure_agent_skills(&builtin(), &paths);
        let tool = install_tool(Arc::clone(&registry), Arc::new(AlwaysPass));

        let out = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &ctx_at(workspace.path(), &agent),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };

        let own = agent.skills_dir(&paths);
        assert_eq!(
            v["installed_at"],
            path_to_string(&own.join("private-recipe"))
        );
        assert!(own.join("private-recipe/SKILL.md").is_file());
        assert!(
            !default_dir.join("private-recipe").exists(),
            "an agent's install must not touch another scope's directory"
        );

        // The installing agent sees it…
        assert!(
            registry
                .get_scoped(Some(&agent), "private-recipe")
                .is_some()
        );
        assert_eq!(v["skills_in_scope"], 1);
        // …and nobody else does, the default scope included.
        assert!(registry.get_scoped(None, "private-recipe").is_none());
        assert!(
            registry
                .get_scoped(Some(&builtin()), "private-recipe")
                .is_none()
        );
        assert!(registry.get_scoped(None, "private-recipe").is_none());
        assert!(registry.summaries_for(None).is_empty());
    }

    /// The reload trap the scoped install has to clear: an agent whose
    /// folder did not exist yet is absent from the registry's replay list,
    /// and `reload` replays exactly that list. Registering the freshly
    /// created folder is what keeps the skill alive past the next reload —
    /// without it the install succeeds and then silently evaporates.
    #[tokio::test]
    async fn install_into_a_brand_new_folder_survives_a_later_reload() {
        let workspace = tempdir().unwrap();
        let paths = workspace_paths(workspace.path());
        let agent = custom_agent();
        let overlay = paths.persona_skills_dir(agent.as_str());
        assert!(!overlay.exists(), "the folder must be absent to start");

        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "late-bloomer");

        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(Arc::clone(&registry), Arc::new(AlwaysPass));
        tool.execute(
            json!({"source_dir": source.path().to_str().unwrap()}),
            &ctx_at(workspace.path(), &agent),
        )
        .await
        .unwrap();

        assert!(registry.get_scoped(Some(&agent), "late-bloomer").is_some());
        registry.reload();
        assert!(
            registry.get_scoped(Some(&agent), "late-bloomer").is_some(),
            "the freshly created folder was never registered, so reload dropped it"
        );
    }

    fn uninstall_tool(registry: Arc<SkillRegistry>) -> SkillUninstallTool {
        SkillUninstallTool { registry }
    }

    #[tokio::test]
    async fn uninstall_removes_dir_and_reloads_registry() {
        let workspace = tempdir().unwrap();
        let skills_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&skills_dir).unwrap();
        let skill_dir = write_installed_skill(&skills_dir, "demo");
        fs::create_dir(skill_dir.join("references")).unwrap();
        fs::write(skill_dir.join("references/a.md"), "x").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        let loaded = registry.ensure_agent_skills(&builtin(), &workspace_paths(workspace.path()));
        assert_eq!(loaded, 1);
        assert!(registry.get_scoped(Some(&builtin()), "demo").is_some());

        let tool = uninstall_tool(Arc::clone(&registry));
        let out = tool
            .execute(
                json!({"name": "demo"}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["name"], "demo");
        assert_eq!(v["skills_in_scope"], 0);
        assert!(!skill_dir.exists());
        assert!(registry.get_scoped(Some(&builtin()), "demo").is_none());
    }

    #[tokio::test]
    async fn uninstall_unknown_skill_returns_not_found() {
        let workspace = tempdir().unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let tool = uninstall_tool(registry);

        let err = tool
            .execute(
                json!({"name": "ghost"}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn uninstall_refuses_skill_outside_workspace() {
        // A skill registered from outside every agent's directory — a
        // third-party mount — must not be deletable through this tool: the
        // LLM is not allowed to roam the host filesystem.
        let workspace = tempdir().unwrap();

        let other = tempdir().unwrap();
        let outside = write_installed_skill(other.path(), "rogue");

        // Registered directly, as a third-party mount would be: on disk, but
        // outside every agent's directory.
        let registry = Arc::new(SkillRegistry::new());
        registry.register(mk_skill(&outside, "rogue"));
        assert!(registry.get_scoped(Some(&builtin()), "rogue").is_some());

        let tool = uninstall_tool(registry);
        let err = tool
            .execute(
                json!({"name": "rogue"}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
        assert!(outside.exists());
    }

    #[tokio::test]
    async fn uninstall_refuses_inline_skill() {
        // A registered skill with no source_path (e.g. directly
        // injected for tests) has nothing on disk to delete.
        let workspace = tempdir().unwrap();

        let mut def = mk_skill(workspace.path(), "inline");
        def.source_path = None;
        let registry = Arc::new(SkillRegistry::new());
        registry.register(def);

        let tool = uninstall_tool(registry);
        let err = tool
            .execute(
                json!({"name": "inline"}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    /// The defect this scoping exists to close: a custom agent could name a
    /// skill it cannot see and delete it out from under the session that
    /// owns it. The lookup is scoped now, so the name simply misses — and the
    /// miss is an ordinary "not found", never a refusal that would confirm
    /// the skill is there.
    #[tokio::test]
    async fn a_custom_agent_cannot_uninstall_the_default_scopes_skill() {
        let workspace = tempdir().unwrap();
        let paths = workspace_paths(workspace.path());
        let default_dir = builtin_skills_dir(workspace.path());
        fs::create_dir_all(&default_dir).unwrap();
        let theirs = write_installed_skill(&default_dir, "everyones");

        let registry = Arc::new(SkillRegistry::new());
        assert_eq!(registry.ensure_agent_skills(&builtin(), &paths), 1);

        let tool = uninstall_tool(Arc::clone(&registry));
        let err = tool
            .execute(
                json!({"name": "everyones"}),
                &ctx_at(workspace.path(), &custom_agent()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "got {err:?}");
        assert!(theirs.exists());
        assert!(registry.get_scoped(Some(&builtin()), "everyones").is_some());
    }

    /// Seeing a skill and owning it are different things. A custom agent
    /// reaches a `UNIVERSAL_SKILLS` entry through the compiled-in builtins —
    /// so the lookup succeeds — and must still not be able to remove it. A
    /// compiled-in skill has no directory to delete, which is what the
    /// refusal says.
    #[tokio::test]
    async fn a_custom_agent_cannot_uninstall_the_universal_skill_it_can_see() {
        let workspace = tempdir().unwrap();
        let paths = workspace_paths(workspace.path());
        let agent = custom_agent();
        fs::create_dir_all(agent.skills_dir(&paths)).unwrap();
        let universal = crate::registry::UNIVERSAL_SKILLS[0];

        let registry = Arc::new(SkillRegistry::new());
        assert!(registry.register_builtins() > 0);
        assert!(
            registry.get_scoped(Some(&agent), universal).is_some(),
            "precondition: the agent can see it"
        );

        let tool = uninstall_tool(Arc::clone(&registry));
        let err = tool
            .execute(
                json!({"name": universal}),
                &ctx_at(workspace.path(), &agent),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "got {err:?}");
        assert!(
            registry.get_scoped(Some(&agent), universal).is_some(),
            "still there"
        );
    }

    /// The mirror image: an unbound session is the built-in, whose scope is
    /// its own directory, so another agent's private skill is not addressable
    /// from it at all.
    #[tokio::test]
    async fn the_builtin_cannot_uninstall_an_agents_private_skill() {
        let workspace = tempdir().unwrap();
        let paths = workspace_paths(workspace.path());
        let agent = custom_agent();
        let overlay = paths.persona_skills_dir(agent.as_str());
        fs::create_dir_all(&overlay).unwrap();
        let private = write_installed_skill(&overlay, "private-recipe");

        let registry = Arc::new(SkillRegistry::new());
        assert_eq!(registry.ensure_agent_skills(&agent, &paths), 1);

        let tool = uninstall_tool(Arc::clone(&registry));
        let err = tool
            .execute(
                json!({"name": "private-recipe"}),
                &ctx_at(workspace.path(), &builtin()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)), "got {err:?}");
        assert!(private.exists());

        // …while its owner removes it normally.
        tool.execute(
            json!({"name": "private-recipe"}),
            &ctx_at(workspace.path(), &agent),
        )
        .await
        .unwrap();
        assert!(!private.exists());
        assert!(
            registry
                .get_scoped(Some(&agent), "private-recipe")
                .is_none()
        );
    }
}
