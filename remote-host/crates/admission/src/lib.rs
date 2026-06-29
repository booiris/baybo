//! Per-key admission: the allow-list of `remote_api_key`s that may use the relay
//! and push roles, each with per-key limits (a connection cap, a relay-bandwidth
//! ceiling, a per-`(key, server)` bandwidth sub-cap) and a [`Tier`].
//!
//! [`Admission`] is the single read seam both roles check via
//! [`Admission::resolve`]. [`InMemoryAdmission`] is the live, hot-swappable
//! backing — the runtime refreshes it via [`InMemoryAdmission::replace_all`] on
//! each poll of the source-of-truth table.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use chrono::Utc;
use parking_lot::RwLock;

/// Guest-tier default limits — the **final** fallback for a `tier='guest'` row that
/// leaves the matching column NULL, used only when the [`GUEST_TEMPLATE_KEY`] row is
/// absent or also leaves that column NULL. Registered rows have **no** tier default —
/// a NULL limit there stays `None`, so the caller floors it with its own conservative
/// global role default (`MAX_CONNS_PER_REMOTE_API_KEY` / `RELAY_BYTES_PER_SEC`). That
/// asymmetry is what removes any tier inversion.
pub const GUEST_MAX_CONNS: u32 = 2_000;
pub const GUEST_MAX_BPS: u64 = 20_971_520; // 20 MiB/s
pub const GUEST_PER_SERVER_MAX_BPS: u64 = 2_097_152; // 2 MiB/s

/// The reserved `remote_api_key` whose own row is the **guest-tier template**: a
/// guest row's NULL limit column inherits this row's value for that column (then the
/// `GUEST_*` const). It is an ordinary admitted row — the shared trial key — that
/// doubles as the tier's tunable defaults, so the limits are libsql-configurable
/// (`UPDATE remote_api_keys SET max_conns=… WHERE remote_api_key='guest'`) with no
/// separate config table. The lookup is by `remote_api_key`, independent of the
/// template row's own tier.
pub const GUEST_TEMPLATE_KEY: &str = "guest";

/// SQLite `datetime('now')` wall-clock shape (UTC, fixed-width). `expires_at` is
/// stored and compared in exactly this format, so a lexicographic compare here
/// matches SQLite's own ordering in the DB-side `load()` expired-guest filter.
const SQLITE_DATETIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

/// The admission tier of a `remote_api_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// Auto-issued, ephemeral; carries the guest default limits and (when a TTL is
    /// enabled) an `expires_at`; eligible for the guest GC sweep.
    Guest,
    /// Control-plane-provisioned, persistent, no TTL; limits set explicitly per row.
    #[default]
    Registered,
}

impl FromStr for Tier {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "guest" => Ok(Self::Guest),
            "registered" => Ok(Self::Registered),
            _ => Err(()),
        }
    }
}

/// The per-key policy stored for one admitted `remote_api_key`. Each limit is
/// optional — [`Admission::resolve`] fills a NULL with the guest tier default
/// (guest rows) or leaves it `None` for the caller's global role floor (registered).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdmissionEntry {
    pub tier: Tier,
    /// Max simultaneous relay connections this key may hold across all its legs.
    pub max_conns: Option<u32>,
    /// Max relay bandwidth in bytes/sec, aggregated across all the key's legs.
    pub max_bps: Option<u64>,
    /// Per-`(key, server)` bandwidth sub-cap in bytes/sec — the "防自家互饿"
    /// anti-starvation bound so one gateway can't eat the whole key's `max_bps`.
    pub per_server_max_bps: Option<u64>,
    /// Wall-clock expiry (SQLite `datetime` text, UTC); `None` → never expires.
    /// Only guest rows carry one, and only when a TTL is enabled.
    pub expires_at: Option<String>,
}

impl AdmissionEntry {
    fn is_expired(&self) -> bool {
        match &self.expires_at {
            Some(at) => *at < Utc::now().format(SQLITE_DATETIME_FMT).to_string(),
            None => false,
        }
    }

    /// Fill this guest row's NULL limits from the [`GUEST_TEMPLATE_KEY`] row's
    /// columns (`template`), falling through per-column to the `GUEST_*` const when
    /// the template is absent or also NULL there. Caller guarantees
    /// `self.tier == Tier::Guest`. Idempotent (an explicit per-row limit is kept).
    fn with_guest_defaults(mut self, template: Option<&AdmissionEntry>) -> Self {
        self.max_conns.get_or_insert(
            template
                .and_then(|t| t.max_conns)
                .unwrap_or(GUEST_MAX_CONNS),
        );
        self.max_bps
            .get_or_insert(template.and_then(|t| t.max_bps).unwrap_or(GUEST_MAX_BPS));
        self.per_server_max_bps.get_or_insert(
            template
                .and_then(|t| t.per_server_max_bps)
                .unwrap_or(GUEST_PER_SERVER_MAX_BPS),
        );
        self
    }
}

/// The outcome of resolving a `remote_api_key` against the allow-list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// Admitted — carries the entry with guest-tier defaults already applied.
    Ok(AdmissionEntry),
    /// Not on the allow-list.
    Unknown,
    /// On the list but past its `expires_at`.
    Expired,
}

/// "Auth" on C = per-key admission only (machine-to-machine, no device auth and no
/// plaintext). Both roles resolve a `remote_api_key` against the same list — one
/// source of truth for admit-or-reject + limit/expiry. Extraction (relay header vs
/// push body) and the limiter applied stay role-specific; both just key on the
/// resolved `remote_api_key`.
pub trait Admission: Send + Sync {
    /// Resolve a `remote_api_key`: admit-or-reject + limit/expiry in one shot. The
    /// returned [`AdmissionEntry`] has guest-tier defaults applied; registered NULL
    /// limits stay `None` for the caller's global role floor.
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

    /// Admit `keys` as registered with no per-key overrides (each falls back to the
    /// caller's role floor). Handy for tests.
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

    /// Sum every admitted key's **effective** `max_conns`: a guest NULL is filled
    /// from the [`GUEST_TEMPLATE_KEY`] row's column then [`GUEST_MAX_CONNS`]; a
    /// registered NULL is floored to `registered_fallback` (the caller's role
    /// connection-cap default). This is the live ceiling on simultaneous relay
    /// connections across all keys — used to size a capacity-derived cap (the
    /// traffic ledger's per-`(key, server)` entry cap) to the current admission set
    /// so it tracks hot-reloaded edits. O(n) over the allow-list — call off the hot
    /// path (e.g. once per flush interval), never per request.
    pub fn total_max_conns(&self, registered_fallback: u32) -> u64 {
        let keys = self.keys.read();
        let template_max_conns = keys.get(GUEST_TEMPLATE_KEY).and_then(|t| t.max_conns);
        keys.values()
            .map(|e| {
                let effective = match e.tier {
                    Tier::Guest => e
                        .max_conns
                        .or(template_max_conns)
                        .unwrap_or(GUEST_MAX_CONNS),
                    Tier::Registered => e.max_conns.unwrap_or(registered_fallback),
                };
                u64::from(effective)
            })
            .sum()
    }

    /// Remove guest entries whose `expires_at` is in the past; return the removed
    /// keys so the caller can drop their live connections.
    ///
    /// This is an **infra / admission** GC over the `remote_api_keys` allow-list —
    /// it is NOT session data, so the "never delete sessions" rule does not apply.
    pub fn gc_expired_guests(&self) -> HashSet<String> {
        let mut guard = self.keys.write();
        let expired: HashSet<String> = guard
            .iter()
            .filter(|(_, e)| e.tier == Tier::Guest && e.is_expired())
            .map(|(k, _)| k.clone())
            .collect();
        for k in &expired {
            guard.remove(k);
        }
        expired
    }
}

impl Admission for InMemoryAdmission {
    fn resolve(&self, remote_api_key: &str) -> Admit {
        let keys = self.keys.read();
        match keys.get(remote_api_key) {
            None => Admit::Unknown,
            Some(entry) if entry.is_expired() => Admit::Expired,
            Some(entry) if entry.tier == Tier::Guest => {
                // A guest row's NULL limits inherit the `guest` template row's columns
                // (then the GUEST_* consts). The template is just another row in this
                // same map — read under the one shared lock, no extra synchronization.
                let template = keys.get(GUEST_TEMPLATE_KEY);
                Admit::Ok(entry.clone().with_guest_defaults(template))
            }
            Some(entry) => Admit::Ok(entry.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tier: Tier) -> AdmissionEntry {
        AdmissionEntry {
            tier,
            ..Default::default()
        }
    }

    fn keyset<const N: usize>(
        keys: [(&str, AdmissionEntry); N],
    ) -> HashMap<String, AdmissionEntry> {
        keys.into_iter().map(|(k, e)| (k.to_string(), e)).collect()
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
    fn guest_null_limits_fall_back_to_guest_defaults() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([("g", entry(Tier::Guest))]));
        let Admit::Ok(e) = a.resolve("g") else {
            panic!("guest is admitted")
        };
        assert_eq!(e.max_conns, Some(GUEST_MAX_CONNS));
        assert_eq!(e.max_bps, Some(GUEST_MAX_BPS));
        assert_eq!(e.per_server_max_bps, Some(GUEST_PER_SERVER_MAX_BPS));
    }

    #[test]
    fn guest_explicit_limit_wins_over_the_default() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            "g",
            AdmissionEntry {
                tier: Tier::Guest,
                max_conns: Some(5),
                ..Default::default()
            },
        )]));
        let Admit::Ok(e) = a.resolve("g") else {
            panic!()
        };
        assert_eq!(e.max_conns, Some(5), "explicit beats the guest default");
        assert_eq!(
            e.max_bps,
            Some(GUEST_MAX_BPS),
            "the unset one still defaults"
        );
    }

    #[test]
    fn guest_template_row_supplies_defaults_to_other_guests() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            (
                GUEST_TEMPLATE_KEY,
                AdmissionEntry {
                    tier: Tier::Guest,
                    max_conns: Some(500),
                    max_bps: Some(10_485_760),
                    per_server_max_bps: Some(1_048_576),
                    ..Default::default()
                },
            ),
            ("g2", entry(Tier::Guest)),
        ]));
        let Admit::Ok(e) = a.resolve("g2") else {
            panic!("guest is admitted")
        };
        assert_eq!(e.max_conns, Some(500), "inherits the `guest` template row");
        assert_eq!(e.max_bps, Some(10_485_760));
        assert_eq!(e.per_server_max_bps, Some(1_048_576));
    }

    #[test]
    fn guest_row_explicit_limit_beats_the_template() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            (
                GUEST_TEMPLATE_KEY,
                AdmissionEntry {
                    tier: Tier::Guest,
                    max_conns: Some(500),
                    ..Default::default()
                },
            ),
            (
                "g2",
                AdmissionEntry {
                    tier: Tier::Guest,
                    max_conns: Some(5),
                    ..Default::default()
                },
            ),
        ]));
        let Admit::Ok(e) = a.resolve("g2") else {
            panic!()
        };
        assert_eq!(e.max_conns, Some(5), "the row's explicit limit wins");
        assert_eq!(
            e.max_bps,
            Some(GUEST_MAX_BPS),
            "a column the template also leaves NULL falls through to the const"
        );
    }

    #[test]
    fn guest_template_null_column_falls_through_to_the_const() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            (
                GUEST_TEMPLATE_KEY,
                AdmissionEntry {
                    tier: Tier::Guest,
                    max_conns: Some(500),
                    // max_bps / per_server_max_bps left NULL on the template.
                    ..Default::default()
                },
            ),
            ("g2", entry(Tier::Guest)),
        ]));
        let Admit::Ok(e) = a.resolve("g2") else {
            panic!()
        };
        assert_eq!(e.max_conns, Some(500), "set template column is inherited");
        assert_eq!(
            e.max_bps,
            Some(GUEST_MAX_BPS),
            "NULL template column -> const fallback"
        );
        assert_eq!(e.per_server_max_bps, Some(GUEST_PER_SERVER_MAX_BPS));
    }

    #[test]
    fn guest_template_row_resolves_itself_with_its_own_limits() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            GUEST_TEMPLATE_KEY,
            AdmissionEntry {
                tier: Tier::Guest,
                max_conns: Some(500),
                ..Default::default()
            },
        )]));
        let Admit::Ok(e) = a.resolve(GUEST_TEMPLATE_KEY) else {
            panic!()
        };
        assert_eq!(e.max_conns, Some(500));
        assert_eq!(e.max_bps, Some(GUEST_MAX_BPS), "its own NULL -> const");
    }

    #[test]
    fn registered_null_limits_stay_none_for_the_caller_floor() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([("r", entry(Tier::Registered))]));
        let Admit::Ok(e) = a.resolve("r") else {
            panic!()
        };
        assert_eq!(e.max_conns, None, "registered NULL -> caller's role floor");
        assert_eq!(e.max_bps, None);
        assert_eq!(e.per_server_max_bps, None);
    }

    #[test]
    fn per_server_bps_round_trips() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            "r",
            AdmissionEntry {
                tier: Tier::Registered,
                per_server_max_bps: Some(1_000_000),
                ..Default::default()
            },
        )]));
        let Admit::Ok(e) = a.resolve("r") else {
            panic!()
        };
        assert_eq!(e.per_server_max_bps, Some(1_000_000));
    }

    #[test]
    fn expired_guest_resolves_to_expired() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([(
            "old",
            AdmissionEntry {
                tier: Tier::Guest,
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
                tier: Tier::Guest,
                expires_at: Some("2999-01-01 00:00:00".into()),
                ..Default::default()
            },
        )]));
        assert!(matches!(a.resolve("fresh"), Admit::Ok(_)));
    }

    #[test]
    fn gc_removes_only_expired_guests() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            (
                "expired",
                AdmissionEntry {
                    tier: Tier::Guest,
                    expires_at: Some("2000-01-01 00:00:00".into()),
                    ..Default::default()
                },
            ),
            (
                "live-guest",
                AdmissionEntry {
                    tier: Tier::Guest,
                    expires_at: Some("2999-01-01 00:00:00".into()),
                    ..Default::default()
                },
            ),
            ("never-guest", entry(Tier::Guest)),
            (
                "expired-registered",
                AdmissionEntry {
                    tier: Tier::Registered,
                    expires_at: Some("2000-01-01 00:00:00".into()),
                    ..Default::default()
                },
            ),
        ]));
        let removed = a.gc_expired_guests();
        assert_eq!(removed, HashSet::from(["expired".to_string()]));
        assert_eq!(a.resolve("expired"), Admit::Unknown);
        assert!(matches!(a.resolve("live-guest"), Admit::Ok(_)));
        assert!(matches!(a.resolve("never-guest"), Admit::Ok(_)));
        // A registered row past expires_at is NOT swept (only guests are), but
        // resolve still rejects it as Expired.
        assert_eq!(a.resolve("expired-registered"), Admit::Expired);
    }

    #[test]
    fn total_max_conns_sums_effective_caps_across_tiers() {
        let a = InMemoryAdmission::new();
        a.replace_all(keyset([
            // registered, explicit cap → its own value
            (
                "reg",
                AdmissionEntry {
                    tier: Tier::Registered,
                    max_conns: Some(8),
                    ..Default::default()
                },
            ),
            // registered, NULL → the caller's role fallback (100 here)
            ("reg-null", entry(Tier::Registered)),
            // guest, NULL, no template → GUEST_MAX_CONNS
            ("g", entry(Tier::Guest)),
        ]));
        // 8 + 100 + GUEST_MAX_CONNS
        assert_eq!(a.total_max_conns(100), 8 + 100 + u64::from(GUEST_MAX_CONNS));

        // A guest template row supplies the guest default; an empty set is 0.
        let b = InMemoryAdmission::new();
        assert_eq!(b.total_max_conns(100), 0, "no keys → no capacity");
        b.replace_all(keyset([
            (
                GUEST_TEMPLATE_KEY,
                AdmissionEntry {
                    tier: Tier::Guest,
                    max_conns: Some(50),
                    ..Default::default()
                },
            ),
            ("g2", entry(Tier::Guest)),
        ]));
        // template(50) + g2 inherits the template(50)
        assert_eq!(b.total_max_conns(100), 100);
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
