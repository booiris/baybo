//! LLM-facing tools for managing cron jobs. Require an `Arc<CronScheduler>`
//! to manipulate the global scheduler state.

use std::sync::Arc;

use async_trait::async_trait;
use aura_tools::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CronSchedule, CronScheduler, CronStatus, TriggerAction};

/// Build every cron tool with its manifest, ready to be registered with a
/// `ToolRegistry`. Always `Trusted` with no capabilities — they operate on
/// agent-internal scheduler state, not on the host filesystem or network.
pub fn agent_tools(scheduler: Arc<CronScheduler>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(CronCreateTool::new(Arc::clone(&scheduler))),
        Arc::new(CronDeleteTool::new(Arc::clone(&scheduler))),
        Arc::new(CronListTool::new(scheduler)),
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

// ---------------------------------------------------------------------------
// CronCreate
// ---------------------------------------------------------------------------

struct CronCreateTool {
    scheduler: Arc<CronScheduler>,
}

impl CronCreateTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[derive(Debug, Deserialize)]
struct CreateParams {
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    at: Option<String>,
    prompt: Option<String>,
    tool: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> &str {
        "Schedule a cron job. Exactly one of `schedule` (recurring cron \
         expression, e.g. \"0 9 * * *\") or `at` (one-shot RFC3339 UTC \
         timestamp, e.g. \"2026-04-17T14:25:00Z\") is required — `at` jobs \
         fire once then auto-delete. Exactly one of `prompt` (sends text \
         through the LLM on trigger) or `tool` (directly invokes a registered \
         tool, with optional `params`) is required."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "Recurring cron expression (mutually exclusive with `at`)"
                },
                "at": {
                    "type": "string",
                    "description": "One-shot RFC3339 UTC timestamp; fires once and auto-deletes (mutually exclusive with `schedule`)"
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt text sent to the LLM on each trigger (mutually exclusive with `tool`)"
                },
                "tool": {
                    "type": "string",
                    "description": "Registered tool name to invoke directly (mutually exclusive with `prompt`)"
                },
                "params": {
                    "type": "object",
                    "description": "JSON parameters for the tool (requires `tool`)"
                }
            }
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: CreateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let schedule = match (p.schedule, p.at) {
            (Some(_), Some(_)) => {
                return Err(ToolError::InvalidParams(
                    "`schedule` and `at` are mutually exclusive".into(),
                ));
            }
            (Some(expr), None) => CronSchedule::cron(expr),
            (None, Some(at)) => {
                let parsed: DateTime<Utc> = at
                    .parse()
                    .map_err(|e| ToolError::InvalidParams(format!("`at` must be RFC3339: {e}")))?;
                CronSchedule::at(parsed)
            }
            (None, None) => {
                return Err(ToolError::InvalidParams(
                    "either `schedule` or `at` is required".into(),
                ));
            }
        };

        let action = match p.tool {
            Some(tool_name) => TriggerAction::ToolCall {
                tool_name,
                params: p.params.unwrap_or(json!({})),
                approved_resources: Vec::new(),
            },
            None => {
                let text = p.prompt.ok_or_else(|| {
                    ToolError::InvalidParams("either `prompt` or `tool` is required".into())
                })?;
                TriggerAction::Prompt { prompt: text }
            }
        };

        let job = self
            .scheduler
            .create_job(
                &ctx.user.id,
                ctx.user.channel,
                schedule,
                action,
                Some(ctx.session_id.clone()),
            )
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({
            "id": job.id,
            "schedule": job.schedule.display(),
            "one_shot": job.is_one_shot(),
            "status": "enabled",
            "next_trigger_at": job.next_trigger_at.map(|t| t.to_rfc3339()),
        })))
    }
}

// ---------------------------------------------------------------------------
// CronDelete
// ---------------------------------------------------------------------------

struct CronDeleteTool {
    scheduler: Arc<CronScheduler>,
}

impl CronDeleteTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[derive(Debug, Deserialize)]
struct DeleteParams {
    id: String,
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> &str {
        "Cancel and remove a scheduled cron job by its ID."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Cron job ID to delete" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: DeleteParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        self.scheduler
            .delete_job(&p.id)
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({ "deleted": p.id })))
    }
}

// ---------------------------------------------------------------------------
// CronList
// ---------------------------------------------------------------------------

struct CronListTool {
    scheduler: Arc<CronScheduler>,
}

impl CronListTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }

    fn description(&self) -> &str {
        "List all scheduled cron jobs with their status and next trigger time."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let mut jobs = self
            .scheduler
            .list_all_jobs()
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let rows: Vec<Value> = jobs
            .iter()
            .map(|j| {
                json!({
                    "id": j.id,
                    "user": j.user_id,
                    "channel": format!("{}", j.channel),
                    "schedule": j.schedule.display(),
                    "one_shot": j.is_one_shot(),
                    "status": match j.status {
                        CronStatus::Enabled => "enabled",
                        CronStatus::Disabled => "disabled",
                    },
                    "next_trigger_at": j.next_trigger_at.map(|t| t.to_rfc3339()),
                })
            })
            .collect();

        Ok(ToolOutput::Json(json!({ "jobs": rows })))
    }
}
