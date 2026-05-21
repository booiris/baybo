//! Channel-bot registration step.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::registration::{Prompter as ChannelPrompter, RegistrationResult};
use aura_gateway::SidecarRuntime;
use aura_model::ChannelType;
use aura_security::SecretVault;
use aura_storage::retry::retry_on_busy;
use aura_store::ChannelBotStore;

use crate::error::{Result, SetupError};
use crate::prompt::Prompter;

mod register_driver;
// Public so integration tests can drive a real sidecar bundle without a TTY.
pub use register_driver::run_registration;

pub const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredBot {
    pub channel_type: ChannelType,
    pub bot_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelStepOutcome {
    Added(RegisteredBot),
    Skipped,
}

pub async fn configure_channel_step<P: Prompter>(
    prompter: &mut P,
    runtime: &SidecarRuntime,
    bot_store: &Arc<dyn ChannelBotStore>,
    vault: &Arc<SecretVault>,
    allow_skip: bool,
) -> Result<ChannelStepOutcome> {
    let channels = offered_channels(runtime);
    if channels.is_empty() {
        return Err(SetupError::Channel(
            "no channel bundles embedded in this build; rebuild after `pnpm install`".into(),
        ));
    }

    let has_existing = !collect_existing(bot_store, &channels).await?.is_empty();
    let label = if has_existing {
        "Channel step: an existing bot is already registered."
    } else {
        "Channel step:"
    };
    let add_label = if has_existing {
        "Add another channel bot"
    } else {
        "Configure a channel bot now"
    };
    if !crate::flow::pick_add_or_skip(
        prompter,
        label,
        add_label,
        "Skip — configure later with `aura channel add`",
        allow_skip,
    )? {
        return Ok(ChannelStepOutcome::Skipped);
    }

    let labels: Vec<&str> = channels.iter().map(|c| c.as_str()).collect();
    let idx = prompter.select("Channel:", &labels)?;
    let ct = channels[idx].clone();

    let mut adapter = SetupChannelPrompter { inner: prompter };
    let result = run_registration(runtime, &ct, &mut adapter, REGISTRATION_TIMEOUT).await?;

    persist_bot_registration(vault, bot_store, &ct, &result).await?;

    Ok(ChannelStepOutcome::Added(RegisteredBot {
        channel_type: ct,
        bot_id: result.bot_id,
    }))
}

fn offered_channels(runtime: &SidecarRuntime) -> Vec<ChannelType> {
    let mut channels: Vec<ChannelType> = runtime
        .names_in_domain(aura_gateway::sidecar::domains::CHANNEL)
        .map(ChannelType::from)
        .collect();
    channels.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    channels
}

async fn collect_existing(
    store: &Arc<dyn ChannelBotStore>,
    channels: &[ChannelType],
) -> Result<Vec<(ChannelType, aura_store::ChannelBotRow)>> {
    let mut out: Vec<(ChannelType, aura_store::ChannelBotRow)> = Vec::new();
    for ct in channels {
        let rows = store
            .list_live(ct)
            .await
            .map_err(|e| SetupError::Channel(format!("list live bots: {e}")))?;
        for row in rows {
            out.push((ct.clone(), row));
        }
    }
    Ok(out)
}

fn validate_bot_id(bot_id: &str) -> Result<()> {
    if bot_id.is_empty() || bot_id.len() > 64 {
        return Err(SetupError::Channel("bot_id must be 1-64 characters".into()));
    }
    if !bot_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(SetupError::Channel(
            "bot_id may contain only alphanumerics, '-', and '_'".into(),
        ));
    }
    Ok(())
}

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

    let key = secret_name(ct, &result.bot_id);
    let token = result.token.clone();
    retry_on_busy("vault.store_secret", || {
        vault.store_secret(&key, token.as_bytes())
    })
    .await
    .map_err(|e| SetupError::Vault(format!("store secret '{key}': {e}")))?;

    let ct_for_put = ct.clone();
    let bot_id_owned = result.bot_id.clone();
    retry_on_busy("channel_bots.put", || {
        let ct = ct_for_put.clone();
        let id = bot_id_owned.clone();
        async move { store.put(&ct, &id).await }
    })
    .await
    .map_err(|e| SetupError::Channel(format!("register bot metadata: {e}")))?;

    Ok(())
}

/// Adapter: turns the wizard's `Prompter` into the wire-protocol's
/// `Prompter` (different signature: returns `anyhow::Result`, takes a
/// `required: bool`). Empty input is re-prompted on `required = true`.
struct SetupChannelPrompter<'a, P: Prompter> {
    inner: &'a mut P,
}

impl<P: Prompter> ChannelPrompter for SetupChannelPrompter<'_, P> {
    fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
        loop {
            let v = self
                .inner
                .text(label, "")
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if v.is_empty() && required {
                tracing::info!("(required field)");
                continue;
            }
            return Ok(v);
        }
    }

    fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String> {
        loop {
            let v = self
                .inner
                .password(label)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if v.is_empty() && required {
                tracing::info!("(required field)");
                continue;
            }
            return Ok(v);
        }
    }
}
