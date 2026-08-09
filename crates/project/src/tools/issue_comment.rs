use std::sync::Arc;

use async_trait::async_trait;
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{actor, project_err, scope};
use crate::{CommentDelivery, ProjectManager};

pub const ISSUE_COMMENT_TOOL_NAME: &str = "IssueComment";

pub(super) struct IssueCommentTool {
    manager: Arc<ProjectManager>,
}

impl IssueCommentTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    number: i64,
    text: String,
}

#[async_trait]
impl Tool for IssueCommentTool {
    fn name(&self) -> &str {
        ISSUE_COMMENT_TOOL_NAME
    }

    fn description(&self) -> String {
        r#"Say something on an issue's timeline. This is how you report what you found, ask its assignee a question, explain why you are blocked, or ask somebody to merge your branch — the card is what a person reads, and what the next run on that issue is told about.

A comment on a card somebody is assigned to **reaches them**: if they are idle it wakes them, and if they are mid-run it is picked up when that run finishes. On an unassigned card, or one in Backlog or Done, it is recorded and nobody is woken. The result says which of those happened, so you know whether to expect an answer."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "The issue's number on this board." },
                "text": { "type": "string", "description": "What you want to say. Written for a person reading the card." },
            },
            "required": ["number", "text"],
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("number")
            .and_then(Value::as_i64)
            .map(|n| format!("#{n}"))
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::ProjectBoard
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let project = scope(ctx)?;
        let delivery = self
            .manager
            .comment_delivery(&project, p.number)
            .await
            .map_err(project_err)?;
        self.manager
            .comment(&project, p.number, actor(ctx), &p.text)
            .await
            .map_err(project_err)?;
        Ok(ToolOutput::Json(json!({
            "recorded": true,
            "delivery": describe(delivery),
        })))
    }
}

fn describe(delivery: CommentDelivery) -> &'static str {
    match delivery {
        CommentDelivery::RecordOnly => {
            "recorded only — nobody is assigned, or the issue is parked, so no one was woken"
        }
        CommentDelivery::Wake => "the assignee was woken and will read this",
        CommentDelivery::WaitsForQueuedRun => {
            "a run is already queued and will read this when it starts"
        }
        CommentDelivery::AfterCurrentRun => {
            "the assignee is mid-run; this is picked up when that run finishes"
        }
    }
}
