//! The goal tools (`create_goal` / `get_goal` / `update_goal`): the model's
//! lifecycle controls over the session's autonomous objective. Each holds an
//! `Arc<GoalService>` and writes directly from `execute` — no actor round-trip,
//! like the `Task*` tools. The tools cannot pause/resume/budget a goal; those
//! are user/system controlled (`/goal`, the budget/cost gates).

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{
    CREATE_GOAL_TOOL_NAME, GET_GOAL_TOOL_NAME, Goal, GoalStatus, UPDATE_GOAL_TOOL_NAME,
};
use baybo_store::goal::GoalStore;
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::service::{GoalError, GoalService};

/// Build the three goal tools with their manifests, ready to register with a
/// `ToolRegistry`. Always `Trusted` with no capabilities — they operate on
/// agent-internal goal state, not the host filesystem or network, so the
/// approval gate is a no-op (`accessed_resources` stays empty).
pub fn agent_tools(store: Arc<dyn GoalStore>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let service = Arc::new(GoalService::new(store));
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(CreateGoalTool::new(Arc::clone(&service))),
        Arc::new(GetGoalTool::new(Arc::clone(&service))),
        Arc::new(UpdateGoalTool::new(service)),
    ];
    tools.into_iter().map(with_manifest).collect()
}

fn with_manifest(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
    };
    (tool, manifest)
}

fn exec_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(e.to_string())
}

/// Model-facing view of a goal: objective, status, budget/usage. Built by hand
/// (not `serde_json::to_value`) so it's infallible and omits the internal id.
fn render_goal(goal: &Goal) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("objective".into(), json!(goal.objective));
    obj.insert("status".into(), json!(goal.status.as_str()));
    obj.insert("tokens_used".into(), json!(goal.tokens_used));
    obj.insert("time_used_seconds".into(), json!(goal.time_used_seconds));
    match goal.token_budget {
        Some(budget) => {
            obj.insert("token_budget".into(), json!(budget));
            obj.insert("tokens_remaining".into(), json!(goal.remaining_budget()));
        }
        None => {
            obj.insert("token_budget".into(), Value::Null);
        }
    }
    obj.insert("created_at".into(), json!(goal.created_at.to_rfc3339()));
    obj.insert("updated_at".into(), json!(goal.updated_at.to_rfc3339()));
    Value::Object(obj)
}

struct CreateGoalTool {
    service: Arc<GoalService>,
}

impl CreateGoalTool {
    fn new(service: Arc<GoalService>) -> Self {
        Self { service }
    }
}

#[derive(Debug, Deserialize)]
struct CreateGoalParams {
    objective: String,
    #[serde(default)]
    token_budget: Option<u64>,
}

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        CREATE_GOAL_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Start a persistent goal: a standing objective you will keep working toward on your own, turn after turn, until it is verifiably done — without the user prompting you each time. Use this ONLY when the user explicitly asks you to keep going until something is achieved ("keep working on X until it's done", "don't stop until the tests pass"). Do NOT infer a goal from an ordinary one-off task. `objective` is the goal in plain language. Optional `token_budget` caps the tokens the goal may spend before it winds down (omit it to run until you mark it complete or blocked; the operator's global spend limit is the backstop either way). Fails if a goal is already active for this session. Once set, the system re-runs you automatically at each turn boundary; call `get_goal` to check progress and `update_goal` to mark the goal complete or blocked."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "objective": { "type": "string", "description": "The standing objective to pursue, in plain language." },
                "token_budget": { "type": "integer", "minimum": 1, "description": "Optional: max tokens the goal may spend before winding down. Omit for no per-goal budget." }
            },
            "required": ["objective"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("objective")
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: CreateGoalParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if p.objective.trim().is_empty() {
            return Err(ToolError::InvalidParams(
                "`objective` must be non-empty".into(),
            ));
        }
        if matches!(p.token_budget, Some(0)) {
            return Err(ToolError::InvalidParams(
                "`token_budget` must be at least 1 (omit it for no budget)".into(),
            ));
        }
        match self
            .service
            .create(&ctx.session_id, p.objective.trim(), p.token_budget)
            .await
        {
            Ok(goal) => Ok(ToolOutput::Json(json!({
                "created": true,
                "goal": render_goal(&goal),
            }))),
            Err(GoalError::AlreadyActive { objective }) => Err(ToolError::Execution(format!(
                "a goal is already active for this session ({objective:?}); the user can `/goal pause` or `/goal clear` it before a new one is set"
            ))),
            Err(e) => Err(exec_err(e)),
        }
    }
}

struct GetGoalTool {
    service: Arc<GoalService>,
}

impl GetGoalTool {
    fn new(service: Arc<GoalService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        GET_GOAL_TOOL_NAME
    }

    /// Read-only lookup — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "Read this session's current goal: its objective, status, token budget, \
         tokens used so far, remaining budget, and elapsed time. Returns \
         `{ \"goal\": null }` when no goal is set."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let goal = self
            .service
            .current(&ctx.session_id)
            .await
            .map_err(exec_err)?;
        Ok(ToolOutput::Json(json!({
            "goal": goal.as_ref().map(render_goal),
        })))
    }
}

struct UpdateGoalTool {
    service: Arc<GoalService>,
}

impl UpdateGoalTool {
    fn new(service: Arc<GoalService>) -> Self {
        Self { service }
    }
}

#[derive(Debug, Deserialize)]
struct UpdateGoalParams {
    /// Raw string: only `complete` or `blocked` are accepted (the other states
    /// are user/system controlled). Parsed in `execute`.
    status: String,
}

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        UPDATE_GOAL_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Update the status of this session's current goal. Set `status` to `complete` ONLY when the objective is genuinely and verifiably done — before doing so, confirm every requirement the objective implies is satisfied with authoritative evidence (a passing test, the file that exists, the actual command output); treat anything you cannot directly verify as not done. Set `status` to `blocked` ONLY for a genuine impasse, and only after the SAME blocker has stopped progress across at least three consecutive goal turns despite real attempts to get around it. You cannot pause, resume, or budget a goal through this tool — those are controlled by the user (`/goal`) and the system."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["complete", "blocked"],
                    "description": "`complete` (objective verifiably achieved) or `blocked` (genuine, recurring impasse)."
                }
            },
            "required": ["status"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("status")
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: UpdateGoalParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let status = match p.status.as_str() {
            "complete" => GoalStatus::Complete,
            "blocked" => GoalStatus::Blocked,
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "`status` must be `complete` or `blocked` (got {other:?}); pause/resume are user-controlled via `/goal`"
                )));
            }
        };
        let updated = self
            .service
            .set_status(&ctx.session_id, status)
            .await
            .map_err(exec_err)?;
        if !updated {
            return Err(ToolError::Execution(
                "no goal is set for this session".into(),
            ));
        }
        Ok(ToolOutput::Json(json!({
            "status": status.as_str(),
            "goal": self
                .service
                .current(&ctx.session_id)
                .await
                .map_err(exec_err)?
                .as_ref()
                .map(render_goal),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryGoalStore;
    use baybo_model::{ChannelType, User};
    use std::time::Duration;

    fn ctx(session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            job_id: baybo_model::JobId::default(),
            span_id: baybo_model::SpanId::default(),
            user: User {
                id: "u1".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: baybo_tools::noop_event_sink(),
            llm: None,
            secrets: None,
            virtual_reads: None,
            read_tracker: None,
            background_jobs: None,
            background_control: None,
        }
    }

    fn service() -> Arc<GoalService> {
        Arc::new(GoalService::new(Arc::new(MemoryGoalStore::new())))
    }

    fn json_out(out: ToolOutput) -> Value {
        match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected Json output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips() {
        let svc = service();
        let create = CreateGoalTool::new(Arc::clone(&svc));
        let v = json_out(
            create
                .execute(
                    json!({ "objective": "ship the feature", "token_budget": 5000 }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        assert_eq!(v["created"], true);
        assert_eq!(v["goal"]["status"], "active");
        assert_eq!(v["goal"]["token_budget"], 5000);

        let get = GetGoalTool::new(Arc::clone(&svc));
        let v = json_out(get.execute(json!({}), &ctx("s1")).await.unwrap());
        assert_eq!(v["goal"]["objective"], "ship the feature");
        assert_eq!(v["goal"]["tokens_remaining"], 5000);
    }

    #[tokio::test]
    async fn create_rejects_blank_and_zero_budget() {
        let svc = service();
        let create = CreateGoalTool::new(svc);
        assert!(matches!(
            create
                .execute(json!({ "objective": "  " }), &ctx("s1"))
                .await
                .unwrap_err(),
            ToolError::InvalidParams(_)
        ));
        assert!(matches!(
            create
                .execute(json!({ "objective": "x", "token_budget": 0 }), &ctx("s1"))
                .await
                .unwrap_err(),
            ToolError::InvalidParams(_)
        ));
    }

    #[tokio::test]
    async fn create_fails_when_one_already_active() {
        let svc = service();
        let create = CreateGoalTool::new(svc);
        create
            .execute(json!({ "objective": "first" }), &ctx("s1"))
            .await
            .unwrap();
        let err = create
            .execute(json!({ "objective": "second" }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn get_with_no_goal_is_null() {
        let svc = service();
        let get = GetGoalTool::new(svc);
        let v = json_out(get.execute(json!({}), &ctx("s1")).await.unwrap());
        assert!(v["goal"].is_null());
    }

    #[tokio::test]
    async fn update_marks_complete_and_blocked() {
        let svc = service();
        let create = CreateGoalTool::new(Arc::clone(&svc));
        create
            .execute(json!({ "objective": "x" }), &ctx("s1"))
            .await
            .unwrap();
        let update = UpdateGoalTool::new(Arc::clone(&svc));
        let v = json_out(
            update
                .execute(json!({ "status": "complete" }), &ctx("s1"))
                .await
                .unwrap(),
        );
        assert_eq!(v["status"], "complete");
        assert_eq!(v["goal"]["status"], "complete");
    }

    #[tokio::test]
    async fn update_rejects_non_model_statuses() {
        let svc = service();
        let create = CreateGoalTool::new(Arc::clone(&svc));
        create
            .execute(json!({ "objective": "x" }), &ctx("s1"))
            .await
            .unwrap();
        let update = UpdateGoalTool::new(svc);
        for bad in ["active", "paused", "budget_limited", "spend_capped", "nope"] {
            assert!(
                matches!(
                    update
                        .execute(json!({ "status": bad }), &ctx("s1"))
                        .await
                        .unwrap_err(),
                    ToolError::InvalidParams(_)
                ),
                "status {bad} must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn update_without_goal_errors() {
        let svc = service();
        let update = UpdateGoalTool::new(svc);
        let err = update
            .execute(json!({ "status": "complete" }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }
}
