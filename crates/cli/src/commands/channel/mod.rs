use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use aura_channels::registration::{
    Prompter, RegistrationFlow, RegistrationResult, WeixinRegistration, builtin_registration_flows,
};
use aura_model::ChannelType;
use aura_security::SecretVault;
use aura_storage::{ChannelBotStore, retry_on_busy};
use serde_json::json;

use crate::cli::ChannelCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

mod select;
mod weixin_login;

/// Assemble the flows offered by `aura channel add` / `remove` / `bots`.
/// Starts from the builtin catalog and appends weixin when the current
/// build ships a weixin sidecar bundle (the QR-login flow delegates
/// into that bundle via bun).
fn registration_flows() -> Vec<Arc<dyn RegistrationFlow>> {
    let mut flows = builtin_registration_flows();
    match weixin_login::SidecarLoginRunner::try_new() {
        Ok(Some(runner)) => {
            flows.push(Arc::new(WeixinRegistration::new(Arc::new(runner))));
        }
        Ok(None) => {
            tracing::debug!(
                "weixin sidecar bundle absent; omitting from channel registration catalog"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "could not prepare weixin login runner; omitting from catalog"
            );
        }
    }
    flows
}

pub async fn handle(ctx: &CommandContext, cmd: ChannelCmd) -> Result<CommandOutput> {
    match cmd {
        ChannelCmd::List => list(ctx).await,
        ChannelCmd::Add => add_bot(ctx).await,
        ChannelCmd::Remove => remove_bot(ctx).await,
        ChannelCmd::Bots => list_bots(ctx).await,
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

struct CliPrompter<'a, R, W> {
    reader: &'a mut R,
    writer: &'a mut W,
    tty_fd: i32,
}

impl<R: BufRead, W: Write> Prompter for CliPrompter<'_, R, W> {
    fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
        loop {
            let value = prompt_line(self.reader, self.writer, label)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if value.is_empty() && required {
                writeln!(self.writer, "required")?;
                continue;
            }
            return Ok(value);
        }
    }

    fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
        loop {
            let _raw =
                RawModeGuard::new(self.tty_fd).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let value = read_masked_secret(self.reader, self.writer, label)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            drop(_raw);
            if value.is_empty() && required {
                writeln!(self.writer, "required")?;
                continue;
            }
            return Ok(value);
        }
    }
}

// Vault naming for the *remove* path. The add path lets each channel
// compose its own secret keys via `RegistrationResult::secrets`; when the
// remove path grows beyond a single hardcoded key, push this down into
// the channel trait too.
fn secret_name(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.token", channel_type.as_str(), bot_id)
}

async fn persist_bot_registration(
    vault: &Arc<SecretVault>,
    store: &Arc<dyn ChannelBotStore>,
    ct: &ChannelType,
    result: &RegistrationResult,
) -> Result<()> {
    validate_bot_id(&result.bot_id)?;

    // Vault first — if encryption fails we want to know before
    // advertising the bot in libsql. On success libsql picks up the
    // row and the next gateway reconcile tick pushes StartBot.
    //
    // Both writes run under `retry_on_busy`: the CLI shares the libsql
    // file with a potentially-running gateway, and a transient
    // `database is locked` should be a warn-logged retry rather than
    // an operator-facing failure. Real lock pathology escapes after
    // ~200ms with the original error so we don't silently swallow bugs.
    for (key, value) in &result.secrets {
        retry_on_busy("vault.store_secret", || {
            vault.store_secret(key, value.as_bytes())
        })
        .await
        .map_err(|e| CliError::Manager(format!("store secret '{key}' in vault: {e}")))?;
    }

    let ct_for_put = ct.clone();
    let bot_id_owned = result.bot_id.clone();
    retry_on_busy("channel_bots.put", || {
        let ct = ct_for_put.clone();
        let id = bot_id_owned.clone();
        async move { store.put(&ct, &id).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("register bot metadata: {e}")))?;

    Ok(())
}

async fn add_bot(ctx: &CommandContext) -> Result<CommandOutput> {
    let (vault, store) = require_bot_deps(ctx)?;

    let flows = registration_flows();
    let labels: Vec<&str> = flows.iter().map(|f| f.display_name()).collect();
    let idx = select::select_one("Channel:", &labels)?;
    let flow: Arc<dyn RegistrationFlow> = flows[idx].clone();
    let ct = flow.channel_type();

    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive bot registration requires a terminal".into(),
        ));
    }

    let result = {
        let mut reader = stdin.lock();
        let mut writer = stderr.lock();
        let fd = reader.as_raw_fd();
        let mut prompter = CliPrompter {
            reader: &mut reader,
            writer: &mut writer,
            tty_fd: fd,
        };
        flow.prompt(&mut prompter)
            .map_err(|e| CliError::Config(e.to_string()))?
    };

    persist_bot_registration(vault, store, &ct, &result).await?;

    let human = format!(
        "Registered {} bot '{}'. A running gateway will start it within a few seconds.",
        ct.as_str(),
        result.bot_id
    );

    Ok(CommandOutput::structured(
        human,
        &json!({
            "channel_type": ct.as_str(),
            "bot_id": result.bot_id,
            "action": "added",
        }),
    ))
}

/// Returns `None` when no bots are registered at all — callers render
/// that as a friendly success, not an error.
async fn pick_channel_with_bots(store: &Arc<dyn ChannelBotStore>) -> Result<Option<ChannelType>> {
    // Enumerate via the builtin add-flow catalog so the picker stays
    // in sync with what's registrable. A bot registered for an unknown
    // channel type would be unreachable here — acceptable while the
    // catalog is the source of truth.
    let flows = registration_flows();
    let mut populated: Vec<(String, ChannelType)> = Vec::new();
    for flow in &flows {
        let ct = flow.channel_type();
        let rows = store
            .list_live(&ct)
            .await
            .map_err(|e| CliError::Manager(format!("list live bots: {e}")))?;
        if !rows.is_empty() {
            populated.push((flow.display_name().to_string(), ct));
        }
    }

    if populated.is_empty() {
        return Ok(None);
    }

    let labels: Vec<&str> = populated.iter().map(|(n, _)| n.as_str()).collect();
    let idx = select::select_one("Channel:", &labels)?;
    Ok(Some(populated.swap_remove(idx).1))
}

async fn pick_bot(store: &Arc<dyn ChannelBotStore>, ct: &ChannelType) -> Result<String> {
    let rows = store
        .list_live(ct)
        .await
        .map_err(|e| CliError::Manager(format!("list live bots: {e}")))?;
    if rows.is_empty() {
        return Err(CliError::Config(format!("no bots for '{}'", ct.as_str())));
    }
    let labels: Vec<&str> = rows.iter().map(|r| r.bot_id.as_str()).collect();
    let idx = select::select_one("Bot:", &labels)?;
    Ok(rows[idx].bot_id.clone())
}

fn confirm(question: &str) -> Result<bool> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive confirmation requires a terminal".into(),
        ));
    }
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    let label = format!("{question} [y/N]: ");
    let ans = prompt_line(&mut reader, &mut writer, &label)?;
    Ok(matches!(ans.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

async fn remove_bot(ctx: &CommandContext) -> Result<CommandOutput> {
    let (vault, store) = require_bot_deps(ctx)?;
    let Some(ct) = pick_channel_with_bots(store).await? else {
        return Ok(CommandOutput::structured(
            "no bots to remove".to_string(),
            &json!({ "bots": [], "action": "noop" }),
        ));
    };
    let bot_id = pick_bot(store, &ct).await?;

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

async fn list_bots(ctx: &CommandContext) -> Result<CommandOutput> {
    let (_vault, store) = require_bot_deps(ctx)?;
    let Some(ct) = pick_channel_with_bots(store).await? else {
        return Ok(CommandOutput::structured(
            "no bots registered".to_string(),
            &json!({ "bots": [] }),
        ));
    };
    let rows = store
        .list_live(&ct)
        .await
        .map_err(|e| CliError::Manager(format!("list live bots: {e}")))?;
    let human = if rows.is_empty() {
        format!("(no bots for '{}')", ct.as_str())
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
    async fn persist_registration_writes_every_secret_and_bot_row() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::telegram();

        let result = RegistrationResult {
            bot_id: "123456789".into(),
            secrets: vec![
                (
                    "channel.telegram.bot.123456789.token".into(),
                    "123456789:hunter2".into(),
                ),
                (
                    "channel.telegram.bot.123456789.webhook".into(),
                    "https://example.test/hook".into(),
                ),
            ],
        };

        persist_bot_registration(&vault, &store, &channel_type, &result)
            .await
            .unwrap();

        for (key, value) in &result.secrets {
            let saved = vault.get_secret(key).await.unwrap().unwrap();
            assert_eq!(saved.as_bytes(), value.as_bytes());
        }
        assert!(
            store
                .get(&channel_type, "123456789")
                .await
                .unwrap()
                .is_some()
        );
    }
}
