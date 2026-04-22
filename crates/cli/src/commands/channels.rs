use std::io::{self, BufRead, IsTerminal, Write};

use aura_model::ChannelType;
use aura_security::SecretVault;
use aura_storage::{ChannelBotStore, retry_on_busy};
use serde_json::json;

use crate::cli::{ChannelBotCmd, ChannelsCmd};
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: ChannelsCmd) -> Result<CommandOutput> {
    match cmd {
        ChannelsCmd::List => list(ctx).await,
        ChannelsCmd::Bot { cmd } => handle_bot(ctx, cmd).await,
    }
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let entries: Vec<String> = ctx
        .channels
        .list()
        .into_iter()
        .map(|ct| ct.to_string())
        .collect();
    let human = if entries.is_empty() {
        "(no channels registered)".to_string()
    } else {
        let mut buf = String::from("CHANNEL\n");
        for ct in &entries {
            buf.push_str(&format!("{ct}\n"));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "channels": entries
                .iter()
                .map(|ct| json!({ "channel": ct }))
                .collect::<Vec<_>>(),
        }),
    ))
}

async fn handle_bot(ctx: &CommandContext, cmd: ChannelBotCmd) -> Result<CommandOutput> {
    match cmd {
        ChannelBotCmd::Add {
            channel_type,
            bot_id,
            token_env,
        } => add_bot(ctx, channel_type, bot_id, token_env).await,
        ChannelBotCmd::Remove {
            channel_type,
            bot_id,
        } => remove_bot(ctx, channel_type, bot_id).await,
        ChannelBotCmd::List { channel_type } => list_bots(ctx, channel_type).await,
    }
}

fn require_bot_deps(
    ctx: &CommandContext,
) -> Result<(
    &std::sync::Arc<SecretVault>,
    &std::sync::Arc<dyn ChannelBotStore>,
)> {
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root with a valid aura.json".into(),
        )
    })?;
    let store = ctx.channel_bot_store.as_ref().ok_or_else(|| {
        CliError::Config(
            "channel-bot store unavailable — run from the workspace root with a valid aura.json"
                .into(),
        )
    })?;
    Ok((vault, store))
}

fn validate_bot_id(bot_id: &str) -> Result<()> {
    // Stable bot ids end up in libsql primary keys, vault secret names
    // (`channel.<ct>.bot.<id>.token`), and Telegram callback_data
    // payloads. Restrict to a conservative charset so no caller has to
    // think about quoting.
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

fn read_token(token_env: Option<String>) -> Result<String> {
    if let Some(var) = token_env {
        let value = std::env::var(&var)
            .map_err(|_| CliError::Config(format!("env var '{var}' is not set")))?;
        if value.is_empty() {
            return Err(CliError::Config(format!("env var '{var}' is empty")));
        }
        return Ok(value);
    }
    // Fall through to stdin — operator pipes in the token. Avoids
    // putting the secret in shell history, and stays friendly to CI
    // (echo "$TOKEN" | aura channels bot add ...). Refuse to prompt
    // interactively on a tty because we don't want to echo.
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Err(CliError::Config(
            "no token source: pipe the token on stdin or pass --token-env VAR".into(),
        ));
    }
    let mut buf = String::new();
    stdin
        .lock()
        .read_line(&mut buf)
        .map_err(|e| CliError::Config(format!("failed to read token from stdin: {e}")))?;
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        return Err(CliError::Config("stdin produced an empty token".into()));
    }
    Ok(trimmed)
}

fn secret_name(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.token", channel_type.as_str(), bot_id)
}

async fn add_bot(
    ctx: &CommandContext,
    channel_type: String,
    bot_id: String,
    token_env: Option<String>,
) -> Result<CommandOutput> {
    validate_bot_id(&bot_id)?;
    let (vault, store) = require_bot_deps(ctx)?;
    let ct = ChannelType::from(channel_type.as_str());
    let token = read_token(token_env)?;

    // Vault first — if encryption fails we want to know before
    // advertising the bot in libsql. On success libsql picks up the
    // row and the next gateway reconcile tick pushes StartBot.
    //
    // Both writes run under `retry_on_busy`: the CLI shares the libsql
    // file with a potentially-running gateway, and a transient
    // `database is locked` should be a warn-logged retry rather than
    // an operator-facing failure. Real lock pathology escapes after
    // ~200ms with the original error so we don't silently swallow bugs.
    let secret_name_owned = secret_name(&ct, &bot_id);
    retry_on_busy("vault.store_secret", || {
        vault.store_secret(&secret_name_owned, token.as_bytes())
    })
    .await
    .map_err(|e| CliError::Manager(format!("store token in vault: {e}")))?;
    let bot_id_owned = bot_id.clone();
    let ct_for_put = ct.clone();
    retry_on_busy("channel_bots.put", || {
        let ct = ct_for_put.clone();
        let id = bot_id_owned.clone();
        async move { store.put(&ct, &id).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("register bot metadata: {e}")))?;

    // Don't echo the token back.
    let _ = io::stdout().flush();
    let human = format!(
        "Registered {} bot '{}'. A running gateway will start it within a few seconds.",
        ct.as_str(),
        bot_id
    );
    Ok(CommandOutput::structured(
        human,
        &json!({
            "channel_type": ct.as_str(),
            "bot_id": bot_id,
            "action": "added",
        }),
    ))
}

async fn remove_bot(
    ctx: &CommandContext,
    channel_type: String,
    bot_id: String,
) -> Result<CommandOutput> {
    validate_bot_id(&bot_id)?;
    let (vault, store) = require_bot_deps(ctx)?;
    let ct = ChannelType::from(channel_type.as_str());

    // Retry both writes on transient libsql BUSY (see `add_bot` for
    // why). Metadata goes first so a running reconciler sees the bot
    // disappear and pushes StopBot before we strip the token.
    let ct_for_del = ct.clone();
    let bot_id_owned = bot_id.clone();
    retry_on_busy("channel_bots.delete", || {
        let ct = ct_for_del.clone();
        let id = bot_id_owned.clone();
        async move { store.delete(&ct, &id).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("soft-delete bot metadata: {e}")))?;
    let secret_name_owned = secret_name(&ct, &bot_id);
    retry_on_busy("vault.delete_secret", || {
        vault.delete_secret(&secret_name_owned)
    })
    .await
    .map_err(|e| CliError::Manager(format!("soft-delete vault token: {e}")))?;

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

async fn list_bots(ctx: &CommandContext, channel_type: String) -> Result<CommandOutput> {
    let (_vault, store) = require_bot_deps(ctx)?;
    let ct = ChannelType::from(channel_type.as_str());
    let rows = store
        .list_live(&ct)
        .await
        .map_err(|e| CliError::Manager(format!("list live bots: {e}")))?;
    let human = if rows.is_empty() {
        format!("(no bots registered for '{}')", ct.as_str())
    } else {
        let mut buf = String::from("BOT_ID\tCREATED_AT\n");
        for r in &rows {
            buf.push_str(&format!("{}\t{}\n", r.bot_id, r.created_at));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "channel_type": ct.as_str(),
            "bots": rows
                .iter()
                .map(|r| json!({
                    "bot_id": r.bot_id,
                    "created_at": r.created_at,
                }))
                .collect::<Vec<_>>(),
        }),
    ))
}
