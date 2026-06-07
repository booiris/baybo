//! The `Task*` tools: store-scoped CRUD over the per-session planning
//! checklist. Each holds an `Arc<dyn TaskStore>` and writes directly from
//! `execute` (the `session_tasks` table is shared state, so no actor round-trip
//! is needed — see the crate docs).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{
    TASK_CREATE_TOOL_NAME, TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME, Task,
    TaskId, TaskStatus,
};
use aura_store::task::{TaskPatch, TaskStore};
use aura_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

/// Build every checklist tool with its manifest, ready to register with a
/// `ToolRegistry`. Always `Trusted` with no capabilities — they operate on
/// agent-internal checklist state, not on the host filesystem or network, so
/// the approval gate is a no-op (`accessed_resources` stays empty).
pub fn agent_tools(store: Arc<dyn TaskStore>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(TaskCreateTool::new(Arc::clone(&store))),
        Arc::new(TaskListTool::new(Arc::clone(&store))),
        Arc::new(TaskGetTool::new(Arc::clone(&store))),
        Arc::new(TaskUpdateTool::new(store)),
    ];
    tools.into_iter().map(with_manifest).collect()
}

fn with_manifest(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: aura_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
    };
    (tool, manifest)
}

// --- shared helpers ---------------------------------------------------------

/// The special `status` value on `TaskUpdate` that DELETES the task (removes
/// the row) rather than setting a stored status — mirrors Claude Code's
/// `TaskUpdate(status: "deleted")`.
const DELETED_ACTION: &str = "deleted";

fn status_values() -> Vec<&'static str> {
    TaskStatus::ALL.iter().map(TaskStatus::as_str).collect()
}

/// JSON-schema fragment for a `status` field, sourced from [`TaskStatus::ALL`]
/// so the allowed values can't drift from the enum.
fn status_schema(description: &str) -> Value {
    json!({ "type": "string", "enum": status_values(), "description": description })
}

/// Like [`status_schema`] but also accepts the `deleted` action (`TaskUpdate`).
fn status_or_deleted_schema(description: &str) -> Value {
    let mut allowed = status_values();
    allowed.push(DELETED_ACTION);
    json!({ "type": "string", "enum": allowed, "description": description })
}

fn parse_task_id(s: &str, field: &str) -> Result<TaskId, ToolError> {
    s.parse::<TaskId>()
        .map_err(|e| ToolError::InvalidParams(format!("invalid task id in `{field}` ({s:?}): {e}")))
}

fn parse_depends_on(raw: Option<&Vec<String>>) -> Result<Vec<TaskId>, ToolError> {
    match raw {
        None => Ok(Vec::new()),
        Some(ids) => ids.iter().map(|s| parse_task_id(s, "depends_on")).collect(),
    }
}

/// Compact, model-facing view: the fields the LLM scans in the list (`subject`
/// is the title; the `description` body shows in `TaskGet`). Built by hand (not
/// `serde_json::to_value`) so it's infallible and omits internal timestamps.
fn render_task(task: &Task) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(task.id.to_string()));
    obj.insert("subject".into(), json!(task.subject));
    obj.insert("status".into(), json!(task.status.as_str()));
    if !task.depends_on.is_empty() {
        let deps: Vec<String> = task.depends_on.iter().map(TaskId::to_string).collect();
        obj.insert("depends_on".into(), json!(deps));
    }
    Value::Object(obj)
}

fn render_task_detailed(task: &Task) -> Value {
    let mut v = render_task(task);
    if let Value::Object(map) = &mut v {
        map.insert("description".into(), json!(task.description));
        map.insert("created_at".into(), json!(task.created_at.to_rfc3339()));
        map.insert("updated_at".into(), json!(task.updated_at.to_rfc3339()));
    }
    v
}

fn render_list(tasks: &[Task]) -> Value {
    json!({
        "count": tasks.len(),
        "tasks": tasks.iter().map(render_task).collect::<Vec<_>>(),
    })
}

fn exec_err(e: impl std::fmt::Display) -> ToolError {
    ToolError::Execution(e.to_string())
}

/// `base + index µs`. Tasks created in one batch otherwise share an identical
/// `created_at`, and `ORDER BY created_at, task_id` would then fall back to the
/// random ULID tail — losing the order the model wrote the list in.
fn stamp(base: chrono::DateTime<Utc>, index: usize) -> chrono::DateTime<Utc> {
    base + chrono::TimeDelta::microseconds(index as i64)
}

// ---------------------------------------------------------------------------
// TaskCreate
// ---------------------------------------------------------------------------

struct TaskCreateTool {
    store: Arc<dyn TaskStore>,
}

impl TaskCreateTool {
    fn new(store: Arc<dyn TaskStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct NewTaskInput {
    subject: String,
    description: String,
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TaskCreateParams {
    tasks: Vec<NewTaskInput>,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        TASK_CREATE_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Add one or more tasks to this session's planning checklist. Use it to lay out a multi-step plan before you start, so both you and the user can track progress. Each task has a brief imperative `subject` (e.g. "Add the TaskStore table"), a `description` of what needs to be done, an optional `status` (defaults to `pending`), and optional `depends_on` task ids. Returns the created task ids. Convention: keep at most ONE task `in_progress` at a time — mark a task `in_progress` when you start it and `completed` the moment it's done. If a task turns out bigger or more complex than expected while you're working on it, decompose it: call `TaskCreate` again to add the finer sub-tasks (wire prerequisites with `depends_on`) rather than pushing through one giant step. To edit or delete one task use `TaskUpdate`."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "One or more tasks to add to the checklist.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "subject": { "type": "string", "description": "Brief imperative title of the task." },
                            "description": { "type": "string", "description": "What needs to be done — the task body." },
                            "status": status_schema("Initial status; defaults to `pending`."),
                            "depends_on": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Ids of tasks that must complete first (advisory). May reference other tasks created in this same call."
                            }
                        },
                        "required": ["subject", "description"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("tasks")
            .and_then(Value::as_array)
            .and_then(|t| t.first())
            .and_then(|t| t.get("subject"))
            .and_then(Value::as_str)
            .and_then(aura_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: TaskCreateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if p.tasks.is_empty() {
            return Err(ToolError::InvalidParams(
                "`tasks` must be a non-empty array".into(),
            ));
        }
        for t in &p.tasks {
            if t.subject.trim().is_empty() {
                return Err(ToolError::InvalidParams(
                    "task `subject` must be non-empty".into(),
                ));
            }
            if t.description.trim().is_empty() {
                return Err(ToolError::InvalidParams(
                    "task `description` must be non-empty".into(),
                ));
            }
        }

        // Mint ids up front so same-batch `depends_on` references resolve.
        let existing = self.store.list(&ctx.session_id).await.map_err(exec_err)?;
        let minted: Vec<TaskId> = p.tasks.iter().map(|_| TaskId::new()).collect();
        let mut valid: HashSet<TaskId> = existing.iter().map(|t| t.id).collect();
        valid.extend(minted.iter().copied());

        let now = Utc::now();
        let mut new_tasks = Vec::with_capacity(p.tasks.len());
        for (i, (input, id)) in p.tasks.iter().zip(minted.iter().copied()).enumerate() {
            let depends_on = parse_depends_on(input.depends_on.as_ref())?;
            for dep in &depends_on {
                if !valid.contains(dep) {
                    return Err(ToolError::InvalidParams(format!(
                        "`depends_on` references unknown task id {dep}"
                    )));
                }
            }
            // Offset created_at by batch index so the list view keeps the
            // order the model wrote (ULID tie-break alone is random within a
            // millisecond).
            let created_at = stamp(now, i);
            new_tasks.push(Task {
                id,
                subject: input.subject.clone(),
                description: input.description.clone(),
                status: input.status.unwrap_or(TaskStatus::Pending),
                depends_on,
                created_at,
                updated_at: created_at,
            });
        }

        for task in &new_tasks {
            self.store
                .create(&ctx.session_id, task)
                .await
                .map_err(exec_err)?;
        }

        Ok(ToolOutput::Json(json!({
            "created": new_tasks.iter().map(|t| t.id.to_string()).collect::<Vec<_>>(),
            "tasks": new_tasks.iter().map(render_task).collect::<Vec<_>>(),
        })))
    }
}

// ---------------------------------------------------------------------------
// TaskList
// ---------------------------------------------------------------------------

struct TaskListTool {
    store: Arc<dyn TaskStore>,
}

impl TaskListTool {
    fn new(store: Arc<dyn TaskStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct TaskListParams {
    #[serde(default)]
    status: Option<TaskStatus>,
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        TASK_LIST_TOOL_NAME
    }

    /// Read-only listing — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "List this session's planning-checklist tasks with their status — the \
         at-a-glance progress view. Optionally filter by `status`."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": status_schema("Optional: only return tasks with this status.")
            }
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: TaskListParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let mut tasks = self.store.list(&ctx.session_id).await.map_err(exec_err)?;
        if let Some(status) = p.status {
            tasks.retain(|t| t.status == status);
        }
        Ok(ToolOutput::Json(render_list(&tasks)))
    }
}

// ---------------------------------------------------------------------------
// TaskGet
// ---------------------------------------------------------------------------

struct TaskGetTool {
    store: Arc<dyn TaskStore>,
}

impl TaskGetTool {
    fn new(store: Arc<dyn TaskStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct IdParams {
    id: String,
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        TASK_GET_TOOL_NAME
    }

    /// Read-only lookup — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "Get the full detail of one planning-checklist task by `id`.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Task id to retrieve." }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: IdParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let id = parse_task_id(&p.id, "id")?;
        match self
            .store
            .get(&ctx.session_id, &id)
            .await
            .map_err(exec_err)?
        {
            Some(task) => Ok(ToolOutput::Json(render_task_detailed(&task))),
            None => Err(ToolError::Execution(format!(
                "no task with id {id} in this session"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskUpdate
// ---------------------------------------------------------------------------

struct TaskUpdateTool {
    store: Arc<dyn TaskStore>,
}

impl TaskUpdateTool {
    fn new(store: Arc<dyn TaskStore>) -> Self {
        Self { store }
    }
}

#[derive(Debug, Deserialize)]
struct TaskUpdateParams {
    id: String,
    /// Raw string: a lifecycle status OR the `deleted` action. Parsed in
    /// `execute` because `deleted` is not a stored [`TaskStatus`].
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        TASK_UPDATE_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Update or delete one task in the session checklist by `id`. Set `status` to move it through `pending` -> `in_progress` -> `completed`, or to `deleted` to remove the task entirely. You can also edit its `subject` / `description` / `depends_on`. Only the fields you pass change. Mark a task `in_progress` when you start it (one at a time) and `completed` the moment it's done so the checklist reflects reality. When you split a task into finer sub-tasks, narrow this task's scope (edit its `subject`/`description`), or set `status` to `deleted` if the sub-tasks fully replace it."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Task id to update." },
                "status": status_or_deleted_schema("New status, or `deleted` to remove the task entirely."),
                "subject": { "type": "string", "description": "New brief title." },
                "description": { "type": "string", "description": "New task body." },
                "depends_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Replace the task's dependency ids (advisory)."
                }
            },
            "required": ["id"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("id")
            .and_then(Value::as_str)
            .and_then(aura_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: TaskUpdateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let id = parse_task_id(&p.id, "id")?;

        // `status` is a raw string: a lifecycle status, the `deleted` action,
        // or absent. `deleted` removes the row instead of setting a status.
        let mut new_status: Option<TaskStatus> = None;
        if let Some(s) = p.status.as_deref() {
            if s == DELETED_ACTION {
                let deleted = self
                    .store
                    .delete(&ctx.session_id, &id)
                    .await
                    .map_err(exec_err)?;
                if !deleted {
                    return Err(ToolError::Execution(format!(
                        "no task with id {id} in this session"
                    )));
                }
                return Ok(ToolOutput::Json(json!({ "deleted": id.to_string() })));
            }
            match TaskStatus::from_storage_str(s) {
                Some(st) => new_status = Some(st),
                None => {
                    let mut allowed = status_values();
                    allowed.push(DELETED_ACTION);
                    return Err(ToolError::InvalidParams(format!(
                        "`status` must be one of {allowed:?}"
                    )));
                }
            }
        }

        for (field, val) in [("subject", &p.subject), ("description", &p.description)] {
            if let Some(v) = val
                && v.trim().is_empty()
            {
                return Err(ToolError::InvalidParams(format!(
                    "`{field}` must be non-empty"
                )));
            }
        }

        let mut patch = TaskPatch::new(id);
        patch.status = new_status;
        patch.subject = p.subject;
        patch.description = p.description;
        if let Some(dep_strs) = &p.depends_on {
            let deps = parse_depends_on(Some(dep_strs))?;
            let existing = self.store.list(&ctx.session_id).await.map_err(exec_err)?;
            let valid: HashSet<TaskId> = existing.iter().map(|t| t.id).collect();
            for dep in &deps {
                if *dep == id {
                    return Err(ToolError::InvalidParams(
                        "a task cannot depend on itself".into(),
                    ));
                }
                if !valid.contains(dep) {
                    return Err(ToolError::InvalidParams(format!(
                        "`depends_on` references unknown task id {dep}"
                    )));
                }
            }
            patch.depends_on = Some(deps);
        }

        if patch.is_empty() {
            return Err(ToolError::InvalidParams(
                "provide at least one field to update (`status`, `subject`, `description`, or `depends_on`)".into(),
            ));
        }

        let updated = self
            .store
            .update(&ctx.session_id, &patch)
            .await
            .map_err(exec_err)?;
        if !updated {
            return Err(ToolError::Execution(format!(
                "no task with id {id} in this session"
            )));
        }
        match self
            .store
            .get(&ctx.session_id, &id)
            .await
            .map_err(exec_err)?
        {
            Some(task) => Ok(ToolOutput::Json(render_task_detailed(&task))),
            None => Ok(ToolOutput::Json(json!({ "updated": id.to_string() }))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryTaskStore;
    use aura_model::{ChannelType, User};
    use std::time::Duration;

    fn ctx(session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u1".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: aura_tools::noop_event_sink(),
            llm: None,
            secrets: None,
            virtual_reads: None,
            background_jobs: None,
        }
    }

    fn store() -> Arc<dyn TaskStore> {
        Arc::new(MemoryTaskStore::new())
    }

    fn json_out(out: ToolOutput) -> Value {
        match out {
            ToolOutput::Json(v) => v,
            other => panic!("expected Json output, got {other:?}"),
        }
    }

    /// Create one task and return its id.
    async fn create_one(s: &Arc<dyn TaskStore>, session: &str, subject: &str) -> String {
        let create = TaskCreateTool::new(Arc::clone(s));
        let v = json_out(
            create
                .execute(
                    json!({ "tasks": [ { "subject": subject, "description": "body" } ] }),
                    &ctx(session),
                )
                .await
                .unwrap(),
        );
        v["created"][0].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn create_then_list_round_trips() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        let out = create
            .execute(
                json!({ "tasks": [
                    { "subject": "write code", "description": "implement the feature" },
                    { "subject": "run tests", "description": "cargo test", "status": "pending" }
                ] }),
                &ctx("s1"),
            )
            .await
            .unwrap();
        let v = json_out(out);
        assert_eq!(v["created"].as_array().unwrap().len(), 2);

        let list = TaskListTool::new(Arc::clone(&s));
        let v = json_out(list.execute(json!({}), &ctx("s1")).await.unwrap());
        assert_eq!(v["count"], 2);
        assert_eq!(v["tasks"][0]["subject"], "write code");
        assert_eq!(v["tasks"][0]["status"], "pending");
        // The compact list view carries the subject, not the body.
        assert!(v["tasks"][0]["description"].is_null());
    }

    #[tokio::test]
    async fn create_rejects_empty_and_blank_fields() {
        let s = store();
        let create = TaskCreateTool::new(s);
        let err = create
            .execute(json!({ "tasks": [] }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
        // Blank subject.
        let err = create
            .execute(
                json!({ "tasks": [ { "subject": "  ", "description": "x" } ] }),
                &ctx("s1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
        // Missing description.
        let err = create
            .execute(json!({ "tasks": [ { "subject": "x" } ] }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn create_validates_depends_on_and_allows_same_batch_refs() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        // Unknown dependency id is rejected.
        let err = create
            .execute(
                json!({ "tasks": [ { "subject": "x", "description": "y", "depends_on": [TaskId::new().to_string()] } ] }),
                &ctx("s1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
        // Malformed id is rejected.
        let err = create
            .execute(
                json!({ "tasks": [ { "subject": "x", "description": "y", "depends_on": ["not-an-id"] } ] }),
                &ctx("s1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn update_is_sparse_and_returns_task() {
        let s = store();
        let id = create_one(&s, "s1", "build").await;

        let update = TaskUpdateTool::new(Arc::clone(&s));
        let v = json_out(
            update
                .execute(json!({ "id": id, "status": "in_progress" }), &ctx("s1"))
                .await
                .unwrap(),
        );
        assert_eq!(v["status"], "in_progress");
        assert_eq!(v["subject"], "build");
    }

    #[tokio::test]
    async fn update_requires_a_field_and_existing_id() {
        let s = store();
        let id = create_one(&s, "s1", "build").await;

        let update = TaskUpdateTool::new(Arc::clone(&s));
        let err = update
            .execute(json!({ "id": id }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)), "empty patch");

        let err = update
            .execute(
                json!({ "id": TaskId::new().to_string(), "status": "completed" }),
                &ctx("s1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)), "absent id");
    }

    #[tokio::test]
    async fn update_is_scoped_to_session() {
        let s = store();
        let id = create_one(&s, "s1", "a").await;
        // Same id, different session → not found.
        let update = TaskUpdateTool::new(Arc::clone(&s));
        let err = update
            .execute(json!({ "id": id, "status": "completed" }), &ctx("other"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn update_status_deleted_removes_the_task() {
        let s = store();
        let id = create_one(&s, "s1", "scrap me").await;

        let update = TaskUpdateTool::new(Arc::clone(&s));
        let v = json_out(
            update
                .execute(json!({ "id": id, "status": "deleted" }), &ctx("s1"))
                .await
                .unwrap(),
        );
        assert_eq!(v["deleted"].as_str(), Some(id.as_str()));

        // Gone from the list, and a second delete is a not-found error.
        let list = TaskListTool::new(Arc::clone(&s));
        let v = json_out(list.execute(json!({}), &ctx("s1")).await.unwrap());
        assert_eq!(v["count"], 0);
        let err = update
            .execute(json!({ "id": id, "status": "deleted" }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn update_rejects_unknown_status() {
        let s = store();
        let id = create_one(&s, "s1", "x").await;
        let update = TaskUpdateTool::new(s);
        let err = update
            .execute(json!({ "id": id, "status": "archived" }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn get_returns_detail_or_not_found() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        let v = json_out(
            create
                .execute(
                    json!({ "tasks": [ { "subject": "build", "description": "carefully" } ] }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        let id = v["created"][0].as_str().unwrap().to_string();

        let get = TaskGetTool::new(Arc::clone(&s));
        let v = json_out(get.execute(json!({ "id": &id }), &ctx("s1")).await.unwrap());
        assert_eq!(v["subject"], "build");
        assert_eq!(v["description"], "carefully");
        assert!(v["created_at"].is_string());

        let err = get
            .execute(json!({ "id": TaskId::new().to_string() }), &ctx("s1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        create
            .execute(
                json!({ "tasks": [ { "subject": "a", "description": "x", "status": "completed" }, { "subject": "b", "description": "y", "status": "pending" } ] }),
                &ctx("s1"),
            )
            .await
            .unwrap();
        let list = TaskListTool::new(s);
        let v = json_out(
            list.execute(json!({ "status": "completed" }), &ctx("s1"))
                .await
                .unwrap(),
        );
        assert_eq!(v["count"], 1);
        assert_eq!(v["tasks"][0]["subject"], "a");
    }
}
