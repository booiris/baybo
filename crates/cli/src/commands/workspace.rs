use aura_workspace::IdentityKind;
use serde_json::json;

use crate::cli::WorkspaceCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: WorkspaceCmd) -> Result<CommandOutput> {
    match cmd {
        WorkspaceCmd::Show => show(ctx).await,
        WorkspaceCmd::SetIdentity {
            name,
            file,
            content,
            yes,
        } => set_identity(ctx, &name, file.as_deref(), content.as_deref(), yes).await,
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

async fn set_identity(
    ctx: &CommandContext,
    name: &str,
    file: Option<&str>,
    content: Option<&str>,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would overwrite identity file '{name}'; re-run with --yes to confirm"
        )));
    }

    let kind = IdentityKind::from_label(name).ok_or_else(|| {
        CliError::Parse(format!(
            "unknown identity name '{name}'; expected one of agents, soul, user, identity"
        ))
    })?;

    let body = match (file, content) {
        (Some(path), None) => tokio::fs::read_to_string(path)
            .await
            .map_err(|e| CliError::Io(format!("read --file {path}: {e}")))?,
        (None, Some(text)) => text.to_string(),
        (Some(_), Some(_)) => {
            return Err(CliError::Parse(
                "--file and --content are mutually exclusive".into(),
            ));
        }
        (None, None) => {
            return Err(CliError::Parse(
                "--file <path> or --content <text> is required".into(),
            ));
        }
    };

    let written = ctx
        .workspace
        .write_identity_file(kind, &body)
        .await
        .map_err(|e| CliError::Manager(format!("write identity file: {e}")))?;

    let path_str = written.display().to_string();
    Ok(CommandOutput {
        human: format!(
            "wrote {} ({} bytes); restart the process to reload.",
            path_str,
            body.len()
        ),
        data: Some(json!({
            "kind": kind.file_name(),
            "path": path_str,
            "bytes": body.len(),
            "reload_required": true,
        })),
    })
}
