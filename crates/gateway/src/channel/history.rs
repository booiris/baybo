//! Server-side persistent input-history store for the built-in TUI.
//!
//! The TUI deliberately does not open the workspace vault itself — it
//! only holds a WS connection — so the persistent ring lives here, on
//! the single process that already owns [`SecretVault`] and the sqlite
//! file. Clients request their snapshot via the [`Frame::HistorySnapshot`](baybo_channels::wire::Frame::HistorySnapshot)
//! frame the route emits right after a successful TUI register, and
//! push new entries with [`Frame::HistoryAppend`](baybo_channels::wire::Frame::HistoryAppend).
//!
//! Multiple concurrent TUI processes on the same gateway share this
//! store, so the read-modify-write of each append is serialized by an
//! internal [`tokio::sync::Mutex`] — a single-process lock is enough
//! because no other process ever writes this vault key.

use std::sync::Arc;

use anyhow::Context as _;
use baybo_security::SecretVault;
use tokio::sync::Mutex;

/// Vault key under which the TUI input history lives. Matches the
/// historical name so an upgrade preserves any pre-existing ring.
const TUI_HISTORY_SECRET_NAME: &str = "baybo.tui.input_history";

/// Maximum entries retained. Matches the in-memory ring used by the
/// TUI so a full reload doesn't drop rows the client just saw.
const TUI_HISTORY_CAP: usize = 500;

/// Vault-backed, in-gateway TUI history store. Cheap to clone — state
/// lives behind `Arc`s.
#[derive(Clone)]
pub struct TuiHistoryStore {
    vault: Arc<SecretVault>,
    /// Serializes the read-modify-write of [`Self::append`]. Only one
    /// process writes this key, so an in-process mutex is all that's
    /// needed for correctness against concurrent TUI clients.
    write_lock: Arc<Mutex<()>>,
}

impl TuiHistoryStore {
    pub fn new(vault: Arc<SecretVault>) -> Self {
        Self {
            vault,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Return the persisted ring in chronological order. `Ok(vec![])`
    /// when nothing has been saved yet.
    pub async fn load(&self) -> anyhow::Result<Vec<String>> {
        self.read_entries().await
    }

    /// Append one submitted line. Consecutive duplicates are collapsed
    /// so mashing Enter on the same prompt doesn't flood the ring, and
    /// the ring is capped at [`TUI_HISTORY_CAP`] so the encrypted blob
    /// stays bounded.
    pub async fn append(&self, entry: &str) -> anyhow::Result<()> {
        let _guard = self.write_lock.lock().await;
        let mut entries = self.read_entries().await.unwrap_or_default();
        if entries.last().map(String::as_str) != Some(entry) {
            entries.push(entry.to_owned());
        }
        if entries.len() > TUI_HISTORY_CAP {
            let skip = entries.len() - TUI_HISTORY_CAP;
            entries.drain(..skip);
        }
        self.write_entries(&entries).await
    }

    async fn read_entries(&self) -> anyhow::Result<Vec<String>> {
        let Some(blob) = self
            .vault
            .get_secret(TUI_HISTORY_SECRET_NAME)
            .await
            .context("read tui input history from vault")?
        else {
            return Ok(Vec::new());
        };
        serde_json::from_slice(blob.as_bytes()).context("decode tui input history JSON")
    }

    async fn write_entries(&self, entries: &[String]) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(entries).context("encode tui input history JSON")?;
        self.vault
            .store_secret(TUI_HISTORY_SECRET_NAME, &bytes)
            .await
            .context("write tui input history to vault")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_security::EncryptionKey;
    use baybo_security::test_support::MemorySecretStore;

    fn build_store() -> TuiHistoryStore {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("build test encryption key");
        let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
        TuiHistoryStore::new(vault)
    }

    #[tokio::test]
    async fn append_persists_entry() {
        let store = build_store();
        store.append("one").await.unwrap();
        store.append("two").await.unwrap();
        assert_eq!(store.load().await.unwrap(), vec!["one", "two"]);
    }

    #[tokio::test]
    async fn append_dedupes_consecutive_duplicates() {
        let store = build_store();
        store.append("same").await.unwrap();
        store.append("same").await.unwrap();
        store.append("other").await.unwrap();
        store.append("same").await.unwrap();
        assert_eq!(store.load().await.unwrap(), vec!["same", "other", "same"]);
    }

    #[tokio::test]
    async fn concurrent_appends_all_survive() {
        // Two "virtual TUIs" appending at the same time. The in-process
        // mutex must keep each entry even though RMW happens
        // concurrently — otherwise we reproduce the old
        // last-writer-wins race.
        let store = build_store();
        let store = Arc::new(store);

        let store_b = Arc::clone(&store);
        let b = tokio::spawn(async move {
            for i in 0..20 {
                store_b.append(&format!("b-{i}")).await.unwrap();
            }
        });
        for i in 0..20 {
            store.append(&format!("a-{i}")).await.unwrap();
        }
        b.await.unwrap();

        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.len(), 40);
        for i in 0..20 {
            assert!(loaded.contains(&format!("a-{i}")));
            assert!(loaded.contains(&format!("b-{i}")));
        }
    }

    #[tokio::test]
    async fn append_caps_ring_to_history_limit() {
        let store = build_store();
        for i in 0..(TUI_HISTORY_CAP + 25) {
            store.append(&format!("entry-{i}")).await.unwrap();
        }
        let loaded = store.load().await.unwrap();
        assert_eq!(loaded.len(), TUI_HISTORY_CAP);
        assert_eq!(loaded.first().unwrap(), "entry-25");
        assert_eq!(
            loaded.last().unwrap(),
            &format!("entry-{}", TUI_HISTORY_CAP + 24)
        );
    }
}
