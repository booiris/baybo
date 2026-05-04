//! Canonical layout for per-bot vault keys.
//!
//! Every persistent credential the gateway holds for a channel bot
//! lives under a deterministic prefix so the secret-vault layer can
//! enforce ownership scoping (sidecars can't claim keys outside their
//! `(channel_type, bot_id)` namespace) and the CLI can sweep the
//! entire per-bot subtree on removal in one prefix scan.
//!
//! Layout:
//! ```text
//! channel.<channel_type>.bot.<bot_id>.token         primary credential
//! channel.<channel_type>.bot.<bot_id>.config.<key>  registration-time aux secrets
//! channel.<channel_type>.bot.<bot_id>.user.<key>    runtime SDK `secrets()` mints
//! ```
//!
//! Every consumer (CLI registration / removal, gateway StartBot
//! merger, `Frame::SecretRequest` handler) routes through this module
//! so a rename is a compile error on every side at once.

use aura_model::ChannelType;

/// Vault key for the bot's primary token.
pub fn primary_token(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.token", channel_type.as_str(), bot_id)
}

/// Vault key for one registration-time auxiliary credential
/// (Lark `app_secret`, `encrypt_key`, `verification_token`, …).
pub fn config(channel_type: &ChannelType, bot_id: &str, key: &str) -> String {
    format!(
        "channel.{}.bot.{}.config.{}",
        channel_type.as_str(),
        bot_id,
        key,
    )
}

/// Vault key for one runtime per-user secret minted via the SDK's
/// `secrets()` API (Lark UATs, OAuth refresh tokens).
pub fn user(channel_type: &ChannelType, bot_id: &str, key: &str) -> String {
    format!(
        "channel.{}.bot.{}.user.{}",
        channel_type.as_str(),
        bot_id,
        key,
    )
}

/// Prefix shared by every key under one bot's namespace. The CLI's
/// `aura channel remove` sweeps the whole subtree with this prefix so
/// re-registration under the same `bot_id` doesn't inherit orphans.
pub fn bot_prefix(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.", channel_type.as_str(), bot_id)
}

/// Prefix shared by every registration-time `config.*` key.
pub fn config_prefix(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.config.", channel_type.as_str(), bot_id)
}

/// Prefix shared by every runtime `user.*` key.
pub fn user_prefix(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.user.", channel_type.as_str(), bot_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_layered_under_bot_prefix() {
        let ct = ChannelType::from("lark");
        let bot = "lark-bot";
        let prefix = bot_prefix(&ct, bot);
        assert!(primary_token(&ct, bot).starts_with(&prefix));
        assert!(config(&ct, bot, "app_secret").starts_with(&prefix));
        assert!(user(&ct, bot, "uid-1").starts_with(&prefix));
        assert!(config_prefix(&ct, bot).starts_with(&prefix));
        assert!(user_prefix(&ct, bot).starts_with(&prefix));
    }
}
