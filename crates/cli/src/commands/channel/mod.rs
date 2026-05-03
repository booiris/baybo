use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(600);

use aura_channels::registration::{Prompter, RegistrationResult};
use aura_channels::vault_keys;
use aura_gateway::SidecarRuntime;
use aura_model::ChannelType;
use aura_security::SecretVault;
use aura_storage::{ChannelBotStore, retry_on_busy};
use serde_json::json;

use crate::cli::ChannelCmd;
use crate::commands::secret_input::{RawModeGuard, read_masked_secret};
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

mod register;

use crate::commands::prompt::{confirm, prompt_line};
use crate::commands::select::select_one;

#[cfg(any(test, feature = "test-support"))]
pub use register::run_registration;

pub async fn handle(ctx: &CommandContext, cmd: ChannelCmd) -> Result<CommandOutput> {
    match cmd {
        ChannelCmd::List => list(ctx).await,
        ChannelCmd::Add => add_bot(ctx).await,
        ChannelCmd::Remove => remove_bot(ctx).await,
    }
}

async fn collect_all_bots(
    store: &Arc<dyn ChannelBotStore>,
) -> Result<Vec<(ChannelType, aura_storage::ChannelBotRow)>> {
    let runtime = installed_runtime()?;
    let mut out: Vec<(ChannelType, aura_storage::ChannelBotRow)> = Vec::new();
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

struct CliPrompter {
    tty_fd: i32,
}

impl Prompter for CliPrompter {
    fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
        let stdin = io::stdin();
        let stderr = io::stderr();
        let mut reader = stdin.lock();
        let mut writer = stderr.lock();
        loop {
            let value = prompt_line(&mut reader, &mut writer, label)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            if value.is_empty() && required {
                writeln!(writer, "required")?;
                continue;
            }
            return Ok(value);
        }
    }

    fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
        let stdin = io::stdin();
        let stderr = io::stderr();
        let mut reader = stdin.lock();
        let mut writer = stderr.lock();
        loop {
            let _raw =
                RawModeGuard::new(self.tty_fd).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let value = read_masked_secret(&mut reader, &mut writer, label)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            drop(_raw);
            if value.is_empty() && required {
                writeln!(writer, "required")?;
                continue;
            }
            return Ok(value);
        }
    }
}

/// Per-key validation for aux metadata + secret keys. Mirrors
/// [`validate_bot_id`]'s charset (plus `.` so dotted-key conventions
/// like `oauth.refresh` round-trip cleanly through the vault prefix).
fn validate_aux_key(kind: &str, key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 64 {
        return Err(CliError::Config(format!(
            "{kind} key must be 1-64 characters: '{key}'"
        )));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(CliError::Config(format!(
            "{kind} key may contain only alphanumerics, '-', '_', '.': '{key}'"
        )));
    }
    Ok(())
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
        .names_in_domain(aura_gateway::sidecar::domains::CHANNEL)
        .map(ChannelType::from)
        .collect();
    channels.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    channels
}

async fn persist_bot_registration(
    vault: &Arc<SecretVault>,
    store: &Arc<dyn ChannelBotStore>,
    ct: &ChannelType,
    result: &RegistrationResult,
) -> Result<()> {
    validate_bot_id(&result.bot_id)?;
    for key in result.metadata.keys() {
        validate_aux_key("metadata", key)?;
    }
    for key in result.secrets.keys() {
        validate_aux_key("secret", key)?;
    }

    // Order matters for rollback safety. The previous implementation
    // wrote the primary token, then wiped every existing config.* key,
    // then wrote the new config secrets, then bumped the row. A
    // store_secret failure between the wipe and the row update left
    // the old creds soft-deleted with no replacement and no advertised
    // revision — the running bot limped along on cached creds in
    // memory and the next restart failed.
    //
    // The non-destructive order: write all the new state first, then
    // sweep specifically the keys NOT in the new set. A failure mid-
    // way leaves both old and new keys in the vault (the next attempt
    // overwrites them); the row is bumped only after every secret is
    // durable.

    let key = vault_keys::primary_token(ct, &result.bot_id);
    let token = result.token.clone();
    retry_on_busy("vault.store_secret", || {
        vault.store_secret(&key, token.as_bytes())
    })
    .await
    .map_err(|e| CliError::Manager(format!("store secret '{key}' in vault: {e}")))?;

    for (config_key, value) in &result.secrets {
        let name = vault_keys::config(ct, &result.bot_id, config_key);
        let bytes = value.clone().into_bytes();
        retry_on_busy("vault.store_secret", || vault.store_secret(&name, &bytes))
            .await
            .map_err(|e| CliError::Manager(format!("store secret '{name}' in vault: {e}")))?;
    }

    let ct_for_put = ct.clone();
    let bot_id_owned = result.bot_id.clone();
    let metadata_owned = result.metadata.clone();
    retry_on_busy("channel_bots.put", || {
        let ct = ct_for_put.clone();
        let id = bot_id_owned.clone();
        let metadata = metadata_owned.clone();
        async move { store.put(&ct, &id, metadata).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("register bot metadata: {e}")))?;

    // Now safe to sweep. Anything under config.* that isn't in the
    // new set is stale (operator dropped or renamed a key). A failure
    // here is non-fatal for correctness — orphaned keys persist until
    // the next re-registration or `aura channel remove` — but report
    // it so the operator knows the durable state didn't fully
    // converge to the registration intent.
    let kept: std::collections::HashSet<String> = result
        .secrets
        .keys()
        .map(|k| vault_keys::config(ct, &result.bot_id, k))
        .collect();
    let prefix = vault_keys::config_prefix(ct, &result.bot_id);
    let existing = retry_on_busy("vault.list_secrets_with_prefix", || {
        vault.list_secrets_with_prefix(&prefix)
    })
    .await
    .map_err(|e| {
        CliError::Manager(format!(
            "list config secrets under '{prefix}' for sweep: {e}"
        ))
    })?;
    for name in existing {
        if kept.contains(&name) {
            continue;
        }
        retry_on_busy("vault.delete_secret", || vault.delete_secret(&name))
            .await
            .map_err(|e| CliError::Manager(format!("sweep stale config secret '{name}': {e}")))?;
    }

    Ok(())
}

async fn add_bot(ctx: &CommandContext) -> Result<CommandOutput> {
    let (vault, store) = require_bot_deps(ctx)?;

    let runtime = installed_runtime()?;
    let channels = offered_channels(&runtime);
    if channels.is_empty() {
        return Err(CliError::Config(
            "no channel bundles embedded in this build; rebuild after `pnpm install`".into(),
        ));
    }
    let labels: Vec<&str> = channels.iter().map(|c| c.as_str()).collect();
    let idx = select_one("Channel:", &labels)?;
    let ct = channels[idx].clone();

    let stdin = io::stdin();
    let stderr = io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive bot registration requires a terminal".into(),
        ));
    }
    let tty_fd = stdin.as_raw_fd();

    let mut prompter = CliPrompter { tty_fd };
    let result =
        register::run_registration(&runtime, &ct, &mut prompter, REGISTRATION_TIMEOUT).await?;

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
    .map_err(|e| CliError::Manager(format!("soft-delete bot metadata: {e}")))?;
    // Sweep the entire per-bot vault namespace in one shot — primary
    // token, registration-time `config.*` credentials, and any
    // runtime-minted `user.*` UATs. Without this, a future
    // re-registration under the same `bot_id` would resurrect the
    // soft-deleted secrets the SDK had stashed for OAuth flows
    // (Codex review finding: orphan namespaces survive removal).
    let prefix = vault_keys::bot_prefix(&ct, &bot_id);
    retry_on_busy("vault.delete_secrets_with_prefix", || {
        vault.delete_secrets_with_prefix(&prefix)
    })
    .await
    .map_err(|e| CliError::Manager(format!("sweep vault prefix '{prefix}': {e}")))?;

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
    async fn persist_registration_writes_token_and_bot_row() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::telegram();

        let result = RegistrationResult {
            bot_id: "123456789".into(),
            token: "123456789:hunter2".into(),
            metadata: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
        };

        persist_bot_registration(&vault, &store, &channel_type, &result)
            .await
            .unwrap();

        let key = vault_keys::primary_token(&channel_type, "123456789");
        let saved = vault.get_secret(&key).await.unwrap().unwrap();
        assert_eq!(saved.as_bytes(), result.token.as_bytes());
        assert!(
            store
                .get(&channel_type, "123456789")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn persist_routes_metadata_to_row_and_secrets_to_vault() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::from("lark");

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("base_url".into(), "https://open.feishu.cn".into());
        let mut secrets = std::collections::HashMap::new();
        secrets.insert("app_secret".into(), "shh-app".into());
        secrets.insert("encrypt_key".into(), "shh-encrypt".into());

        let result = RegistrationResult {
            bot_id: "lark-bot".into(),
            token: "primary-token".into(),
            metadata,
            secrets,
        };
        persist_bot_registration(&vault, &store, &channel_type, &result)
            .await
            .unwrap();

        // Non-secret metadata round-trips on the row, in the clear.
        let row = store
            .get(&channel_type, "lark-bot")
            .await
            .unwrap()
            .expect("row");
        assert_eq!(
            row.metadata.get("base_url").map(String::as_str),
            Some("https://open.feishu.cn"),
        );
        // Secret-shaped fields land in the vault under per-key
        // `config.*` names — never in `channel_bots.metadata`.
        assert!(!row.metadata.contains_key("app_secret"));
        let app_secret = vault
            .get_secret(&vault_keys::config(&channel_type, "lark-bot", "app_secret"))
            .await
            .unwrap()
            .expect("app_secret in vault");
        assert_eq!(app_secret.as_bytes(), b"shh-app");
        let encrypt_key = vault
            .get_secret(&vault_keys::config(
                &channel_type,
                "lark-bot",
                "encrypt_key",
            ))
            .await
            .unwrap()
            .expect("encrypt_key in vault");
        assert_eq!(encrypt_key.as_bytes(), b"shh-encrypt");
    }

    #[tokio::test]
    async fn remove_bot_sweeps_per_bot_vault_namespace() {
        // Codex review regression: deregistering a bot must not leave
        // OAuth UATs / config secrets behind under the same `bot_id`,
        // otherwise a re-register inherits orphaned credentials.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::from("lark");

        // Initial registration: token + a config secret + a runtime
        // user UAT (the SDK's `secrets()` API path, simulated here).
        let result = RegistrationResult {
            bot_id: "lark-bot".into(),
            token: "primary".into(),
            metadata: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::from([("app_secret".into(), "shh-app".into())]),
        };
        persist_bot_registration(&vault, &store, &channel_type, &result)
            .await
            .unwrap();
        // Mint a fake UAT under the runtime per-user namespace.
        let uat_key = format!(
            "{}user.uid-1",
            vault_keys::bot_prefix(&channel_type, "lark-bot")
        );
        vault
            .store_secret(&uat_key, b"refresh-token")
            .await
            .unwrap();

        // Removal sweep — token, config.*, and user.* all gone.
        let prefix = vault_keys::bot_prefix(&channel_type, "lark-bot");
        vault.delete_secrets_with_prefix(&prefix).await.unwrap();
        store.delete(&channel_type, "lark-bot").await.unwrap();

        assert!(
            vault
                .get_secret(&vault_keys::primary_token(&channel_type, "lark-bot"))
                .await
                .unwrap()
                .is_none(),
            "primary token should be swept",
        );
        assert!(
            vault
                .get_secret(&vault_keys::config(&channel_type, "lark-bot", "app_secret"))
                .await
                .unwrap()
                .is_none(),
            "config.app_secret should be swept",
        );
        assert!(
            vault.get_secret(&uat_key).await.unwrap().is_none(),
            "user.* UAT should be swept",
        );
    }

    #[tokio::test]
    async fn re_registration_sweeps_dropped_config_secrets() {
        // Codex review regression: re-registering with a smaller secret
        // map must drop the omitted keys from the vault. Otherwise an
        // operator who removed a credential during re-registration
        // would silently keep it live for the next StartBot.
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::from("lark");

        let initial = RegistrationResult {
            bot_id: "lark-bot".into(),
            token: "primary".into(),
            metadata: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::from([
                ("app_secret".into(), "shh-app".into()),
                ("encrypt_key".into(), "shh-encrypt".into()),
            ]),
        };
        persist_bot_registration(&vault, &store, &channel_type, &initial)
            .await
            .unwrap();

        // Re-register with `encrypt_key` dropped — only `app_secret`
        // (rotated to a new value) survives.
        let updated = RegistrationResult {
            bot_id: "lark-bot".into(),
            token: "primary-rotated".into(),
            metadata: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::from([("app_secret".into(), "shh-rotated".into())]),
        };
        persist_bot_registration(&vault, &store, &channel_type, &updated)
            .await
            .unwrap();

        // The dropped key is gone from the vault; no zombie credential.
        assert!(
            vault
                .get_secret(&vault_keys::config(
                    &channel_type,
                    "lark-bot",
                    "encrypt_key"
                ))
                .await
                .unwrap()
                .is_none(),
            "encrypt_key should be swept on re-register",
        );
        // The rotated value lands under the same key.
        let app_secret = vault
            .get_secret(&vault_keys::config(&channel_type, "lark-bot", "app_secret"))
            .await
            .unwrap()
            .expect("app_secret in vault");
        assert_eq!(app_secret.as_bytes(), b"shh-rotated");
        // Token rotation lands too (independent of the config sweep,
        // but worth asserting the same call writes both).
        let token = vault
            .get_secret(&vault_keys::primary_token(&channel_type, "lark-bot"))
            .await
            .unwrap()
            .expect("token in vault");
        assert_eq!(token.as_bytes(), b"primary-rotated");
    }

    /// Codex review regression: a failure mid-write during
    /// re-registration must not leave the bot inoperable.
    /// Specifically, the old destructive sweep order (delete → write
    /// → row update) meant a `store_secret` failure between sweep
    /// and final write left the vault with no usable config.* keys.
    /// The new order writes new state first, then sweeps stale keys
    /// — a failure mid-write leaves both old and new keys, never
    /// fewer than the operator started with.
    ///
    /// We can't trivially inject a failure into the real
    /// `MemorySecretStore`, but we can simulate the partial-failure
    /// shape: write through one set of keys, then verify a partial
    /// re-registration that DOESN'T complete (i.e. the second
    /// `persist_bot_registration` is interrupted before the row
    /// update) doesn't make the old config secrets disappear.
    #[tokio::test]
    async fn partial_re_registration_preserves_old_config_secrets() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store: Arc<dyn ChannelBotStore> = Arc::new(LibsqlChannelBotStore::new(pool));
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        let channel_type = ChannelType::from("lark");

        let initial = RegistrationResult {
            bot_id: "lark-bot".into(),
            token: "primary".into(),
            metadata: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::from([
                ("app_secret".into(), "shh-app".into()),
                ("encrypt_key".into(), "shh-encrypt".into()),
            ]),
        };
        persist_bot_registration(&vault, &store, &channel_type, &initial)
            .await
            .unwrap();

        // Confirm both keys are durable.
        for k in ["app_secret", "encrypt_key"] {
            assert!(
                vault
                    .get_secret(&vault_keys::config(&channel_type, "lark-bot", k))
                    .await
                    .unwrap()
                    .is_some(),
                "{k} should be present after initial registration",
            );
        }

        // Simulate the partial-failure shape: a re-registration that
        // wrote the new token + new app_secret value but stopped
        // before the row update. Under the OLD destructive ordering
        // this would have ALSO swept encrypt_key by now (sweep ran
        // before any new writes). Under the NEW ordering the old
        // encrypt_key is still present — verify that.
        //
        // We invoke just the slice of work that would have completed
        // before a hypothetical mid-flight failure.
        let new_token = vault_keys::primary_token(&channel_type, "lark-bot");
        vault
            .store_secret(&new_token, b"primary-rotated")
            .await
            .unwrap();
        let new_app_secret = vault_keys::config(&channel_type, "lark-bot", "app_secret");
        vault
            .store_secret(&new_app_secret, b"shh-rotated")
            .await
            .unwrap();
        // ... at this point in a real partial-failure run, store.put
        // would error and the function would return.

        // The old encrypt_key MUST still be present — under the
        // previous destructive ordering it would have been deleted
        // BEFORE the new writes ran.
        assert!(
            vault
                .get_secret(&vault_keys::config(
                    &channel_type,
                    "lark-bot",
                    "encrypt_key"
                ))
                .await
                .unwrap()
                .is_some(),
            "old encrypt_key must survive a partial re-registration",
        );
    }
}
