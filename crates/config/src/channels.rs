use serde::{Deserialize, Serialize};

/// Channel adapter configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Built-in CLI channel settings.
    pub cli: CliChannelConfig,
    /// Optional Telegram channel settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramChannelConfig>,
    /// Optional Discord channel settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordChannelConfig>,
    /// Optional HTTP channel settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpChannelConfig>,
    /// Size of the internal mpsc message buffer.
    pub message_buffer_size: usize,
    /// Per-user pairing gate settings. See `docs/modules/pairing.md`.
    pub pairing: PairingConfig,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            cli: CliChannelConfig::default(),
            telegram: None,
            discord: None,
            http: None,
            message_buffer_size: 256,
            pairing: PairingConfig::default(),
        }
    }
}

/// Per-user pairing gate settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PairingConfig {
    /// How long a pending pairing code stays valid, in seconds. A user
    /// whose row has expired will be re-minted a fresh code the next
    /// time they message the bot; an operator trying to approve an
    /// expired code gets `not found`. Default: 900 (15 minutes).
    pub pending_ttl_seconds: u64,
}

impl Default for PairingConfig {
    fn default() -> Self {
        Self {
            pending_ttl_seconds: 900,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CliChannelConfig {
    pub enabled: bool,
}

impl Default for CliChannelConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    /// Name of the environment variable holding the bot token.
    pub bot_token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordChannelConfig {
    pub enabled: bool,
    /// Name of the environment variable holding the bot token.
    pub bot_token_env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpChannelConfig {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
}
