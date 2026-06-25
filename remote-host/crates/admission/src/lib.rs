//! Per-instance admission: the allow-list of gateway `instance_key`s that may
//! use the relay + push roles, each with optional per-key limits (a connection
//! cap and a relay-bandwidth ceiling). [`Admission`] is the read seam both roles
//! check; [`InMemoryAdmission`] is the live, hot-swappable backing — the runtime
//! refreshes it via [`replace_all`] on each poll of the source-of-truth table.
//!
//! [`replace_all`]: InMemoryAdmission::replace_all

use std::collections::{HashMap, HashSet};

use parking_lot::RwLock;

/// The per-key policy stored for one admitted instance. Each limit is optional —
/// `None` means "fall back to the role's hardcoded default for this limit".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdmissionEntry {
    /// Max simultaneous relay connections this key may hold (`None` → the relay's
    /// `MAX_CONNS_PER_INSTANCE` default).
    pub max_conns: Option<u32>,
    /// Max relay bandwidth for this key in bytes/sec, aggregated across all its
    /// content legs (`None` → the relay's `RELAY_BYTES_PER_SEC` default).
    pub max_bps: Option<u64>,
}

/// "Auth" on C = per-instance admission only (machine-to-machine, no device auth
/// and no plaintext). Both roles check membership against the same list.
pub trait Admission: Send + Sync {
    fn is_admitted(&self, instance_key: &str) -> bool;

    /// The key's per-key connection-cap override, if it set one (else `None` — the
    /// caller falls back to its default). Only the relay role consults it.
    fn max_conns(&self, _instance_key: &str) -> Option<u32> {
        None
    }

    /// The key's per-key relay-bandwidth override in bytes/sec, if it set one
    /// (else `None` — the caller falls back to its default). Only the relay role
    /// consults it.
    fn max_bps(&self, _instance_key: &str) -> Option<u64> {
        None
    }
}

/// A live in-memory allow-list mapping each admitted `instance_key` to its
/// [`AdmissionEntry`] (per-key limits). [`replace_all`](Self::replace_all) swaps
/// the whole map atomically; the runtime calls it on each poll of the source
/// table. Reads take a shared lock, so they don't block each other.
#[derive(Default)]
pub struct InMemoryAdmission {
    keys: RwLock<HashMap<String, AdmissionEntry>>,
}

impl InMemoryAdmission {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit `keys` with no per-key overrides (each falls back to the caller's
    /// defaults). Handy for tests.
    pub fn with_keys<I, S>(keys: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            keys: RwLock::new(
                keys.into_iter()
                    .map(|k| (k.into(), AdmissionEntry::default()))
                    .collect(),
            ),
        }
    }

    pub fn admit(&self, instance_key: impl Into<String>) {
        self.keys
            .write()
            .insert(instance_key.into(), AdmissionEntry::default());
    }

    pub fn revoke(&self, instance_key: &str) {
        self.keys.write().remove(instance_key);
    }

    /// Replace the entire allow-list — the poll refresh. Each entry maps an
    /// admitted key to its [`AdmissionEntry`]. Returns the keys that were admitted
    /// before but are absent now (revoked), so the caller can drop their live
    /// connections.
    pub fn replace_all(&self, keys: HashMap<String, AdmissionEntry>) -> HashSet<String> {
        let mut guard = self.keys.write();
        let revoked = guard
            .keys()
            .filter(|k| !keys.contains_key(*k))
            .cloned()
            .collect();
        *guard = keys;
        revoked
    }
}

impl Admission for InMemoryAdmission {
    fn is_admitted(&self, instance_key: &str) -> bool {
        self.keys.read().contains_key(instance_key)
    }

    fn max_conns(&self, instance_key: &str) -> Option<u32> {
        self.keys.read().get(instance_key).and_then(|e| e.max_conns)
    }

    fn max_bps(&self, instance_key: &str) -> Option<u64> {
        self.keys.read().get(instance_key).and_then(|e| e.max_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyset<const N: usize>(
        keys: [(&str, AdmissionEntry); N],
    ) -> HashMap<String, AdmissionEntry> {
        keys.into_iter().map(|(k, e)| (k.to_string(), e)).collect()
    }

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
    fn replace_all_swaps_the_whole_set_and_reports_revoked() {
        let a = InMemoryAdmission::with_keys(["old", "kept"]);
        let revoked = a.replace_all(keyset([
            ("kept", AdmissionEntry::default()),
            ("new", AdmissionEntry::default()),
        ]));
        assert!(a.is_admitted("new"));
        assert!(a.is_admitted("kept"));
        assert!(!a.is_admitted("old"));
        // Only `old` lost admission; `kept` stayed and `new` was added.
        assert_eq!(revoked, HashSet::from(["old".to_string()]));
    }

    #[test]
    fn per_key_overrides_are_exposed() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            (
                "capped",
                AdmissionEntry {
                    max_conns: Some(8),
                    max_bps: Some(2_000_000),
                },
            ),
            ("default", AdmissionEntry::default()),
        ]));
        assert_eq!(a.max_conns("capped"), Some(8));
        assert_eq!(a.max_bps("capped"), Some(2_000_000));
        assert_eq!(
            a.max_conns("default"),
            None,
            "no override -> caller's default"
        );
        assert_eq!(
            a.max_bps("default"),
            None,
            "no override -> caller's default"
        );
        assert_eq!(a.max_conns("absent"), None, "not admitted -> no override");
        assert_eq!(a.max_bps("absent"), None, "not admitted -> no override");
    }
}
