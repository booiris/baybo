use std::collections::HashMap;

pub trait Prompter: Send {
    fn input(&mut self, label: &str, required: bool) -> anyhow::Result<String>;
    fn password(&mut self, label: &str, required: bool) -> anyhow::Result<String>;
}

/// Outcome of a sidecar's registration flow.
///
/// Auxiliary config the sidecar collected at registration time is split
/// across two maps so credentials never land on disk in plaintext:
///
/// * `metadata` — non-secret operator-visible config (Lark `base_url`,
///   Discord intents bitmask, etc.). Persisted as a JSON object on the
///   `channel_bots.metadata` column for direct SQL inspection.
/// * `secrets` — secret-valued auxiliary credentials beyond the primary
///   `token` (Lark `app_secret` / `encrypt_key` / `verification_token`,
///   Slack signing-secret, …). Persisted in [`aura_security::SecretVault`]
///   under per-bot keys (`channel.<channel_type>.bot.<bot_id>.config.<key>`),
///   AES-GCM encrypted at rest, redacted in logs.
///
/// At runtime the gateway decrypts every `secrets` entry and merges
/// them with `metadata` into a single `Frame::StartBot.metadata` map
/// the sidecar consumes, so callers downstream don't need to know
/// where each value came from.
///
/// Both maps are optional in the wire format (`#[serde(default)]`),
/// so single-secret sidecars (Telegram, Weixin) and pre-Lark CLIs
/// keep round-tripping unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationResult {
    pub bot_id: String,
    pub token: String,
    pub metadata: HashMap<String, String>,
    pub secrets: HashMap<String, String>,
}
