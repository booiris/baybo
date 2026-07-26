//! `baybo vault rotate` — re-key the secret vault.
//!
//! Terminal-only. Rotation rewrites every ciphertext and replaces the key file;
//! a gateway running against the same workspace would write entries under the
//! old key, outside the snapshot being re-encrypted, and they would be
//! unreadable once the key is promoted. The workspace singleton lock is what
//! tells us whether that is the case.

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

/// Is another process holding the workspace? Acquiring the advisory lock and
/// immediately dropping it is the same probe the llm command uses.
fn gateway_is_running(paths: &WorkspacePaths) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(paths.singleton_lock())
    else {
        return false;
    };
    file.try_lock().is_err()
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

    if gateway_is_running(&paths) {
        return Err(CliError::Config(
            "a baybo process is holding this workspace — stop the gateway before rotating, or \
             entries it writes mid-rotation become unreadable"
                .into(),
        ));
    }

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
