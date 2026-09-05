//! The process-global [`SecureStore`] the shell installs at construction, and
//! the one place an account name becomes a storage key.
//!
//! iOS does not come through here — it talks to the Security framework from
//! Rust, because its keychain item identity is frozen by the continuity
//! contract. Every other target routes `keychain.rs`'s six primitives to the
//! store installed here.

use std::sync::Arc;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::api::{SecureStore, SecureStoreError};

/// What a caller sees when the shell never installed a store. Reachable only by
/// a misconfigured embedder: [`crate::BayboClient::new`] refuses to construct
/// without one on the targets that need it.
pub(crate) const NOT_INSTALLED_MSG: &str = "secure store not installed";

static STORE: OnceLock<Arc<dyn SecureStore>> = OnceLock::new();

/// First install wins. The client is a launch-time singleton, so a second call
/// means two clients in one process — the first one's store keeps serving, and
/// silently swapping it under a live pairing would be worse than ignoring it.
pub(crate) fn install(store: Arc<dyn SecureStore>) {
    let _ = STORE.set(store);
}

#[cfg(test)]
pub(crate) fn installed() -> bool {
    STORE.get().is_some()
}

pub(crate) fn get() -> Result<&'static Arc<dyn SecureStore>, String> {
    STORE.get().ok_or_else(|| NOT_INSTALLED_MSG.to_string())
}

/// Account name → the key the shell stores bytes under.
///
/// A fixed-length lowercase hex SHA-256, and the derivation lives here rather
/// than in each shell for two reasons. It is one home for the rule, so the two
/// implementations cannot disagree about what a safe key looks like; and it is
/// total — no account name, present or future, can produce a key that means
/// something to a filesystem. The push key's account embeds a device id which
/// is locally derived today (`device-` + 64 hex chars, see
/// `device_proto::delegation::device_id_for`) and therefore already safe, but
/// that is a property of today's id, not of the seam.
///
/// **Frozen once an install ships**: the output is the on-disk name, so a
/// different hash or encoding orphans every stored item.
pub(crate) fn storage_key(account: &str) -> String {
    hex::encode(Sha256::digest(account.as_bytes()))
}

/// Fold the foreign error into the module's internal `String` channel. Kept
/// separate so the absence-vs-failure boundary has exactly one crossing point.
pub(crate) fn to_msg(error: SecureStoreError) -> String {
    match error {
        SecureStoreError::Failed { reason } => reason,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;

    use parking_lot::Mutex;

    use super::*;

    /// An in-memory store for host tests, with a switch for the failure path —
    /// the one that must NOT be reported as absence.
    #[derive(Default)]
    pub(crate) struct MemoryStore {
        items: Mutex<HashMap<String, Vec<u8>>>,
        failing: Mutex<bool>,
    }

    impl MemoryStore {
        pub(crate) fn set_failing(&self, failing: bool) {
            *self.failing.lock() = failing;
        }

        fn guard(&self) -> Result<(), SecureStoreError> {
            if *self.failing.lock() {
                return Err(SecureStoreError::Failed {
                    reason: "injected store failure".into(),
                });
            }
            Ok(())
        }
    }

    impl SecureStore for MemoryStore {
        fn get(&self, key: String) -> Result<Option<Vec<u8>>, SecureStoreError> {
            self.guard()?;
            Ok(self.items.lock().get(&key).cloned())
        }

        fn put(&self, key: String, bytes: Vec<u8>) -> Result<(), SecureStoreError> {
            self.guard()?;
            self.items.lock().insert(key, bytes);
            Ok(())
        }

        fn delete(&self, key: String) -> Result<(), SecureStoreError> {
            self.guard()?;
            self.items.lock().remove(&key);
            Ok(())
        }
    }

    /// Install a shared memory store once for the whole test binary and hand it
    /// back, so a test can drive its failure switch. `install` is first-wins, so
    /// every test in the process sees the same store — tests that assert on
    /// stored bytes use distinct account names rather than a fresh store.
    pub(crate) fn shared_memory_store() -> Arc<MemoryStore> {
        static SHARED: OnceLock<Arc<MemoryStore>> = OnceLock::new();
        let store = SHARED
            .get_or_init(|| Arc::new(MemoryStore::default()))
            .clone();
        install(store.clone());
        store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_keys_are_fixed_length_hex_and_account_specific() {
        let a = storage_key("baybo.paired-gateway");
        let b = storage_key("baybo.device-identity");
        assert_eq!(a.len(), 64);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(a, b);
        assert_eq!(a, storage_key("baybo.paired-gateway"), "must be stable");
    }

    /// The push-key account carries a device id; whatever it contains, the key
    /// the shell writes is a bare hex string.
    #[test]
    fn a_device_id_cannot_shape_the_storage_key() {
        let hostile = storage_key("baybo.push-key.../../../etc/passwd");
        assert_eq!(hostile.len(), 64);
        assert!(hostile.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn an_uninstalled_store_is_an_error_not_an_absence() {
        // `install` is first-wins and other tests in this binary install the
        // shared memory store, so assert on the message rather than the state.
        if !installed() {
            match get() {
                Ok(_) => panic!("no store is installed, yet get() returned one"),
                Err(message) => assert_eq!(message, NOT_INSTALLED_MSG),
            }
        }
    }
}
