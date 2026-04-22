use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;

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
        ChannelBotCmd::Add { channel_type } => add_bot(ctx, channel_type).await,
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

fn prompt_line<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> Result<String> {
    writer.write_all(label.as_bytes())?;
    writer.flush()?;
    let mut buf = String::new();
    let bytes = reader
        .read_line(&mut buf)
        .map_err(|e| CliError::Config(format!("failed to read interactive input: {e}")))?;
    if bytes == 0 {
        return Err(CliError::Io(
            "stdin closed while reading interactive input".into(),
        ));
    }
    Ok(buf.trim().to_string())
}

struct RawModeGuard {
    fd: i32,
    original: libc::termios,
}

impl RawModeGuard {
    fn new(fd: i32) -> Result<Self> {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `termios` points to valid, writable memory for libc to fill.
        let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
        if rc != 0 {
            return Err(CliError::Io(format!(
                "failed to read terminal mode: {}",
                io::Error::last_os_error()
            )));
        }

        // SAFETY: `tcgetattr` succeeded and fully initialized `termios`.
        let original = unsafe { termios.assume_init() };
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;

        // SAFETY: `raw` is a valid termios struct for the same file descriptor.
        let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
        if rc != 0 {
            return Err(CliError::Io(format!(
                "failed to enable raw terminal mode: {}",
                io::Error::last_os_error()
            )));
        }

        Ok(Self { fd, original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: restoring a previously captured termios for the same fd.
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

fn read_masked_secret<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    label: &str,
) -> Result<String> {
    writer.write_all(label.as_bytes())?;
    writer.flush()?;

    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader
            .read(&mut byte)
            .map_err(|e| CliError::Config(format!("failed to read token input: {e}")))?;
        if n == 0 {
            writer.write_all(b"\n")?;
            writer.flush()?;
            return Err(CliError::Io("stdin closed while reading token".into()));
        }

        match byte[0] {
            b'\n' | b'\r' => {
                writer.write_all(b"\n")?;
                writer.flush()?;
                break;
            }
            0x08 | 0x7f => {
                if buf.pop().is_some() {
                    writer.write_all(b"\x08 \x08")?;
                    writer.flush()?;
                }
            }
            b => {
                buf.push(b);
                writer.write_all(b"*")?;
                writer.flush()?;
            }
        }
    }

    String::from_utf8(buf).map_err(|e| CliError::Config(format!("token must be valid utf-8: {e}")))
}

fn prompt_bot_registration() -> Result<(String, String)> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive bot registration requires a terminal".into(),
        ));
    }

    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    let bot_id = prompt_line(&mut reader, &mut writer, "bot_id: ")?;
    let _raw_mode = RawModeGuard::new(reader.as_raw_fd())?;
    let token = read_masked_secret(&mut reader, &mut writer, "token (empty for \"\"): ")?;
    Ok((bot_id, token))
}

fn secret_name(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.token", channel_type.as_str(), bot_id)
}

async fn persist_bot_registration(
    vault: &Arc<SecretVault>,
    store: &Arc<dyn ChannelBotStore>,
    ct: &ChannelType,
    bot_id: &str,
    token: &str,
) -> Result<()> {
    validate_bot_id(bot_id)?;

    // Vault first — if encryption fails we want to know before
    // advertising the bot in libsql. On success libsql picks up the
    // row and the next gateway reconcile tick pushes StartBot.
    //
    // Both writes run under `retry_on_busy`: the CLI shares the libsql
    // file with a potentially-running gateway, and a transient
    // `database is locked` should be a warn-logged retry rather than
    // an operator-facing failure. Real lock pathology escapes after
    // ~200ms with the original error so we don't silently swallow bugs.
    let secret_name_owned = secret_name(ct, bot_id);
    retry_on_busy("vault.store_secret", || {
        vault.store_secret(&secret_name_owned, token.as_bytes())
    })
    .await
    .map_err(|e| CliError::Manager(format!("store token in vault: {e}")))?;

    let bot_id_owned = bot_id.to_string();
    let ct_for_put = ct.clone();
    retry_on_busy("channel_bots.put", || {
        let ct = ct_for_put.clone();
        let id = bot_id_owned.clone();
        async move { store.put(&ct, &id).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("register bot metadata: {e}")))?;

    Ok(())
}

async fn add_bot(ctx: &CommandContext, channel_type: String) -> Result<CommandOutput> {
    let (vault, store) = require_bot_deps(ctx)?;
    let ct = ChannelType::from(channel_type.as_str());
    let (bot_id, token) = prompt_bot_registration()?;

    persist_bot_registration(vault, store, &ct, &bot_id, &token).await?;

    let human = if token.is_empty() {
        format!(
            "Registered {} bot '{}'. Stored an empty token; a running gateway will push that empty token to the sidecar within a few seconds.",
            ct.as_str(),
            bot_id
        )
    } else {
        format!(
            "Registered {} bot '{}'. A running gateway will start it within a few seconds.",
            ct.as_str(),
            bot_id
        )
    };

    Ok(CommandOutput::structured(
        human,
        &json!({
            "channel_type": ct.as_str(),
            "bot_id": bot_id,
            "action": "added",
            "token_is_empty": token.is_empty(),
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use aura_security::EncryptionKey;
    use aura_storage::libsql::{LibsqlChannelBotStore, LibsqlPool};
    use aura_storage::test_support::MemorySecretStore;

    use super::*;

    #[test]
    fn masked_secret_allows_empty_string() {
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        let token =
            read_masked_secret(&mut input, &mut output, "token (empty for \"\"): ").unwrap();
        assert!(token.is_empty());
    }

    #[test]
    fn masked_secret_handles_backspace() {
        let mut input = Cursor::new(b"ab\x7fc\n".to_vec());
        let mut output = Vec::new();
        let token =
            read_masked_secret(&mut input, &mut output, "token (empty for \"\"): ").unwrap();
        assert_eq!(token, "ac");
    }

    #[tokio::test]
    async fn persist_registration_stores_empty_token_string() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::telegram();

        persist_bot_registration(&vault, &store, &channel_type, "bot_alpha", "")
            .await
            .unwrap();

        let saved = vault
            .get_secret(&secret_name(&channel_type, "bot_alpha"))
            .await
            .unwrap()
            .unwrap();
        assert!(saved.as_bytes().is_empty());
        assert!(
            store
                .get(&channel_type, "bot_alpha")
                .await
                .unwrap()
                .is_some()
        );
    }
}
