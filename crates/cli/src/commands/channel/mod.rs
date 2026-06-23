use std::sync::Arc;

use baybo_gateway::SidecarRuntime;
use baybo_model::ChannelType;
use baybo_security::SecretVault;
use baybo_storage::retry::retry_on_busy;
use baybo_store::ChannelBotStore;
use serde_json::json;

use crate::cli::ChannelCmd;
use crate::commands::prompt::confirm;
use crate::commands::select::select_one;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: ChannelCmd) -> Result<CommandOutput> {
    match cmd {
        ChannelCmd::List => list(ctx).await,
        ChannelCmd::Add => add_bot(ctx).await,
        ChannelCmd::Remove => remove_bot(ctx).await,
    }
}

async fn collect_all_bots(
    store: &Arc<dyn ChannelBotStore>,
) -> Result<Vec<(ChannelType, baybo_store::ChannelBotRow)>> {
    let runtime = installed_runtime()?;
    let mut out: Vec<(ChannelType, baybo_store::ChannelBotRow)> = Vec::new();
    for ct in offered_channels(&runtime) {
        let rows = store
            .list_live(&ct)
            .await
            .map_err(|e| CliError::Manager(format!("list live bots: {e}")))?;
        for row in rows {
            out.push((ct.clone(), row));
        }
    }
    Ok(out)
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let (_vault, store) = require_bot_deps(ctx)?;
    let bots = collect_all_bots(store).await?;
    let human = if bots.is_empty() {
        "(no bots registered)".to_string()
    } else {
        let mut buf = String::from("BOT_ID\tCHANNEL\n");
        for (ct, row) in &bots {
            buf.push_str(&format!("{}\t{}\n", row.bot_id, ct.as_str()));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "bots": bots
                .iter()
                .map(|(ct, row)| json!({
                    "bot_id": row.bot_id,
                    "channel_type": ct.as_str(),
                    "created_at": row.created_at,
                }))
                .collect::<Vec<_>>(),
        }),
    ))
}

fn require_bot_deps(
    ctx: &CommandContext,
) -> Result<(
    &std::sync::Arc<SecretVault>,
    &std::sync::Arc<dyn ChannelBotStore>,
)> {
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root with a valid baybo.json".into(),
        )
    })?;
    let store = ctx.channel_bot_store.as_ref().ok_or_else(|| {
        CliError::Config(
            "channel-bot store unavailable — run from the workspace root with a valid baybo.json"
                .into(),
        )
    })?;
    Ok((vault, store))
}

fn validate_bot_id(bot_id: &str) -> Result<()> {
    if bot_id.is_empty() || bot_id.len() > 64 {
        return Err(CliError::Config("bot_id must be 1-64 characters".into()));
    }
    if !bot_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(CliError::Config(
            "bot_id may contain only alphanumerics, '-', and '_'".into(),
        ));
    }
    Ok(())
}

fn secret_name(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.token", channel_type.as_str(), bot_id)
}

fn installed_runtime() -> Result<SidecarRuntime> {
    SidecarRuntime::install().map_err(|e| {
        CliError::Config(format!(
            "sidecar runtime unavailable ({e}); rebuild with an embedded bundle"
        ))
    })
}

fn offered_channels(runtime: &SidecarRuntime) -> Vec<ChannelType> {
    let mut channels: Vec<ChannelType> = runtime
        .names_in_domain(baybo_gateway::sidecar::domains::CHANNEL)
        .map(ChannelType::from)
        .collect();
    channels.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    channels
}

async fn add_bot(ctx: &CommandContext) -> Result<CommandOutput> {
    let (vault, store) = require_bot_deps(ctx)?;
    let runtime = installed_runtime()?;

    let mut prompter = baybo_setup::TtyPrompter::new()?;
    // `allow_skip = false` suppresses the picker — the call always
    // runs the add flow and returns `Added`.
    let baybo_setup::flow::ChannelStepOutcome::Added(registered) =
        baybo_setup::flow::configure_channel_step(&mut prompter, &runtime, store, vault, false)
            .await?
    else {
        unreachable!("configure_channel_step(allow_skip=false) must return Added");
    };

    let channel_type = registered.channel_type.as_str();
    let human = format!(
        "Registered {channel_type} bot '{}'. A running gateway will start it within a few seconds.",
        registered.bot_id,
    );

    Ok(CommandOutput::structured(
        human,
        &json!({
            "channel_type": channel_type,
            "bot_id": registered.bot_id,
            "action": "added",
        }),
    ))
}

async fn remove_bot(ctx: &CommandContext) -> Result<CommandOutput> {
    let (vault, store) = require_bot_deps(ctx)?;
    let bots = collect_all_bots(store).await?;
    if bots.is_empty() {
        return Ok(CommandOutput::structured(
            "no bots to remove".to_string(),
            &json!({ "bots": [], "action": "noop" }),
        ));
    }
    let labels: Vec<String> = bots
        .iter()
        .map(|(ct, row)| format!("{} ({})", row.bot_id, ct.as_str()))
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let idx = select_one("Bot:", &label_refs)?;
    let (ct, row) = &bots[idx];
    let ct = ct.clone();
    let bot_id = row.bot_id.clone();

    let question = format!("Delete {}/{}?", ct.as_str(), bot_id);
    if !confirm(&question)? {
        return Ok(CommandOutput::structured(
            format!("Cancelled: {}/{} not removed.", ct.as_str(), bot_id),
            &json!({
                "channel_type": ct.as_str(),
                "bot_id": bot_id,
                "action": "cancelled",
            }),
        ));
    }

    validate_bot_id(&bot_id)?;

    let ct_for_del = ct.clone();
    let bot_id_owned = bot_id.clone();
    retry_on_busy("channel_bots.delete", || {
        let ct = ct_for_del.clone();
        let id = bot_id_owned.clone();
        async move { store.delete(&ct, &id).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("delete bot metadata: {e}")))?;
    let secret_name_owned = secret_name(&ct, &bot_id);
    retry_on_busy("vault.delete_secret", || {
        vault.delete_secret(&secret_name_owned)
    })
    .await
    .map_err(|e| CliError::Manager(format!("delete vault token: {e}")))?;

    let human = format!(
        "Deregistered {} bot '{}'. A running gateway will stop it within a few seconds.",
        ct.as_str(),
        bot_id
    );
    Ok(CommandOutput::structured(
        human,
        &json!({
            "channel_type": ct.as_str(),
            "bot_id": bot_id,
            "action": "removed",
        }),
    ))
}

// Tests for the masked-secret reader and channel-bot persistence
// moved with the helpers into `baybo-setup` (see
// `crates/setup/src/tty.rs` and `crates/setup/src/flow/channel/`).
