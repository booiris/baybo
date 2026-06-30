//! Per-key admission: the allow-list of `remote_api_key`s that may use the relay,
//! each with per-key limits (a connection cap, a relay-bandwidth ceiling, a
//! per-`(key, server)` bandwidth sub-cap) and an optional expiry.
//!
//! [`Admission`] is the relay read seam via [`Admission::resolve`].
//! [`InMemoryAdmission`] is the live, hot-swappable backing — the runtime
//! refreshes it via [`InMemoryAdmission::replace_all`] on each poll of the
//! source-of-truth table.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use parking_lot::RwLock;

/// SQLite `datetime('now')` wall-clock shape (UTC, fixed-width). `expires_at` is
/// stored and compared in exactly this format, so a lexicographic compare here
/// matches SQLite's own ordering in the DB-side `load()` expiry filter.
const SQLITE_DATETIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// The per-key policy stored for one admitted `remote_api_key`. Each limit is
/// optional — a NULL column stays `None` here, so the caller floors it with its
/// own conservative global role default (`MAX_CONNS_PER_REMOTE_API_KEY` /
/// `RELAY_BYTES_PER_SEC`). The admission write path requires `max_conns` +
/// `max_bps` on every row, so a surviving NULL only ever comes from a legacy DB.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdmissionEntry {
    /// Max simultaneous relay connections this key may hold across all its legs.
    pub max_conns: Option<u32>,
    /// Max relay bandwidth in bytes/sec, aggregated across all the key's legs.
    pub max_bps: Option<u64>,
    /// Per-`(key, server)` bandwidth sub-cap in bytes/sec — the "防自家互饿"
    /// anti-starvation bound so one gateway can't eat the whole key's `max_bps`.
    pub per_server_max_bps: Option<u64>,
    /// Wall-clock expiry (SQLite `datetime` text, UTC); `None` → never expires.
    pub expires_at: Option<String>,
}

impl AdmissionEntry {
    fn is_expired(&self) -> bool {
        match &self.expires_at {
            Some(at) => *at < Utc::now().format(SQLITE_DATETIME_FMT).to_string(),
            None => false,
        }
    }
}

/// The outcome of resolving a `remote_api_key` against the allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Admitted — carries the entry; a NULL limit stays `None` for the caller's
    /// global role floor.
    Ok(AdmissionEntry),
    /// Not on the allow-list.
    Unknown,
    /// On the list but past its `expires_at`.
    Expired,
}

/// "Auth" on C's relay = per-key admission only (machine-to-machine, no device
/// auth and no plaintext). Relay routes resolve the `remote_api_key` header
/// against this list — one source of truth for admit-or-reject + limit/expiry.
/// Push routes are keyless at this layer and authenticate via the device→gateway
/// delegation chain.
pub trait Admission: Send + Sync {
    /// Resolve a `remote_api_key`: admit-or-reject + limit/expiry in one shot. A
    /// NULL limit on the returned [`AdmissionEntry`] stays `None` for the caller's
    /// global role floor.
    fn resolve(&self, remote_api_key: &str) -> Admit;
}

/// A live in-memory allow-list mapping each admitted `remote_api_key` to its
/// [`AdmissionEntry`]. [`replace_all`](Self::replace_all) swaps the whole map
/// atomically; the runtime calls it on each poll of the source table. Reads take a
/// shared lock, so they don't block each other.
#[derive(Default)]
pub struct InMemoryAdmission {
    keys: RwLock<HashMap<String, AdmissionEntry>>,
}

impl InMemoryAdmission {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit `keys` with no per-key overrides (each falls back to the caller's role
    /// floor). Handy for tests.
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

    pub fn admit(&self, remote_api_key: impl Into<String>) {
        self.keys
            .write()
            .insert(remote_api_key.into(), AdmissionEntry::default());
    }

    pub fn revoke(&self, remote_api_key: &str) {
        self.keys.write().remove(remote_api_key);
    }

    /// Replace the entire allow-list — the poll refresh. Returns the keys that were
    /// admitted before but are absent now (revoked), so the caller can drop their
    /// live connections.
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

    /// Sum every admitted key's **effective** `max_conns`, flooring a NULL to
    /// `fallback` (the caller's role connection-cap default). This is the live
    /// ceiling on simultaneous relay connections across all keys — used to size a
    /// capacity-derived cap (the traffic ledger's per-`(key, server)` entry cap) to
    /// the current admission set so it tracks hot-reloaded edits. O(n) over the
    /// allow-list — call off the hot path (e.g. once per flush interval), never per
    /// request.
    pub fn total_max_conns(&self, fallback: u32) -> u64 {
        self.keys
            .read()
            .values()
            .map(|e| u64::from(e.max_conns.unwrap_or(fallback)))
            .sum()
    }

    /// Number of admitted keys currently in the in-memory allow-list. Feeds the
    /// dashboard overview's `keys_admitted` card.
    pub fn len(&self) -> usize {
        self.keys.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Admission for InMemoryAdmission {
    fn resolve(&self, remote_api_key: &str) -> Admit {
        let keys = self.keys.read();
        match keys.get(remote_api_key) {
            None => Admit::Unknown,
            Some(entry) if entry.is_expired() => Admit::Expired,
            Some(entry) => Admit::Ok(entry.clone()),
        }
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
    fn len_and_is_empty_track_the_admitted_set() {
        let a = InMemoryAdmission::new();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
        a.replace_all(keyset([
            ("k1", AdmissionEntry::default()),
            ("k2", AdmissionEntry::default()),
        ]));
        assert!(!a.is_empty());
        assert_eq!(a.len(), 2);
        a.revoke("k1");
        assert_eq!(a.len(), 1);
    }

    #[test]
    fn resolve_unknown_known_and_revoked() {
        let a = InMemoryAdmission::with_keys(["k-A"]);
        assert!(matches!(a.resolve("k-A"), Admit::Ok(_)));
        assert_eq!(a.resolve("k-B"), Admit::Unknown);
        a.admit("k-B");
        assert!(matches!(a.resolve("k-B"), Admit::Ok(_)));
        a.revoke("k-A");
        assert_eq!(a.resolve("k-A"), Admit::Unknown);
    }

    #[test]
    fn null_limits_stay_none_for_the_caller_floor() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([("r", AdmissionEntry::default())]));
        let Admit::Ok(e) = a.resolve("r") else {
            panic!()
        };
        assert_eq!(e.max_conns, None, "NULL -> caller's role floor");
        assert_eq!(e.max_bps, None);
        assert_eq!(e.per_server_max_bps, None);
    }

    #[test]
    fn explicit_limits_round_trip() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            "r",
            AdmissionEntry {
                max_conns: Some(8),
                max_bps: Some(4_194_304),
                per_server_max_bps: Some(1_000_000),
                ..Default::default()
            },
        )]));
        let Admit::Ok(e) = a.resolve("r") else {
            panic!()
        };
        assert_eq!(e.max_conns, Some(8));
        assert_eq!(e.max_bps, Some(4_194_304));
        assert_eq!(e.per_server_max_bps, Some(1_000_000));
    }

    #[test]
    fn expired_key_resolves_to_expired() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            "old",
            AdmissionEntry {
                expires_at: Some("2000-01-01 00:00:00".into()),
                ..Default::default()
            },
        )]));
        assert_eq!(a.resolve("old"), Admit::Expired);
    }

    #[test]
    fn far_future_expiry_still_admits() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            "fresh",
            AdmissionEntry {
                expires_at: Some("2999-01-01 00:00:00".into()),
                ..Default::default()
            },
        )]));
        assert!(matches!(a.resolve("fresh"), Admit::Ok(_)));
    }

    #[test]
    fn total_max_conns_sums_effective_caps() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            // explicit cap → its own value
            (
                "reg",
                AdmissionEntry {
                    max_conns: Some(8),
                    ..Default::default()
                },
            ),
            // NULL → the caller's role fallback (100 here)
            ("reg-null", AdmissionEntry::default()),
        ]));
        assert_eq!(a.total_max_conns(100), 8 + 100);

        let b = InMemoryAdmission::new();
        assert_eq!(b.total_max_conns(100), 0, "no keys → no capacity");
    }

    #[test]
    fn replace_all_swaps_the_whole_set_and_reports_revoked() {
        let a = InMemoryAdmission::with_keys(["old", "kept"]);
        let revoked = a.replace_all(keyset([
            ("kept", AdmissionEntry::default()),
            ("new", AdmissionEntry::default()),
        ]));
        assert!(matches!(a.resolve("new"), Admit::Ok(_)));
        assert!(matches!(a.resolve("kept"), Admit::Ok(_)));
        assert_eq!(a.resolve("old"), Admit::Unknown);
        assert_eq!(revoked, HashSet::from(["old".to_string()]));
    }
}
