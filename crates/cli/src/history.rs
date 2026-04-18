//! Secret-vault-backed [`InputHistoryStore`] for the TUI.
//!
//! TUI input often contains secrets (API keys, tokens, prompts that quote
//! credentials), so the persistent ring is encrypted at rest by routing
//! it through [`SecretVault`] under a fixed name. The payload is the
//! chronological history serialized as JSON.

use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use aura_channels::InputHistoryStore;
use aura_security::SecretVault;

/// Vault key under which the TUI input history is stored.
const TUI_HISTORY_SECRET_NAME: &str = "aura.tui.input_history";

pub struct CliInputHistoryStore {
    vault: Arc<SecretVault>,
}

impl CliInputHistoryStore {
    pub fn new(vault: Arc<SecretVault>) -> Self {
        Self { vault }
    }
}

#[async_trait]
impl InputHistoryStore for CliInputHistoryStore {
    async fn load(&self) -> anyhow::Result<Vec<String>> {
        let Some(blob) = self
            .vault
            .get_secret(TUI_HISTORY_SECRET_NAME)
            .await
            .context("read input history from vault")?
        else {
            return Ok(Vec::new());
        };
        let entries: Vec<String> =
            serde_json::from_slice(blob.as_bytes()).context("decode input history JSON")?;
        Ok(entries)
    }

    async fn save(&self, history: &[String]) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(history).context("encode input history JSON")?;
        self.vault
            .store_secret(TUI_HISTORY_SECRET_NAME, &bytes)
            .await
            .context("write input history to vault")?;
        Ok(())
    }
}
