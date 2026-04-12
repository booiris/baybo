use serde_json::json;

use crate::cli::WorkspaceCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: WorkspaceCmd) -> Result<CommandOutput> {
    match cmd {
        WorkspaceCmd::Show => show(ctx).await,
    }
}

async fn show(ctx: &CommandContext) -> Result<CommandOutput> {
    let identity = ctx
        .workspace
        .load_identity_files()
        .await
        .map_err(|e| CliError::Manager(format!("load identity files: {e}")))?;

    let value = json!({
        "identity_files": {
            "agents": identity.agents.is_some(),
            "soul": identity.soul.is_some(),
            "user": identity.user.is_some(),
            "identity": identity.identity.is_some(),
        }
    });

    let flag = |present: bool| if present { "present" } else { "missing" };
    let human = format!(
        "identity files:\n  AGENTS.md   {}\n  SOUL.md     {}\n  USER.md     {}\n  IDENTITY.md {}",
        flag(identity.agents.is_some()),
        flag(identity.soul.is_some()),
        flag(identity.user.is_some()),
        flag(identity.identity.is_some()),
    );
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
