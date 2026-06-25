//! Per-instance admission: the allow-list of gateway `instance_key`s that may
//! use the relay + push roles. [`Admission`] is the read seam both roles check;
//! [`InMemoryAdmission`] is the live, hot-swappable backing — the runtime
//! refreshes it via [`replace_all`] on each poll of the source-of-truth table.
//!
//! [`replace_all`]: InMemoryAdmission::replace_all

use std::collections::HashSet;

use parking_lot::RwLock;

/// "Auth" on C = per-instance admission only (machine-to-machine, no device auth
/// and no plaintext). Both roles check membership against the same list.
pub trait Admission: Send + Sync {
    fn is_admitted(&self, instance_key: &str) -> bool;
}

/// A live in-memory admission allow-list. [`replace_all`](Self::replace_all)
/// swaps the whole set atomically; the runtime calls it on each poll of the
/// source table. Reads (`is_admitted`) take a shared lock, so they don't block
/// each other.
#[derive(Default)]
pub struct InMemoryAdmission {
    keys: RwLock<HashSet<String>>,
}

impl InMemoryAdmission {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            keys: RwLock::new(keys.into_iter().map(Into::into).collect()),
        }
    }

    pub fn admit(&self, instance_key: impl Into<String>) {
        self.keys.write().insert(instance_key.into());
    }

    pub fn revoke(&self, instance_key: &str) {
        self.keys.write().remove(instance_key);
    }

    /// Replace the entire allow-list — the poll refresh.
    pub fn replace_all(&self, keys: HashSet<String>) {
        *self.keys.write() = keys;
    }
}

impl Admission for InMemoryAdmission {
    fn is_admitted(&self, instance_key: &str) -> bool {
        self.keys.read().contains(instance_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_allow_list() {
        let a = InMemoryAdmission::with_keys(["inst-A"]);
        assert!(a.is_admitted("inst-A"));
        assert!(!a.is_admitted("inst-B"));
        a.admit("inst-B");
        assert!(a.is_admitted("inst-B"));
        a.revoke("inst-A");
        assert!(!a.is_admitted("inst-A"));
    }

    #[test]
    fn replace_all_swaps_the_whole_set() {
        let a = InMemoryAdmission::with_keys(["old"]);
        a.replace_all(HashSet::from(["new".to_string()]));
        assert!(a.is_admitted("new"));
        assert!(!a.is_admitted("old"));
    }
}
