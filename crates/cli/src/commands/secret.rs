//! `aura secret add | list | delete` — user-managed env-style secrets.
//!
//! The value is only ever read through masked terminal input (never an argv
//! or slash argument, which would leak into shell history / the transcript),
//! so `add` is terminal-only. `list` shows a masked preview; the full value is
//! never printed. Deletion is operator-only here — there is deliberately no
//! agent-facing delete tool. See `docs/todo/secret-management.md`.

use std::io::{BufReader, stderr, stdin};
use std::sync::Arc;

use aura_security::{AddOutcome, UserSecretManager};
use serde_json::json;

use crate::cli::SecretCmd;
use crate::commands::prompt::{confirm, prompt_line};
use crate::commands::secret_input::read_masked_password;
use crate::commands::select::select_one;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

fn manager(ctx: &CommandContext) -> Result<UserSecretManager> {
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root with a valid aura.json".into(),
        )
    })?;
    Ok(UserSecretManager::new(Arc::clone(vault)))
}

pub async fn handle(ctx: &CommandContext, cmd: SecretCmd) -> Result<CommandOutput> {
    match cmd {
        SecretCmd::Add { name, force } => add(ctx, name, force).await,
        SecretCmd::List => list(ctx).await,
        SecretCmd::Delete { name, yes } => delete(ctx, name, yes).await,
    }
}

async fn add(ctx: &CommandContext, name: Option<String>, force: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash {
        return Err(CliError::NotAvailableInSlash(
            "secret add reads a masked value from the terminal — run `aura secret add`".into(),
        ));
    }
    let mgr = manager(ctx)?;

    let name = match name {
        Some(n) => n,
        None => {
            let mut reader = BufReader::new(stdin());
            let mut err = stderr();
            prompt_line(&mut reader, &mut err, "Secret name: ")?
        }
    };
    UserSecretManager::validate_name(&name).map_err(|e| CliError::Config(e.to_string()))?;

    let exists = mgr
        .exists(&name)
        .await
        .map_err(|e| CliError::Manager(e.to_string()))?;
    if exists && !force && !confirm(&format!("Secret '{name}' already exists. Overwrite?"))? {
        return Ok(CommandOutput::structured(
            format!("Kept existing secret '{name}'."),
            &json!({ "name": name, "action": "kept" }),
        ));
    }

    let value = read_masked_password(&format!("Value for '{name}': "))?;
    let outcome = mgr
        .add(&name, value.as_bytes(), true)
        .await
        .map_err(|e| CliError::Manager(e.to_string()))?;
    let action = match outcome {
        AddOutcome::Created => "created",
        AddOutcome::Replaced => "replaced",
    };
    Ok(CommandOutput::structured(
        format!("Secret '{name}' {action}."),
        &json!({ "name": name, "action": action }),
    ))
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let mgr = manager(ctx)?;
    let rows = mgr
        .list_with_previews()
        .await
        .map_err(|e| CliError::Manager(e.to_string()))?;
    if rows.is_empty() {
        return Ok(CommandOutput::structured(
            "No secrets stored.",
            &json!({ "secrets": [] }),
        ));
    }
    let mut human = String::from("NAME\tPREVIEW\n");
    for (name, preview) in &rows {
        human.push_str(&format!("{name}\t{preview}\n"));
    }
    let data = json!({
        "secrets": rows
            .iter()
            .map(|(name, preview)| json!({ "name": name, "preview": preview }))
            .collect::<Vec<_>>()
    });
    Ok(CommandOutput::structured(
        human.trim_end().to_string(),
        &data,
    ))
}

async fn delete(ctx: &CommandContext, name: Option<String>, yes: bool) -> Result<CommandOutput> {
    let mgr = manager(ctx)?;

    let name = match name {
        Some(n) => {
            if !mgr
                .exists(&n)
                .await
                .map_err(|e| CliError::Manager(e.to_string()))?
            {
                return Err(CliError::Manager(format!("secret '{n}' not found")));
            }
            n
        }
        None => {
            if ctx.invocation == Invocation::Slash {
                return Err(CliError::NotAvailableInSlash(
                    "secret delete needs a NAME in slash mode (the no-arg picker is terminal-only)"
                        .into(),
                ));
            }
            let names = mgr
                .list()
                .await
                .map_err(|e| CliError::Manager(e.to_string()))?;
            if names.is_empty() {
                return Ok(CommandOutput::structured(
                    "No secrets to delete.",
                    &json!({ "action": "noop" }),
                ));
            }
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let idx = select_one("Delete which secret?", &refs)?;
            names[idx].clone()
        }
    };

    if ctx.invocation == Invocation::Slash {
        if !(yes || ctx.confirmed) {
            return Err(CliError::ConfirmationRequired(format!(
                "deleting secret '{name}' is destructive — pass --yes"
            )));
        }
    } else if !yes && !confirm(&format!("Delete secret '{name}'?"))? {
        return Ok(CommandOutput::structured(
            format!("Kept secret '{name}'."),
            &json!({ "name": name, "action": "kept" }),
        ));
    }

    mgr.delete(&name)
        .await
        .map_err(|e| CliError::Manager(e.to_string()))?;
    Ok(CommandOutput::structured(
        format!("Deleted secret '{name}'."),
        &json!({ "name": name, "action": "deleted" }),
    ))
}
