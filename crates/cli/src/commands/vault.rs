//! `baybo vault rotate` — re-key the secret vault.
//!
//! Terminal-only. The gateway-must-be-stopped requirement is enforced inside
//! `rotate_master_key`, which holds the workspace singleton lock for the whole
//! operation rather than checking it here and hoping.

use baybo_workspace::WorkspacePaths;
use serde_json::json;

use crate::cli::VaultCmd;
use crate::commands::prompt::confirm;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: VaultCmd) -> Result<CommandOutput> {
    match cmd {
        VaultCmd::Rotate { yes } => rotate(ctx, yes).await,
    }
}

async fn rotate(ctx: &CommandContext, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation != Invocation::Argv {
        return Err(CliError::Config(
            "`vault rotate` re-keys the whole vault; run it from a shell".into(),
        ));
    }

    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root with a valid baybo.json".into(),
        )
    })?;
    let paths = WorkspacePaths::new(ctx.workspace.root.clone());

    let count = vault
        .list_names()
        .await
        .map_err(|e| CliError::Config(format!("read vault: {e}")))?
        .len();

    if !yes && !ctx.confirmed {
        let proceed = confirm(&format!(
            "Re-encrypt {count} vault entries under a new master key?\n\
             The old key stops working the moment this completes — anything holding a copy \
             (backups of .key/encryption.key, another machine) can no longer read this vault."
        ))?;
        if !proceed {
            return Ok(CommandOutput::structured(
                "rotation cancelled",
                &json!({ "rotated": false }),
            ));
        }
    }

    let rotated = baybo_setup::rotate::rotate_master_key(&paths, vault)
        .await
        .map_err(|e| CliError::Config(format!("rotate master key: {e}")))?;

    Ok(CommandOutput::structured(
        format!(
            "re-encrypted {} vault entries under a new master key\n\
             note: placeholders are derived from the master key, so a secret seen again will \
             mint a new placeholder; existing ones keep resolving",
            rotated.entries
        ),
        &json!({ "rotated": true, "entries": rotated.entries }),
    ))
}
