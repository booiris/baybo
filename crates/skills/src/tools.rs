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
    ApprovalDecision, NoticeLevel, ResourceAccess as ToolResourceAccess, Tool, ToolContext,
    ToolError, ToolManifest, ToolOutput,
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
    let tool: Arc<dyn Tool> = Arc::new(SkillTool::new(registry, risk_check));
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
    };
    (tool, manifest)
}

/// Tool implementation. See module-level docs for the contract.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
    risk_check: Arc<dyn SkillRiskCheck>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>, risk_check: Arc<dyn SkillRiskCheck>) -> Self {
        Self {
            registry,
            risk_check,
        }
    }
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
        if let (Some(warn), ToolOutput::Json(v)) = (warning.as_deref(), &mut out) {
            v["risk_warning"] = serde_json::Value::String(warn.to_string());
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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysPass));

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
        let tool = SkillTool::new(registry, Arc::new(AlwaysBlock));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(tool.execute(json!({"skill": "demo"}), &mk_ctx()))
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
    }
}
