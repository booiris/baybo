//! Stub implementations for Claude Code tools whose backing subsystems are
//! not yet available in Aura.
//!
//! Each stub implements the [`Tool`] trait and returns
//! [`ToolError::NotImplemented`] when called. They exist so that:
//!
//! 1. The tool surface shape is documented in one place and stays in sync
//!    with `https://code.claude.com/docs/en/tools-reference`.
//! 2. A follow-up can register a stub temporarily to surface the tool name
//!    to the LLM and degrade gracefully until the real implementation lands.
//!
//! Stubs are NOT auto-registered by [`crate::builtin::default_tools`].

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, ToolOutput};

macro_rules! todo_tool {
    (
        $ident:ident,
        name = $name:literal,
        desc = $desc:literal,
        reason = $reason:literal
        $(,)?
    ) => {
        #[doc = concat!("Stub for the `", $name, "` tool. ", $reason)]
        pub struct $ident;

        #[async_trait]
        impl Tool for $ident {
            fn name(&self) -> &str {
                $name
            }

            fn description(&self) -> &str {
                concat!($desc, " (TODO: ", $reason, ")")
            }

            fn parameters_schema(&self) -> Value {
                json!({ "type": "object", "additionalProperties": true })
            }

            async fn execute(
                &self,
                _params: Value,
                _ctx: &ToolContext,
            ) -> crate::Result<ToolOutput> {
                Err(ToolError::NotImplemented(concat!($name, ": ", $reason).into()))
            }
        }
    };
}

// -- Subagent / interaction ---------------------------------------------------
todo_tool!(
    AgentTool,
    name = "Agent",
    desc = "Spawn a subagent with its own context window.",
    reason = "subagent dispatch is not yet wired to AgentSupervisor"
);

todo_tool!(
    AskUserQuestionTool,
    name = "AskUserQuestion",
    desc = "Ask the user a multiple-choice clarifying question.",
    reason = "requires a channel-level interactive question bridge"
);

todo_tool!(
    SendMessageTool,
    name = "SendMessage",
    desc = "Send a message to an agent-team teammate or resume a subagent.",
    reason = "agent teams are experimental and not yet modeled in Aura"
);

// -- Plan / worktree ---------------------------------------------------------
todo_tool!(
    EnterPlanModeTool,
    name = "EnterPlanMode",
    desc = "Switch to plan mode to design an approach before coding.",
    reason = "plan mode is not yet represented in the agent state machine"
);
todo_tool!(
    ExitPlanModeTool,
    name = "ExitPlanMode",
    desc = "Present a plan for approval and exit plan mode.",
    reason = "plan mode is not yet represented in the agent state machine"
);
todo_tool!(
    EnterWorktreeTool,
    name = "EnterWorktree",
    desc = "Create and switch into an isolated git worktree.",
    reason = "worktree management is not yet available"
);
todo_tool!(
    ExitWorktreeTool,
    name = "ExitWorktree",
    desc = "Exit a worktree session and return to the original directory.",
    reason = "worktree management is not yet available"
);

// -- Code intelligence / monitoring ------------------------------------------
todo_tool!(
    LspTool,
    name = "LSP",
    desc = "Language-server-powered jump-to-def, references, diagnostics.",
    reason = "no LSP host is wired in yet"
);
todo_tool!(
    MonitorTool,
    name = "Monitor",
    desc = "Run a command in the background and stream output back to the agent.",
    reason = "requires background-task streaming in the agent loop"
);
todo_tool!(
    NotebookEditTool,
    name = "NotebookEdit",
    desc = "Edit Jupyter notebook cells.",
    reason = "notebook parsing/serialization is not yet implemented"
);

// -- Skill invocation --------------------------------------------------------
todo_tool!(
    SkillTool,
    name = "Skill",
    desc = "Execute a declared skill within the current conversation.",
    reason = "tool-driven skill invocation is intentionally routed via the agent loop, not the Tool path"
);

// -- Task list ---------------------------------------------------------------
todo_tool!(
    TaskCreateTool,
    name = "TaskCreate",
    desc = "Create a new task in the session task list.",
    reason = "session task list is not yet modeled"
);
todo_tool!(
    TaskGetTool,
    name = "TaskGet",
    desc = "Retrieve full details for a specific task.",
    reason = "session task list is not yet modeled"
);
todo_tool!(
    TaskListTool,
    name = "TaskList",
    desc = "List all tasks with their current status.",
    reason = "session task list is not yet modeled"
);
todo_tool!(
    TaskUpdateTool,
    name = "TaskUpdate",
    desc = "Update task status, dependencies, or details.",
    reason = "session task list is not yet modeled"
);
todo_tool!(
    TaskStopTool,
    name = "TaskStop",
    desc = "Kill a running background task by ID.",
    reason = "background task runtime is not yet modeled"
);
todo_tool!(
    TaskOutputTool,
    name = "TaskOutput",
    desc = "(Deprecated) Retrieve output from a background task.",
    reason = "background task runtime is not yet modeled; prefer Read on the task's output file when implemented"
);
todo_tool!(
    TodoWriteTool,
    name = "TodoWrite",
    desc = "Replace the session task checklist in non-interactive mode.",
    reason = "session task list is not yet modeled"
);

// -- Tool search -------------------------------------------------------------
todo_tool!(
    ToolSearchTool,
    name = "ToolSearch",
    desc = "Search for and load deferred tools.",
    reason = "deferred-tool registry is not yet modeled"
);

// -- Web search --------------------------------------------------------------
todo_tool!(
    WebSearchTool,
    name = "WebSearch",
    desc = "Perform a web search.",
    reason = "no search-engine provider is configured"
);

// -- Agent teams -------------------------------------------------------------
todo_tool!(
    TeamCreateTool,
    name = "TeamCreate",
    desc = "Create an agent team.",
    reason = "agent teams are experimental and not yet modeled in Aura"
);
todo_tool!(
    TeamDeleteTool,
    name = "TeamDelete",
    desc = "Disband an agent team.",
    reason = "agent teams are experimental and not yet modeled in Aura"
);

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, User};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn stub_returns_not_implemented() {
        let ctx = ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(1),
            cancellation_token: CancellationToken::new(),
        };
        let err = AgentTool.execute(json!({}), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::NotImplemented(_)));
    }
}
