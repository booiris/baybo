//! `baybo vault rotate` — re-key the secret vault.
//!
//! Terminal-only. The gateway-must-be-stopped requirement is enforced by
//! `key_file::rotate` taking the workspace lock as a parameter — this command
//! acquires it and holds it for the whole call, so a gateway can neither be
//! running nor start midway.

use serde_json::json;

use crate::cli::VaultCmd;
use crate::commands::secret_input::read_masked_password;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: VaultCmd) -> Result<CommandOutput> {
    match cmd {
        VaultCmd::Rotate { yes } => rotate(ctx, yes).await,
    }
}

/// Gate rotation on the operator producing the key that is about to be retired.
///
/// A `[y/N]`, or even a typed word, only proves someone pressed a key. This
/// proves they hold the outgoing key somewhere other than this disk — which is
/// the thing that matters, because rotation is what makes every other copy of
/// it useless. An operator who cannot produce it is one who was relying on the
/// file alone, and that is exactly who should not be retiring it today.
///
/// Read masked: it is a credential, and a shell that records history should not
/// see it.
fn confirm_by_producing_current_key(count: usize, key_path: &str) -> Result<bool> {
    eprintln!(
        "About to re-encrypt {count} vault entries under a new master key.\n\
         \n\
           key file : {key_path}\n\
         \n\
         This cannot be undone: the current key stops opening this vault the moment\n\
         it completes, so every other copy of it becomes useless. A backup of the\n\
         outgoing key and the current ciphertext is written first — see the path\n\
         printed on success.\n"
    );
    let current = baybo_security::key_file::load(std::path::Path::new(key_path))
        .map_err(|e| CliError::Config(format!("read current key: {e}")))?;
    let typed = read_masked_password("Enter the CURRENT master key (hex) to confirm: ")?;

    let Ok(bytes) = hex::decode(typed.trim()) else {
        eprintln!("that is not hex — nothing was changed");
        return Ok(false);
    };
    if bytes != current.as_bytes() {
        eprintln!("that is not the current master key — nothing was changed");
        return Ok(false);
    }
    Ok(true)
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

    let key_path = ctx
        .config
        .security
        .encryption_key_file
        .as_ref()
        .ok_or_else(|| CliError::Config("security.encryption_key_file is not set".into()))?;

    // Taken before the prompt so a running gateway is reported immediately,
    // rather than after the operator has already confirmed something that
    // cannot happen. Held for the whole rotation, not probed: a gateway
    // starting midway would write an entry under the outgoing key and lose it
    // at promotion.
    let lock = baybo_workspace::acquire_workspace_lock(ctx.workspace.root()).map_err(|e| {
        CliError::Config(format!(
            "cannot rotate while this workspace is in use — stop the gateway first ({e})"
        ))
    })?;

    let count = vault
        .list_names()
        .await
        .map_err(|e| CliError::Config(format!("read vault: {e}")))?
        .len();

    if !yes && !ctx.confirmed && !confirm_by_producing_current_key(count, key_path)? {
        return Ok(CommandOutput::structured(
            "rotation cancelled",
            &json!({ "rotated": false }),
        ));
    }

    let backup_dir = ctx.workspace.state_dir().join(format!(
        "vault-rotation-backup-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));

    let rotated =
        baybo_security::key_file::rotate(std::path::Path::new(key_path), vault, &backup_dir, &lock)
            .await
            .map_err(|e| CliError::Config(format!("rotate master key: {e}")))?;

    Ok(CommandOutput::structured(
        format!(
            "re-encrypted {} vault entries under a new master key\n\
             \n\
             backup  : {}\n\
             restore : cp <backup>/encryption.key {key_path} && sqlite3 <db> < <backup>/secrets.sql\n\
             \n\
             Delete the backup once you have confirmed the workspace works — it holds the\n\
             outgoing key and the ciphertext it opens.\n\
             Note: placeholders are derived from the master key, so a secret seen again\n\
             mints a new placeholder; existing ones keep resolving.",
            rotated.entries,
            rotated.backup_dir.display(),
        ),
        &json!({
            "rotated": true,
            "entries": rotated.entries,
            "backup_dir": rotated.backup_dir.display().to_string(),
        }),
    ))
}
