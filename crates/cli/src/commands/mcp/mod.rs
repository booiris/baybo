use std::path::PathBuf;
use std::sync::Arc;

use aura_security::SecretVault;
use aura_tools::mcp::config::McpFile;

use crate::cli::McpCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

mod add;
mod get;
mod list;
mod probe;
mod remove;

pub(crate) use aura_tools::mcp::vault_keys;

pub async fn handle(ctx: &CommandContext, cmd: McpCmd) -> Result<CommandOutput> {
    match cmd {
        McpCmd::Add {
            transport,
            envs,
            headers,
            client_id,
            client_secret,
            callback_port,
            trust_level,
            name,
            command_or_url,
            args,
        } => {
            add::run(
                ctx,
                add::AddArgs {
                    transport,
                    envs,
                    headers,
                    client_id,
                    client_secret,
                    callback_port,
                    trust_level,
                    name,
                    command_or_url,
                    args,
                },
            )
            .await
        }
        McpCmd::List { no_probe } => list::run(ctx, !no_probe).await,
        McpCmd::Get { name, no_probe } => get::run(ctx, name, !no_probe).await,
        McpCmd::Remove { name, yes } => remove::run(ctx, name, yes).await,
    }
}

/// Resolve the workspace root the same way `boot` does: parse
/// `config.workspace.path`. `.mcp.json` lives at `<root>/config/.mcp.json`
/// (resolved by `McpFile::path_for`).
pub(super) fn workspace_root(ctx: &CommandContext) -> PathBuf {
    PathBuf::from(&ctx.config.workspace.path)
}

pub(super) fn require_vault(ctx: &CommandContext) -> Result<&Arc<SecretVault>> {
    ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root with a valid aura.json".into(),
        )
    })
}

pub(super) async fn load_mcp_file(workspace: &std::path::Path) -> Result<McpFile> {
    McpFile::load(workspace)
        .await
        .map_err(|e| CliError::Config(format!("failed to load .mcp.json: {e}")))
}

pub(super) async fn write_mcp_file(file: &McpFile, workspace: &std::path::Path) -> Result<()> {
    file.write(workspace)
        .await
        .map_err(|e| CliError::Manager(format!("failed to write .mcp.json: {e}")))
}
