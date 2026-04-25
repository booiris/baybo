use serde_json::json;

use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

use super::{load_mcp_file, require_vault, vault_keys, workspace_root, write_mcp_file};

pub async fn run(ctx: &CommandContext, name: String, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !(yes || ctx.confirmed) {
        return Err(CliError::ConfirmationRequired(format!(
            "removing mcp server '{name}' is destructive — pass --yes"
        )));
    }

    let workspace = workspace_root(ctx);
    let mut file = load_mcp_file(&workspace).await?;
    let removed = file
        .remove(&name)
        .ok_or_else(|| CliError::Manager(format!("mcp server '{name}' not found")))?;

    write_mcp_file(&file, &workspace).await?;

    let vault = require_vault(ctx)?;
    for key in vault_keys::all_keys_for(&name) {
        if let Err(e) = vault.delete_secret(&key).await {
            tracing::warn!(
                key = %key,
                error = %e,
                "failed to delete vault secret on mcp removal — continuing"
            );
        }
    }

    let human = format!(
        "Removed MCP server '{}'. The reconciler will drop its tools within a few seconds.",
        removed.name
    );
    Ok(CommandOutput::structured(
        human,
        &json!({
            "name": removed.name,
            "transport": removed.transport.kind_name(),
            "action": "removed",
        }),
    ))
}
