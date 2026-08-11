use std::sync::Arc;

use async_trait::async_trait;
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{project_err, scope};
use crate::{MAX_TEAM_AGENTS, NewTeamMember, ProjectManager};

pub const PROJECT_AGENT_CREATE_TOOL_NAME: &str = "ProjectAgentCreate";

pub(super) struct ProjectAgentCreateTool {
    manager: Arc<ProjectManager>,
}

impl ProjectAgentCreateTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    name: String,
    role: String,
}

#[async_trait]
impl Tool for ProjectAgentCreateTool {
    fn name(&self) -> &str {
        PROJECT_AGENT_CREATE_TOOL_NAME
    }

    fn description(&self) -> String {
        format!(
            r#"Add a new agent to this project's team, so there is somebody to assign work that nobody here can do. The name you give it **is** its `@handle`, and it can never be changed afterwards — not by you, not by the operator, not by the agent itself — so name it as it should be addressed for good.

Hiring is not free and it is not reversible from your side: only the operator can remove an agent, and a handle stays reserved on this board forever. Ask an existing teammate first. Hire when the gap is a real capability the team lacks, not a momentary queue — a board is capped at {MAX_TEAM_AGENTS} agents.

The `role` becomes the new agent's own soul: write what it is for, in its own terms, as the standing description of its job — not the one issue that prompted you to create it."#
        )
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The agent's handle, and its name: lowercase letters, digits and '-', starting with a letter, e.g. \"test-engineer\". Permanent. If it is taken, a number is appended.",
                },
                "role": {
                    "type": "string",
                    "description": "One or two lines saying what this agent is for. It seeds the agent's soul, so write it as its standing job.",
                },
            },
            "required": ["name", "role"],
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("name")
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::ProjectBoard
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let project = scope(ctx)?;
        let hired = self
            .manager
            .hire(
                &project,
                NewTeamMember {
                    name: p.name,
                    role: p.role,
                    framework: None,
                    llm: None,
                },
                // The audit line the whole cap exists to make readable: this
                // hire names the agent that made it, so a hiring loop is
                // visible on the profile card rather than only in a log.
                Some(ctx.agent_id.clone()),
            )
            .await
            .map_err(project_err)?;
        let handle = hired
            .team
            .as_ref()
            .map(|team| format!("@{}", team.handle))
            .unwrap_or_default();
        Ok(ToolOutput::Json(json!({
            "handle": handle,
            "role": hired.description,
        })))
    }
}
