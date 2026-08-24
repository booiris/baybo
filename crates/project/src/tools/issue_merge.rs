use std::sync::Arc;

use async_trait::async_trait;
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{actor, project_err, scope};
use crate::ProjectManager;
use crate::worktree::Merged;

pub const ISSUE_MERGE_TOOL_NAME: &str = "IssueMerge";

/// What the model is told this tool does.
///
/// It says outright that some boards refuse, rather than describing a verb
/// every board has: the registry filters tools by trigger, not by project,
/// so this is offered on every board, and the only honest place to say
/// "yours may not" is the description the model reads before calling it.
const DESCRIPTION: &str = r#"Land an issue's branch in the repository's own checkout, once its work has been reviewed.

Not every board merges its own work. Where yours does not, this refuses and says so — that is policy, not a failure, and the branch is what the board hands over instead.

Commit everything to the branch first: uncommitted work in your checkout is refused rather than left out of what lands. Merging does not move the card."#;

pub(super) struct IssueMergeTool {
    manager: Arc<ProjectManager>,
}

impl IssueMergeTool {
    pub(super) fn new(manager: Arc<ProjectManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    number: i64,
}

#[async_trait]
impl Tool for IssueMergeTool {
    fn name(&self) -> &str {
        ISSUE_MERGE_TOOL_NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "number": { "type": "integer", "description": "The issue's number on this board." },
            },
            "required": ["number"],
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
        let merged = self
            .manager
            .merge_issue_branch(&project, p.number, actor(ctx))
            .await
            .map_err(project_err)?;
        Ok(ToolOutput::Json(describe(merged)))
    }
}

/// A refusal is a normal answer here, not an error: an agent told "this
/// board does not merge" has something to do next, while a tool error reads
/// as a fault worth retrying with the same arguments.
fn describe(merged: Merged) -> Value {
    match merged {
        Merged::Landed {
            into,
            commit,
            commits,
        } => json!({
            "merged": true,
            "into": into,
            "commit": commit,
            "commits": commits,
        }),
        Merged::AlreadyThere { into } => json!({
            "merged": false,
            "already_merged": true,
            "into": into,
        }),
        Merged::Refused { reason, retryable } => json!({
            "merged": false,
            "refused": reason,
            "retryable": retryable,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three shapes are distinguishable without reading prose: an agent
    /// that has to parse a sentence to learn whether its branch landed will
    /// eventually parse one wrong.
    #[test]
    fn every_outcome_answers_the_merged_question_first() {
        let landed = describe(Merged::Landed {
            into: "master".to_owned(),
            commit: "abc123".to_owned(),
            commits: 3,
        });
        assert_eq!(landed["merged"], json!(true));
        assert_eq!(landed["into"], json!("master"));

        let already = describe(Merged::AlreadyThere {
            into: "master".to_owned(),
        });
        assert_eq!(already["merged"], json!(false));
        assert_eq!(already["already_merged"], json!(true));

        let refused = describe(Merged::Refused {
            reason: "nope".to_owned(),
            retryable: true,
        });
        assert_eq!(refused["merged"], json!(false));
        assert_eq!(refused["refused"], json!("nope"));
        assert_eq!(refused["retryable"], json!(true));
    }

    /// The board-level refusal has to be readable as policy rather than as
    /// breakage, because it is the answer on every board that has not opted
    /// in — which is most of them.
    #[test]
    fn the_description_says_some_boards_refuse() {
        assert!(
            DESCRIPTION.contains("Not every board merges its own work"),
            "{DESCRIPTION}"
        );
        assert!(
            DESCRIPTION.contains("policy, not a failure"),
            "a refusal read as breakage is one the model retries or escalates: {DESCRIPTION}"
        );
        assert!(
            DESCRIPTION.contains("reviewed"),
            "the description must name the precondition a merge is asked under: {DESCRIPTION}"
        );
    }
}
