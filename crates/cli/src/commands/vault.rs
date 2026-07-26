//! `baybo vault rotate` — re-key the secret vault.
//!
//! Terminal-only. The gateway-must-be-stopped requirement is enforced by
//! `key_file::rotate` taking the workspace lock as a parameter — this command
//! acquires it and holds it for the whole call, so a gateway can neither be
//! running nor start midway.

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

    let key_path = ctx
        .config
        .security
        .encryption_key_file
        .as_ref()
        .ok_or_else(|| CliError::Config("security.encryption_key_file is not set".into()))?;

    // Held for the whole rotation, not probed: a gateway starting midway would
    // write an entry under the outgoing key and lose it at promotion.
    let lock = baybo_workspace::acquire_workspace_lock(&ctx.workspace.root).map_err(|e| {
        CliError::Config(format!(
            "cannot rotate while this workspace is in use — stop the gateway first ({e})"
        ))
    })?;

    let entries = baybo_security::key_file::rotate(std::path::Path::new(key_path), vault, &lock)
        .await
        .map_err(|e| CliError::Config(format!("rotate master key: {e}")))?;

    Ok(CommandOutput::structured(
        format!(
            "re-encrypted {} vault entries under a new master key\n\
             note: placeholders are derived from the master key, so a secret seen again will \
             mint a new placeholder; existing ones keep resolving",
            entries
        ),
        &json!({ "rotated": true, "entries": entries }),
    ))
}
