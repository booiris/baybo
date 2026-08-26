//! The `Task*` tools: store-scoped CRUD over the per-session planning
//! checklist. Each holds an `Arc<dyn TaskStore>` and writes directly from
//! `execute` (the `session_tasks` table is shared state, so no actor round-trip
//! is needed — see the crate docs).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{
    TASK_CREATE_TOOL_NAME, TASK_GET_TOOL_NAME, TASK_LIST_TOOL_NAME, TASK_UPDATE_TOOL_NAME, Task,
    TaskId, TaskStatus,
};
use baybo_store::task::{TaskPatch, TaskStore};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput};
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
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
        channels: Vec::new(),
    };
    (tool, manifest)
}

// --- shared helpers ---------------------------------------------------------

/// The special `status` value on `TaskUpdate` that DELETES the task (removes
/// the row) rather than setting a stored status — mirrors Claude Code's
/// `TaskUpdate(status: "deleted")`.
const DELETED_ACTION: &str = "deleted";
const UNCHANGED_ACTION: &str = "unchanged";
const ALL_STATUSES_FILTER: &str = "all";

fn status_values() -> Vec<&'static str> {
    TaskStatus::ALL.iter().map(TaskStatus::as_str).collect()
}

/// JSON-schema fragment for a `status` field, sourced from [`TaskStatus::ALL`]
/// so the allowed values can't drift from the enum.
fn status_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "enum": status_values(),
        "default": TaskStatus::Pending.as_str(),
        "description": description,
    })
}

/// Like [`status_schema`] but also accepts the `deleted` action (`TaskUpdate`).
fn status_or_deleted_schema(description: &str) -> Value {
    let mut allowed = vec![UNCHANGED_ACTION];
    allowed.extend(status_values());
    allowed.push(DELETED_ACTION);
    json!({ "type": "string", "enum": allowed, "description": description })
}

fn status_filter_schema(description: &str) -> Value {
    let mut allowed = vec![ALL_STATUSES_FILTER];
    allowed.extend(status_values());
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
    #[serde(default)]
    key: Option<String>,
    subject: String,
    description: String,
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    depends_on_keys: Option<Vec<String>>,
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
        r#"Add one or more tasks to this session's planning checklist. Use it to lay out a multi-step plan before you start, so both you and the user can track progress. Each task has a brief imperative `subject` (e.g. "Add the TaskStore table"), a `description` of what needs to be done, an optional `status` (defaults to `pending`), and optional prerequisites. Use `depends_on` for real task ids returned by an earlier call. Within this call, give prerequisite tasks a short `key` and reference those names through `depends_on_keys`; do not invent numeric or placeholder task ids. Returns the created task ids and their batch-key mapping. Convention: keep at most ONE task `in_progress` at a time — mark a task `in_progress` when you start it and `completed` the moment it's done. If a task turns out bigger or more complex than expected while you're working on it, decompose it: call `TaskCreate` again to add the finer sub-tasks and wire prerequisites rather than pushing through one giant step. To edit or delete one task use `TaskUpdate`."#
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
                            "key": { "type": "string", "description": "Optional name used only by this call so later entries can depend on this task through `depends_on_keys`. An empty string means unset." },
                            "subject": { "type": "string", "description": "Brief imperative title of the task." },
                            "description": { "type": "string", "description": "What needs to be done — the task body." },
                            "status": status_schema("Initial status; defaults to `pending`."),
                            "depends_on": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Real task ids returned by earlier TaskCreate or TaskList calls that must complete first (advisory). Do not put batch keys or list positions here."
                            },
                            "depends_on_keys": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Keys of prerequisite tasks elsewhere in this same `tasks` array. Each referenced entry must declare the matching `key`."
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
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
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

        // Mint ids up front so batch-local keys resolve before any row is
        // written. The caller cannot know these random ids ahead of time.
        let existing = self.store.list(&ctx.session_id).await.map_err(exec_err)?;
        let minted: Vec<TaskId> = p.tasks.iter().map(|_| TaskId::new()).collect();
        let mut valid: HashSet<TaskId> = existing.iter().map(|t| t.id).collect();
        valid.extend(minted.iter().copied());
        let mut ids_by_key = HashMap::new();
        for (input, id) in p.tasks.iter().zip(minted.iter().copied()) {
            let Some(key) = input
                .key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
            else {
                continue;
            };
            if ids_by_key.insert(key.to_owned(), id).is_some() {
                return Err(ToolError::InvalidParams(format!(
                    "duplicate TaskCreate batch key {key:?}"
                )));
            }
        }

        let now = Utc::now();
        let mut new_tasks = Vec::with_capacity(p.tasks.len());
        for (i, (input, id)) in p.tasks.iter().zip(minted.iter().copied()).enumerate() {
            let mut depends_on = parse_depends_on(input.depends_on.as_ref())?;
            for dep in &depends_on {
                if !valid.contains(dep) {
                    return Err(ToolError::InvalidParams(format!(
                        "`depends_on` references unknown task id {dep}"
                    )));
                }
                if *dep == id {
                    return Err(ToolError::InvalidParams(
                        "a task cannot depend on itself".into(),
                    ));
                }
            }
            let mut seen: HashSet<TaskId> = depends_on.iter().copied().collect();
            for raw_key in input.depends_on_keys.as_deref().unwrap_or_default() {
                let key = raw_key.trim();
                if key.is_empty() {
                    return Err(ToolError::InvalidParams(
                        "`depends_on_keys` entries must be non-empty".into(),
                    ));
                }
                let dep = ids_by_key.get(key).copied().ok_or_else(|| {
                    ToolError::InvalidParams(format!(
                        "`depends_on_keys` references unknown batch key {key:?}"
                    ))
                })?;
                if dep == id {
                    return Err(ToolError::InvalidParams(format!(
                        "task with batch key {key:?} cannot depend on itself"
                    )));
                }
                if seen.insert(dep) {
                    depends_on.push(dep);
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
            "created_by_key": ids_by_key
                .iter()
                .map(|(key, id)| (key.clone(), id.to_string()))
                .collect::<BTreeMap<_, _>>(),
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
    status: Option<String>,
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
                "status": status_filter_schema("Only return tasks with this status. Use `all` when the strict schema requires a value but no filter is wanted.")
            }
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: TaskListParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let mut tasks = self.store.list(&ctx.session_id).await.map_err(exec_err)?;
        let status = p
            .status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty() && *status != ALL_STATUSES_FILTER)
            .map(|status| {
                TaskStatus::from_storage_str(status).ok_or_else(|| {
                    ToolError::InvalidParams(format!(
                        "`status` must be one of {:?}",
                        status_values()
                    ))
                })
            })
            .transpose()?;
        if let Some(status) = status {
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

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
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
    #[serde(default)]
    clear_depends_on: bool,
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
                "status": status_or_deleted_schema("New status, `deleted` to remove the task, or `unchanged` when the strict schema requires a value but status is not changing."),
                "subject": { "type": "string", "description": "New brief title. An empty string means unchanged." },
                "description": { "type": "string", "description": "New task body. An empty string means unchanged." },
                "depends_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Replace the task's dependency ids (advisory). An empty array means unchanged; use `clear_depends_on` to remove every dependency."
                },
                "clear_depends_on": { "type": "boolean", "description": "Set true to remove every dependency. False is a no-op." }
            },
            "required": ["id"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("id")
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: TaskUpdateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let id = parse_task_id(&p.id, "id")?;

        // `status` is a raw string: a lifecycle status, the `deleted` action,
        // or absent. `deleted` removes the row instead of setting a status.
        let mut new_status: Option<TaskStatus> = None;
        if let Some(s) = p
            .status
            .as_deref()
            .map(str::trim)
            .filter(|status| !status.is_empty() && *status != UNCHANGED_ACTION)
        {
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

        let mut patch = TaskPatch::new(id);
        patch.status = new_status;
        patch.subject = p.subject.filter(|value| !value.trim().is_empty());
        patch.description = p.description.filter(|value| !value.trim().is_empty());
        let explicit_dependencies = p.depends_on.filter(|deps| !deps.is_empty());
        if p.clear_depends_on && explicit_dependencies.is_some() {
            return Err(ToolError::InvalidParams(
                "`depends_on` and `clear_depends_on: true` are mutually exclusive".into(),
            ));
        }
        if let Some(dep_strs) = &explicit_dependencies {
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
        } else if p.clear_depends_on {
            patch.depends_on = Some(Vec::new());
        }

        if patch.is_empty() {
            return Err(ToolError::InvalidParams(
                "provide at least one field to update (`status`, `subject`, `description`, `depends_on`, or `clear_depends_on`)".into(),
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
    use baybo_model::{ChannelType, User};
    use std::time::Duration;

    fn ctx(session: &str) -> ToolContext {
        ToolContext {
            session_id: session.into(),
            user: User {
                id: "u1".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            ..ToolContext::for_test()
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
    async fn create_validates_real_dependency_ids() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        let prerequisite = create_one(&s, "s1", "first").await;
        let created = json_out(
            create
                .execute(
                    json!({ "tasks": [ { "subject": "second", "description": "y", "depends_on": [&prerequisite] } ] }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        assert_eq!(created["tasks"][0]["depends_on"][0], prerequisite);

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
    async fn create_resolves_same_batch_dependencies_by_key() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        let created = json_out(
            create
                .execute(
                    json!({ "tasks": [
                        { "key": "build", "subject": "build", "description": "compile" },
                        { "key": "test", "subject": "test", "description": "verify", "depends_on_keys": ["build"] },
                        { "key": "report", "subject": "report", "description": "summarize", "depends_on_keys": ["build", "test"] }
                    ] }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );

        let build = created["created_by_key"]["build"]
            .as_str()
            .expect("build id");
        let test = created["created_by_key"]["test"].as_str().expect("test id");
        assert_eq!(created["tasks"][1]["depends_on"], json!([build]));
        assert_eq!(created["tasks"][2]["depends_on"], json!([build, test]));

        let stored = s
            .list(&baybo_model::SessionId::from("s1".to_owned()))
            .await
            .unwrap();
        assert_eq!(stored[1].depends_on[0].to_string(), build);
        assert_eq!(stored[2].depends_on[1].to_string(), test);
    }

    #[tokio::test]
    async fn create_rejects_invalid_same_batch_keys() {
        let s = store();
        let create = TaskCreateTool::new(s);
        for tasks in [
            json!([
                { "key": "same", "subject": "a", "description": "x" },
                { "key": "same", "subject": "b", "description": "y" }
            ]),
            json!([
                { "key": "a", "subject": "a", "description": "x", "depends_on_keys": ["missing"] }
            ]),
            json!([
                { "key": "self", "subject": "a", "description": "x", "depends_on_keys": ["self"] }
            ]),
        ] {
            let err = create
                .execute(json!({ "tasks": tasks }), &ctx("s1"))
                .await
                .unwrap_err();
            assert!(matches!(err, ToolError::InvalidParams(_)), "{err}");
        }
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
    async fn update_ignores_strict_schema_fillers() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        let created = json_out(
            create
                .execute(
                    json!({ "tasks": [
                        { "key": "first", "subject": "first", "description": "body" },
                        { "subject": "second", "description": "old", "status": "in_progress", "depends_on_keys": ["first"] }
                    ] }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        let id = created["created"][1].as_str().expect("id");
        let dependency = created["created"][0].clone();

        let update = TaskUpdateTool::new(Arc::clone(&s));
        let updated = json_out(
            update
                .execute(
                    json!({
                        "id": id,
                        "status": "unchanged",
                        "subject": "",
                        "description": "new",
                        "depends_on": [],
                        "clear_depends_on": false,
                    }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        assert_eq!(updated["subject"], "second");
        assert_eq!(updated["description"], "new");
        assert_eq!(updated["status"], "in_progress");
        assert_eq!(updated["depends_on"], json!([dependency]));
    }

    #[tokio::test]
    async fn update_clears_dependencies_only_with_explicit_intent() {
        let s = store();
        let create = TaskCreateTool::new(Arc::clone(&s));
        let created = json_out(
            create
                .execute(
                    json!({ "tasks": [
                        { "key": "first", "subject": "first", "description": "body" },
                        { "subject": "second", "description": "body", "depends_on_keys": ["first"] }
                    ] }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        let id = created["created"][1].as_str().expect("id");
        let update = TaskUpdateTool::new(s);
        let updated = json_out(
            update
                .execute(
                    json!({ "id": id, "depends_on": [], "clear_depends_on": true }),
                    &ctx("s1"),
                )
                .await
                .unwrap(),
        );
        assert!(updated["depends_on"].is_null(), "{updated}");
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
        let list = TaskListTool::new(Arc::clone(&s));
        let v = json_out(
            list.execute(json!({ "status": "completed" }), &ctx("s1"))
                .await
                .unwrap(),
        );
        assert_eq!(v["count"], 1);
        assert_eq!(v["tasks"][0]["subject"], "a");

        let v = json_out(
            TaskListTool::new(s)
                .execute(json!({ "status": "all" }), &ctx("s1"))
                .await
                .unwrap(),
        );
        assert_eq!(v["count"], 2);
    }
}
