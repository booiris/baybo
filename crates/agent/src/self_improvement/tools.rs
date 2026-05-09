//! SelfImprovement tool ceiling — the four tools the side-channel agent
//! invokes to read existing memory/skill state and to write new
//! entries. Constructed only via [`self_improvement_tools`] and intended
//! for registration with a *separate* `ToolRegistry` that the
//! self_improvement `AgentLoop` runs against. They MUST NOT be added to a
//! user-facing agent's `allowed_tools` — the empty
//! `accessed_resources()` impls bypass the approval gate, which is
//! safe only because of this registration isolation. See
//! `docs/modules/self-improvement.md` Q7.

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{MemoryCategory, MemoryEntry};
use aura_skills::SkillRegistry;
use aura_tools::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::memory::{MemoryManager, StoreOutcome};

/// Build all four self_improvement tools wrapped with their manifests.
/// `Trusted` trust level — they operate on agent-internal state
/// (memory store, workspace `skills/auto/`) and never touch the user's
/// general workspace, network, or shell.
pub fn self_improvement_tools(
    memory: Arc<MemoryManager>,
    skills: Arc<SkillRegistry>,
) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(MemoryWriteTool::new(Arc::clone(&memory))),
        Arc::new(MemoryListTool::new(memory)),
        Arc::new(SkillCreateTool::new(Arc::clone(&skills))),
        Arc::new(SkillListTool::new(skills)),
    ];
    tools.into_iter().map(with_manifest).collect()
}

fn with_manifest(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: aura_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
    };
    (tool, manifest)
}

// ── MemoryWrite ──────────────────────────────────────────────────────

struct MemoryWriteTool {
    memory: Arc<MemoryManager>,
}

impl MemoryWriteTool {
    fn new(memory: Arc<MemoryManager>) -> Self {
        Self { memory }
    }
}

#[derive(Debug, Deserialize)]
struct MemoryWriteParams {
    user_id: String,
    category: MemoryCategoryParam,
    content: String,
    #[serde(default = "default_importance")]
    importance: f32,
}

fn default_importance() -> f32 {
    0.6
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum MemoryCategoryParam {
    User,
    Feedback,
    Project,
    Reference,
}

impl From<MemoryCategoryParam> for MemoryCategory {
    fn from(c: MemoryCategoryParam) -> Self {
        match c {
            MemoryCategoryParam::User => Self::User,
            MemoryCategoryParam::Feedback => Self::Feedback,
            MemoryCategoryParam::Project => Self::Project,
            MemoryCategoryParam::Reference => Self::Reference,
        }
    }
}

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "MemoryWrite"
    }

    fn description(&self) -> &str {
        "Write a new long-term memory entry for the originating user. \
         Used by the self_improvement flow to persist a durable observation. \
         Returns `outcome: stored` on success or `outcome: deduplicated` \
         if a near-identical entry already exists (the write was \
         silently suppressed). Categories: \
         `user` (facts/preferences about the person), \
         `feedback` (corrections + validations — body should carry `Why:` \
         and `How to apply:` lines), \
         `project` (in-flight work specific to the current project — \
         body should carry `Why:` and `How to apply:` lines), \
         `reference` (pointers to external systems). \
         `importance` ∈ [0.0, 1.0]; defaults to 0.6."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id":  { "type": "string", "minLength": 1 },
                "category": { "type": "string", "enum": ["user", "feedback", "project", "reference"] },
                "content":  { "type": "string", "minLength": 1 },
                "importance": { "type": "number", "minimum": 0.0, "maximum": 1.0 }
            },
            "required": ["user_id", "category", "content"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: MemoryWriteParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let entry = MemoryEntry::new(p.user_id, p.content, p.category.into(), p.importance);
        let entry_id = entry.id.clone();
        let outcome = self
            .memory
            .store_or_dedup(entry)
            .await
            .map_err(|e| ToolError::Execution(format!("memory store: {e}")))?;
        let json_out = match outcome {
            StoreOutcome::Stored => json!({
                "outcome": "stored",
                "entry_id": entry_id,
            }),
            StoreOutcome::Deduplicated { against_id } => json!({
                "outcome": "deduplicated",
                "against_id": against_id,
            }),
        };
        Ok(ToolOutput::Json(json_out))
    }
}

// ── MemoryList ───────────────────────────────────────────────────────

struct MemoryListTool {
    memory: Arc<MemoryManager>,
}

impl MemoryListTool {
    fn new(memory: Arc<MemoryManager>) -> Self {
        Self { memory }
    }
}

#[derive(Debug, Deserialize)]
struct MemoryListParams {
    user_id: String,
    #[serde(default = "default_list_limit")]
    limit: usize,
}

fn default_list_limit() -> usize {
    100
}

#[async_trait]
impl Tool for MemoryListTool {
    fn name(&self) -> &str {
        "MemoryList"
    }

    fn description(&self) -> &str {
        "List existing long-term memory entries for the given user. \
         Returns up to `limit` entries (default 100), each with id, \
         category, importance, and content. Used by the self_improvement \
         flow to dedupe candidate writes against existing state."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "user_id": { "type": "string", "minLength": 1 },
                "limit":   { "type": "integer", "minimum": 1, "maximum": 500 }
            },
            "required": ["user_id"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: MemoryListParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let entries = self
            .memory
            .list(Some(&p.user_id))
            .await
            .map_err(|e| ToolError::Execution(format!("memory list: {e}")))?;
        let limited: Vec<Value> = entries
            .into_iter()
            .take(p.limit)
            .map(|e| {
                json!({
                    "id": e.id,
                    "category": category_label(&e.category),
                    "importance": e.importance,
                    "content": e.content,
                })
            })
            .collect();
        Ok(ToolOutput::Json(json!({ "entries": limited })))
    }
}

fn category_label(c: &MemoryCategory) -> &'static str {
    match c {
        MemoryCategory::User => "user",
        MemoryCategory::Feedback => "feedback",
        MemoryCategory::Project => "project",
        MemoryCategory::Reference => "reference",
    }
}

// ── SkillCreate ──────────────────────────────────────────────────────

struct SkillCreateTool {
    skills: Arc<SkillRegistry>,
}

impl SkillCreateTool {
    fn new(skills: Arc<SkillRegistry>) -> Self {
        Self { skills }
    }
}

#[derive(Debug, Deserialize)]
struct SkillCreateParams {
    name: String,
    description: String,
    body: String,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    when_to_use: Option<String>,
    #[serde(default)]
    argument_hint: Option<String>,
}

const SKILL_NAME_RE: &str = r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$";

#[async_trait]
impl Tool for SkillCreateTool {
    fn name(&self) -> &str {
        "SkillCreate"
    }

    fn description(&self) -> &str {
        "Create a new skill under <workspace>/skills/auto/<name>/SKILL.md. \
         Auto-generated skills are recorded with `disable-model-invocation: true` \
         (LLM cannot auto-select; user must explicitly invoke `/<name>` after \
         operator approval). The optional `allowed_tools` list is written \
         verbatim into the SKILL.md frontmatter — the operator approves the \
         skill before it can do anything. \
         `name` must match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` and must not \
         collide with an existing skill (use a `-2`, `-3` suffix on collision)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name":          { "type": "string", "pattern": SKILL_NAME_RE },
                "description":   { "type": "string", "minLength": 1 },
                "body":          { "type": "string", "minLength": 1 },
                "allowed_tools": { "type": "array", "items": { "type": "string" } },
                "when_to_use":   { "type": "string" },
                "argument_hint": { "type": "string" }
            },
            "required": ["name", "description", "body"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: SkillCreateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        // Re-validate name against the same regex the registry enforces.
        // Cheap defence-in-depth: the schema check happens at JSON-parse
        // time but a non-validating LLM call could still slip through.
        let re = regex::Regex::new(SKILL_NAME_RE)
            .map_err(|e| ToolError::Execution(format!("regex compile: {e}")))?;
        if !re.is_match(&p.name) {
            return Err(ToolError::InvalidParams(format!(
                "invalid skill name {:?}; must match {}",
                p.name, SKILL_NAME_RE
            )));
        }

        let auto_dir = ctx.workspace_paths.skills_dir().join("auto").join(&p.name);
        if auto_dir.exists() {
            return Err(ToolError::Execution(format!(
                "skill directory already exists: {} — retry with a suffixed name (e.g. {}-2)",
                auto_dir.display(),
                p.name
            )));
        }
        if self.skills.get(&p.name).is_some() {
            return Err(ToolError::Execution(format!(
                "skill name {:?} is already registered (different on-disk path); pick a unique name",
                p.name
            )));
        }

        tokio::fs::create_dir_all(&auto_dir)
            .await
            .map_err(|e| ToolError::Execution(format!("mkdir {}: {e}", auto_dir.display())))?;

        let skill_md_path = auto_dir.join("SKILL.md");
        let content = render_skill_md(&p);
        tokio::fs::write(&skill_md_path, &content)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", skill_md_path.display())))?;

        // Reload so the new skill is visible to subsequent SkillList
        // calls in this same self_improvement iteration; without this the
        // agent would write a skill, list, and not see it.
        let _ = self.skills.reload();

        Ok(ToolOutput::Json(json!({
            "outcome": "created",
            "name": p.name,
            "path": skill_md_path.display().to_string(),
        })))
    }
}

fn render_skill_md(p: &SkillCreateParams) -> String {
    let mut frontmatter = String::from("---\n");
    frontmatter.push_str(&format!("name: {}\n", p.name));
    let desc_one_line = p.description.replace('\n', " ");
    frontmatter.push_str(&format!("description: {desc_one_line}\n"));
    if let Some(when) = &p.when_to_use {
        let when_one_line = when.replace('\n', " ");
        frontmatter.push_str(&format!("when_to_use: {when_one_line}\n"));
    }
    if let Some(hint) = &p.argument_hint {
        let hint_one_line = hint.replace('\n', " ");
        frontmatter.push_str(&format!("argument-hint: {hint_one_line}\n"));
    }
    if !p.allowed_tools.is_empty() {
        frontmatter.push_str(&format!("allowed-tools: {}\n", p.allowed_tools.join(" ")));
    }
    // Hardcoded — auto-generated skills MUST require explicit user
    // invocation. The operator flips this to `false` in the SKILL.md
    // file after reviewing the body. See `docs/modules/self-improvement.md`.
    frontmatter.push_str("disable-model-invocation: true\n");
    frontmatter.push_str("---\n\n");
    frontmatter.push_str(p.body.trim_end());
    frontmatter.push('\n');
    frontmatter
}

// ── SkillList ────────────────────────────────────────────────────────

struct SkillListTool {
    skills: Arc<SkillRegistry>,
}

impl SkillListTool {
    fn new(skills: Arc<SkillRegistry>) -> Self {
        Self { skills }
    }
}

#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "SkillList"
    }

    fn description(&self) -> &str {
        "List every registered skill with name + description + trust + \
         agent-invocability flag. Used by the self_improvement flow to \
         dedupe candidate skill creations against existing skills."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let summaries = self.skills.all_summaries_sorted();
        let entries: Vec<Value> = summaries
            .into_iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "agent_invocable": s.agent_invocable,
                    "trust_level": format!("{:?}", s.trust_level),
                })
            })
            .collect();
        Ok(ToolOutput::Json(json!({ "skills": entries })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_skill_md_carries_disable_model_invocation_true() {
        let p = SkillCreateParams {
            name: "deploy-helper".into(),
            description: "Run the standard deploy sequence".into(),
            body: "1. Build\n2. Push".into(),
            allowed_tools: vec!["Bash".into()],
            when_to_use: Some("user asks to deploy".into()),
            argument_hint: None,
        };
        let md = render_skill_md(&p);
        assert!(md.contains("disable-model-invocation: true"));
        assert!(md.contains("allowed-tools: Bash"));
        assert!(md.contains("name: deploy-helper"));
        assert!(md.ends_with("1. Build\n2. Push\n"));
    }

    #[test]
    fn render_skill_md_strips_newlines_in_one_line_fields() {
        let p = SkillCreateParams {
            name: "x".into(),
            description: "line one\nline two".into(),
            body: "body".into(),
            allowed_tools: vec![],
            when_to_use: Some("when\nyes".into()),
            argument_hint: Some("[id]\nfoo".into()),
        };
        let md = render_skill_md(&p);
        assert!(md.contains("description: line one line two"));
        assert!(md.contains("when_to_use: when yes"));
        assert!(md.contains("argument-hint: [id] foo"));
    }
}
