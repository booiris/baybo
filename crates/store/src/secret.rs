use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Stable identity of the secrets a store addresses — "which credential set
/// is this really", independent of how many handles happen to point at it.
///
/// Subsystems that keep per-credential coordination state key it on this
/// rather than on the handle: two `SecretVault`s over one database are two
/// views of ONE credential, and treating them as distinct is what lets two
/// coordinators race the same OAuth refresh token.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoreIdentity {
    /// Backed by a file on disk. Equal paths address the same secrets in
    /// this process AND in any other, so this is also the anchor for
    /// cross-process locks.
    File(PathBuf),
    /// Process-local with no on-disk identity (in-memory test stores).
    /// Unique per store, so tests never share coordination state.
    Ephemeral(u64),
}

impl StoreIdentity {
    /// Mint a fresh process-unique [`StoreIdentity::Ephemeral`].
    pub fn ephemeral() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self::Ephemeral(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Filesystem anchor for cross-process coordination, when there is one.
    /// `Ephemeral` stores return `None` — nothing outside this process can
    /// address them, so there is nothing to coordinate with.
    pub fn file_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::File(p) => Some(p),
            Self::Ephemeral(_) => None,
        }
    }
}

/// Async trait for encrypted secret persistence.
///
/// Implemented by `baybo_storage::sqlite::SqliteSecretStore` (production)
/// and `baybo_security::test_support::MemorySecretStore` (tests). The bytes
/// handed in are already AES-256-GCM ciphertext minted by
/// `baybo_security::SecretVault` — this layer only persists opaque blobs
/// keyed by name and never sees plaintext.
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn list(&self) -> Result<Vec<String>>;
    /// Hard-delete the secret. Later `store` calls with the same name
    /// re-create it. Idempotent on missing names.
    async fn delete(&self, name: &str) -> Result<()>;
    /// Which credential set this store addresses. MUST be stable for the
    /// life of the store and equal across every handle on the same backing
    /// data — callers use it to decide whether two stores need to share
    /// coordination state.
    fn identity(&self) -> StoreIdentity;
}
