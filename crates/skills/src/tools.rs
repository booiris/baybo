//! `Skill` tool — the LLM-facing entry point for declarative skills.
//!
//! Env-var values are never templated into the response: skill bodies
//! are untrusted prompt content and the LLM context is not the right
//! exfiltration boundary for secrets. The body is expected to
//! instruct downstream tool calls on how to read them in-process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use aura_model::TrustLevel;
use aura_tools::{
    ApprovalDecision, NoticeLevel, ResourceAccess as ToolResourceAccess, Tool, ToolCapability,
    ToolContext, ToolError, ToolManifest, ToolOutput,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{SkillDefinition, SkillGate, SkillRegistry, SkillRiskCheck};

const MAX_SUBFILE_BYTES: u64 = 256 * 1024;
const MAX_ARGS_BYTES: usize = 4 * 1024;

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
        description: tool.description().to_string(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
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
        "Skill"
    }

    fn description(&self) -> &str {
        "Load a registered skill so its instructions enter the conversation. \
         Available skills are listed in a system reminder each turn — invoke \
         this tool with `skill: \"<name>\"` to pull one in. Pass `args` to \
         forward free-form arguments. Pass `file_path` to fetch a sub-file \
         (relative path inside the skill's directory) referenced from the \
         main SKILL.md. Skills the operator marked untrusted or marked \
         `disable-model-invocation: true` are not callable here."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill": {
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
            "required": ["skill"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: SkillParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        if let Some(args) = &p.args
            && args.len() > MAX_ARGS_BYTES
        {
            return Err(ToolError::InvalidParams(format!(
                "`args` exceeds {} bytes",
                MAX_ARGS_BYTES
            )));
        }

        let skill = self
            .registry
            .get(&p.skill)
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
            None => render_main(&skill, p.args.as_deref())?,
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
) -> aura_tools::Result<()> {
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
    let decision = approval
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
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => Ok(()),
        ApprovalDecision::Deny => Err(ToolError::Denied {
            tool: tool_name.to_string(),
            reason: format!(
                "user declined env-var access required by skill '{}'",
                skill.name
            ),
        }),
    }
}

fn render_main(skill: &SkillDefinition, args: Option<&str>) -> aura_tools::Result<ToolOutput> {
    let dir = skill.source_path.as_deref();
    let path = dir.map(|d| d.join("SKILL.md"));

    let mut out = json!({
        "name": skill.name,
        "version": skill.version,
        "description": skill.description,
        "content": skill.prompt_template,
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
) -> aura_tools::Result<ToolOutput> {
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

pub fn build_install_tool(
    workspace_skills_dir: PathBuf,
    registry: Arc<SkillRegistry>,
    risk_check: Arc<dyn SkillRiskCheck>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let tool: Arc<dyn Tool> = Arc::new(SkillInstallTool {
        workspace_skills_dir,
        registry,
        risk_check,
    });
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![ToolCapability::WriteFile],
    };
    (tool, manifest)
}

struct SkillInstallTool {
    workspace_skills_dir: PathBuf,
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

    fn description(&self) -> &str {
        "Install a skill from an on-disk directory into the workspace's \
         skills folder. The directory must contain a valid SKILL.md; the \
         skill is run through the risk assessor (Dangerous verdicts \
         abort the install) and the registry is hot-reloaded so the new \
         skill is available on the next turn. Refuses to overwrite an \
         existing installation; the operator must remove the old copy \
         first."
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

    fn accessed_resources(&self, _params: &Value) -> Vec<ToolResourceAccess> {
        vec![ToolResourceAccess::WriteFile {
            path: self.workspace_skills_dir.clone(),
        }]
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
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

        // Reject before parsing if the source resolves into the
        // workspace skills directory — operator pointed at an
        // already-installed skill by mistake.
        if let Ok(canonical_skills_root) = tokio::fs::canonicalize(&self.workspace_skills_dir).await
            && source_dir.starts_with(&canonical_skills_root)
        {
            return Err(ToolError::InvalidParams(format!(
                "source_dir `{}` is already inside the workspace skills directory",
                source_dir.display()
            )));
        }

        let skill = crate::loader::load_skill_from_dir(&source_dir)
            .map_err(|e| ToolError::InvalidParams(format!("invalid skill source: {e}")))?;

        let dest_dir = self.workspace_skills_dir.join(&skill.name);
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

        // Stage at depth 2 (`<skills_dir>/.staging/<uuid>/`) rather
        // than alongside `<skills_dir>/<name>/`. `SkillRegistry::scan_dir`
        // only inspects depth-1 children for `SKILL.md`, so a leaked
        // staging tree (process killed between copy and rename) won't
        // be loaded as a phantom skill on the next reload.
        let staging_root = self.workspace_skills_dir.join(".staging");
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

        let reloaded = self.registry.reload();

        let mut out = json!({
            "name": skill.name,
            "version": skill.version,
            "installed_at": path_to_string(&dest_dir),
            "registry_loaded": reloaded,
            "linked_files": skill.linked_files.to_json(),
        });
        if let Some(w) = warning {
            out["risk_warning"] = Value::String(w);
        }
        Ok(ToolOutput::Json(out))
    }
}

pub fn build_uninstall_tool(
    workspace_skills_dir: PathBuf,
    registry: Arc<SkillRegistry>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let tool: Arc<dyn Tool> = Arc::new(SkillUninstallTool {
        workspace_skills_dir,
        registry,
    });
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![ToolCapability::WriteFile],
    };
    (tool, manifest)
}

struct SkillUninstallTool {
    workspace_skills_dir: PathBuf,
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

    fn description(&self) -> &str {
        "Remove an installed skill from the workspace's skills folder. \
         Refuses skills that aren't on disk under the workspace skills \
         dir (so registry-only or third-party-mounted skills aren't \
         accidentally deleted). The registry is hot-reloaded so the \
         skill disappears from the next turn's listing."
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

    fn accessed_resources(&self, _params: &Value) -> Vec<ToolResourceAccess> {
        vec![ToolResourceAccess::WriteFile {
            path: self.workspace_skills_dir.clone(),
        }]
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: UninstallParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let skill = self
            .registry
            .get(&p.name)
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
        let real_root = tokio::fs::canonicalize(&self.workspace_skills_dir)
            .await
            .map_err(|e| ToolError::Execution(format!("canonicalize skills dir: {e}")))?;
        if !real_source.starts_with(&real_root) {
            return Err(ToolError::Denied {
                tool: self.name().to_string(),
                reason: format!(
                    "skill '{}' lives outside the workspace skills directory; refusing to delete {}",
                    p.name,
                    real_source.display()
                ),
            });
        }
        // Refuse to recurse into the skills root itself if a skill
        // somehow registered with `source_path = <skills_dir>`.
        if real_source == real_root {
            return Err(ToolError::Denied {
                tool: self.name().to_string(),
                reason: "skill source_path equals the skills root; refusing to delete".into(),
            });
        }

        tokio::fs::remove_dir_all(&real_source)
            .await
            .map_err(|e| ToolError::Execution(format!("remove {}: {e}", real_source.display())))?;

        let reloaded = self.registry.reload();

        Ok(ToolOutput::Json(json!({
            "name": skill.name,
            "removed_from": path_to_string(&real_source),
            "registry_loaded": reloaded,
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

    use aura_model::{ArtifactSource, ChannelType, TrustLevel, User};
    use aura_tools::{ApprovalHandle, AutoDenyGate};
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
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            sandbox: None,
            approval: Some(ApprovalHandle::new(
                Arc::new(AutoDenyGate),
                Arc::new(Mutex::new(Vec::new())),
            )),
            notifier: None,
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

        for bad in ["../etc/passwd", "/etc/passwd", "a/../../b", ""] {
            let err = tool
                .execute(json!({"skill": "demo", "file_path": bad}), &mk_ctx())
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidParams(_)), "{bad}: {err:?}");
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
        s.requirements.required_env = vec!["AURA_TEST_DEFINITELY_UNSET_VAR".into()];

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
                assert!(msg.contains("AURA_TEST_DEFINITELY_UNSET_VAR"));
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
        assert_eq!(tool.name(), "Skill");
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

    fn install_tool(
        skills_dir: &Path,
        registry: Arc<SkillRegistry>,
        risk: Arc<dyn SkillRiskCheck>,
    ) -> SkillInstallTool {
        SkillInstallTool {
            workspace_skills_dir: skills_dir.to_path_buf(),
            registry,
            risk_check: risk,
        }
    }

    #[tokio::test]
    async fn install_copies_files_and_reloads() {
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "axolotl");
        fs::create_dir(source.path().join("references")).unwrap();
        fs::write(source.path().join("references/a.md"), "ref").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        registry.load_dir(&skills_dir);
        let tool = install_tool(&skills_dir, Arc::clone(&registry), Arc::new(AlwaysPass));

        let out = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &mk_ctx(),
            )
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["name"], "axolotl");
        assert!(skills_dir.join("axolotl/SKILL.md").exists());
        assert!(skills_dir.join("axolotl/references/a.md").exists());
        assert!(registry.get("axolotl").is_some());
    }

    #[tokio::test]
    async fn install_refuses_overwrite() {
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        fs::create_dir(skills_dir.join("axolotl")).unwrap();
        fs::write(skills_dir.join("axolotl/SKILL.md"), "x").unwrap();

        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "axolotl");

        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(&skills_dir, registry, Arc::new(AlwaysPass));

        let err = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &mk_ctx(),
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
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "demo");

        let registry = Arc::new(SkillRegistry::new());
        registry.load_dir(&skills_dir);
        let tool = install_tool(
            &skills_dir,
            Arc::clone(&registry),
            Arc::new(AlwaysSuspicious),
        );

        let out = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &mk_ctx(),
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
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        write_minimal_skill(source.path(), "demo");

        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(&skills_dir, registry, Arc::new(AlwaysBlock));

        let err = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &mk_ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
        assert!(!skills_dir.join("demo").exists());
    }

    #[tokio::test]
    async fn install_invalid_source_returns_invalid_params() {
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let source = tempdir().unwrap();
        // No SKILL.md in source.
        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(&skills_dir, registry, Arc::new(AlwaysPass));

        let err = tool
            .execute(
                json!({"source_dir": source.path().to_str().unwrap()}),
                &mk_ctx(),
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
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let staging = skills_dir.join(".staging").join("abc-123");
        fs::create_dir_all(&staging).unwrap();
        write_minimal_skill(&staging, "phantom");

        let registry = Arc::new(SkillRegistry::new());
        let loaded = registry.load_dir(&skills_dir);
        assert_eq!(loaded, 0, "leaked staging tree must not register");
        assert!(registry.get("phantom").is_none());
    }

    #[tokio::test]
    async fn install_rejects_source_inside_skills_dir() {
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let source = skills_dir.join("foo");
        fs::create_dir(&source).unwrap();
        write_minimal_skill(&source, "foo");

        let registry = Arc::new(SkillRegistry::new());
        let tool = install_tool(&skills_dir, registry, Arc::new(AlwaysPass));

        let err = tool
            .execute(json!({"source_dir": source.to_str().unwrap()}), &mk_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    fn uninstall_tool(skills_dir: &Path, registry: Arc<SkillRegistry>) -> SkillUninstallTool {
        SkillUninstallTool {
            workspace_skills_dir: skills_dir.to_path_buf(),
            registry,
        }
    }

    #[tokio::test]
    async fn uninstall_removes_dir_and_reloads_registry() {
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let skill_dir = skills_dir.join("demo");
        fs::create_dir(&skill_dir).unwrap();
        write_minimal_skill(&skill_dir, "demo");
        fs::create_dir(skill_dir.join("references")).unwrap();
        fs::write(skill_dir.join("references/a.md"), "x").unwrap();

        let registry = Arc::new(SkillRegistry::new());
        let loaded = registry.load_dir(&skills_dir);
        assert_eq!(loaded, 1);
        assert!(registry.get("demo").is_some());

        let tool = uninstall_tool(&skills_dir, Arc::clone(&registry));
        let out = tool
            .execute(json!({"name": "demo"}), &mk_ctx())
            .await
            .unwrap();
        let v = match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["name"], "demo");
        assert_eq!(v["registry_loaded"], 0);
        assert!(!skill_dir.exists());
        assert!(registry.get("demo").is_none());
    }

    #[tokio::test]
    async fn uninstall_unknown_skill_returns_not_found() {
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();
        let registry = Arc::new(SkillRegistry::new());
        let tool = uninstall_tool(&skills_dir, registry);

        let err = tool
            .execute(json!({"name": "ghost"}), &mk_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[tokio::test]
    async fn uninstall_refuses_skill_outside_workspace() {
        // Skill loaded from a directory outside the workspace skills
        // root must not be deletable through this tool — the LLM is
        // not allowed to roam the host filesystem.
        let workspace = tempdir().unwrap();
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();

        let other = tempdir().unwrap();
        let outside = other.path().join("rogue");
        fs::create_dir(&outside).unwrap();
        write_minimal_skill(&outside, "rogue");

        let registry = Arc::new(SkillRegistry::new());
        registry.load_dir(other.path());
        assert!(registry.get("rogue").is_some());

        let tool = uninstall_tool(&skills_dir, registry);
        let err = tool
            .execute(json!({"name": "rogue"}), &mk_ctx())
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
        let skills_dir = workspace.path().join("skills");
        fs::create_dir(&skills_dir).unwrap();

        let mut def = mk_skill(workspace.path(), "inline");
        def.source_path = None;
        let registry = Arc::new(SkillRegistry::new());
        registry.register(def);

        let tool = uninstall_tool(&skills_dir, registry);
        let err = tool
            .execute(json!({"name": "inline"}), &mk_ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }
}
