//! Live, admission-gated relay connections tracked by `remote_api_key`, so an
//! admission hot-reload can **drop** the connections of a revoked key — not just
//! refuse new ones. Admission is checked once, at connect time; without this a
//! gateway whose key is removed from the allow-list would stay connected until
//! it happened to disconnect on its own.
//!
//! Each live connection (the gateway control channel, and the pairing/content
//! host legs) registers a one-shot "kick" channel under its `remote_api_key` and
//! drops a [`ConnGuard`] when it ends. A revoke fires every channel for the
//! removed keys; the connection's loop awaits its receiver and closes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Fallback cap on simultaneous relay connections one admitted `remote_api_key`
/// may hold (control + pairing/content host legs) when the key sets no cap of its
/// own. Bounds a buggy or abusive gateway from exhausting C, while staying
/// generous for a real gateway: one control connection, the chat content leg, and
/// — since blob transfers run **concurrently, one dedicated leg each** (not a
/// single deduped warm leg) — a leg per in-flight blob transfer. This cap is
/// therefore the effective per-device concurrent-transfer bound (minus the chat
/// [`CHAT_CONN_RESERVE`]). Override the fallback with `MAX_CONNS_PER_REMOTE_API_KEY`;
/// override per-key via the `max_conns` column in the admission table.
pub const DEFAULT_MAX_CONNS_PER_KEY: usize = 200;

/// Connection slots withheld from background (blob) legs and kept for the key's
/// interactive legs. A [`register_background`](ConnectionRegistry::register_background)
/// caller may use at most `cap - CHAT_CONN_RESERVE` slots, so a blob-leg flood
/// can't `429` a chat host leg's (re)establishment under a shared `remote_api_key`.
/// The `/control` connection is already cap-exempt; this protects chat *content*
/// legs.
pub const CHAT_CONN_RESERVE: usize = 4;

/// Registry of live connections' kick channels, keyed by `remote_api_key`. Cheap
/// per-connection registration; the only bulk op is [`kick`](Self::kick) on a
/// poll that removed keys. Also caps how many connections one key may hold.
pub struct ConnectionRegistry {
    conns: Mutex<HashMap<String, HashMap<u64, oneshot::Sender<()>>>>,
    next_id: AtomicU64,
    /// Fallback per-key cap for [`register`](Self::register) when a key passes no
    /// override. The control channel ([`register_unchecked`](Self::register_unchecked))
    /// is exempt so a gateway at its leg limit can always (re)establish control.
    max_per_key: usize,
}

impl Default for ConnectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            max_per_key: DEFAULT_MAX_CONNS_PER_KEY,
        }
    }

    /// Override the fallback per-key cap (operator-tunable; the default for keys
    /// with no `max_conns` of their own). Clamped to ≥ 1.
    pub fn with_max_per_key(mut self, max: usize) -> Self {
        self.max_per_key = max.max(1);
        self
    }

    /// Register a live connection for `remote_api_key`, enforcing its connection
    /// cap: `max_override` (the key's per-key cap from admission) if set, else the
    /// registry's fallback default. Returns `None` if the key is already at the
    /// cap — the caller must refuse the new one. Await the returned receiver in
    /// the connection's loop (it resolves on revoke); hold the [`ConnGuard`] for
    /// the connection's lifetime (dropping it deregisters).
    pub(crate) fn register(
        self: &Arc<Self>,
        remote_api_key: &str,
        max_override: Option<usize>,
    ) -> Option<(ConnGuard, oneshot::Receiver<()>)> {
        let cap = max_override.unwrap_or(self.max_per_key);
        self.register_capped(remote_api_key, cap)
    }

    /// Like [`register`](Self::register) but for a **background (blob) leg**: it may
    /// use at most `cap - CHAT_CONN_RESERVE` of the key's budget, leaving a headroom
    /// a chat host leg can always (re)claim. Returns `None` once the key is at the
    /// reduced cap.
    pub(crate) fn register_background(
        self: &Arc<Self>,
        remote_api_key: &str,
        max_override: Option<usize>,
    ) -> Option<(ConnGuard, oneshot::Receiver<()>)> {
        let cap = max_override
            .unwrap_or(self.max_per_key)
            .saturating_sub(CHAT_CONN_RESERVE)
            .max(1);
        self.register_capped(remote_api_key, cap)
    }

    /// Shared body of [`register`](Self::register) /
    /// [`register_background`](Self::register_background): admit one connection for
    /// `remote_api_key` under `cap`, returning `None` if it is already at `cap`.
    fn register_capped(
        self: &Arc<Self>,
        remote_api_key: &str,
        cap: usize,
    ) -> Option<(ConnGuard, oneshot::Receiver<()>)> {
        let mut conns = self.conns.lock();
        let entry = conns.entry(remote_api_key.to_string()).or_default();
        if entry.len() >= cap {
            // `or_default` only inserts an empty map for a brand-new key, which is
            // never at the cap; an at-cap key already existed, so nothing leaks.
            drop(conns);
            tracing::warn!(cap, "relay: remote_api_key at its connection cap; refusing");
            return None;
        }
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        entry.insert(id, tx);
        Some((self.guard(remote_api_key, id), rx))
    }

    /// Register the gateway's control connection — exempt from the cap so a
    /// gateway already at its leg limit can still (re)establish control. Counted
    /// and kickable like any other connection.
    pub(crate) fn register_unchecked(
        self: &Arc<Self>,
        remote_api_key: &str,
    ) -> (ConnGuard, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.conns
            .lock()
            .entry(remote_api_key.to_string())
            .or_default()
            .insert(id, tx);
        (self.guard(remote_api_key, id), rx)
    }

    fn guard(self: &Arc<Self>, remote_api_key: &str, id: u64) -> ConnGuard {
        ConnGuard {
            registry: Arc::clone(self),
            key: remote_api_key.to_string(),
            id,
        }
    }

    /// Drop every live connection whose `remote_api_key` is in `revoked` (called on
    /// an admission hot-reload that removed keys). Dropping each sender resolves
    /// its receiver, whose `select!` arm closes the connection. Returns the
    /// number of connections signalled.
    pub fn kick(&self, revoked: &HashSet<String>) -> usize {
        if revoked.is_empty() {
            return 0;
        }
        let mut kicked = 0;
        {
            let mut conns = self.conns.lock();
            for key in revoked {
                if let Some(map) = conns.remove(key) {
                    // The senders drop here as `map` does, resolving the receivers.
                    kicked += map.len();
                }
            }
        }
        if kicked > 0 {
            tracing::info!(
                connections = kicked,
                "relay: remote_api_key revoked; dropping live connections"
            );
        }
        kicked
    }

    fn deregister(&self, remote_api_key: &str, id: u64) {
        let mut conns = self.conns.lock();
        if let Some(map) = conns.get_mut(remote_api_key) {
            map.remove(&id);
            if map.is_empty() {
                conns.remove(remote_api_key);
            }
        }
    }
}

/// Deregisters its connection from the [`ConnectionRegistry`] on drop, so a
/// connection that ends on its own leaves no stale kick channel behind.
pub(crate) struct ConnGuard {
    registry: Arc<ConnectionRegistry>,
    key: String,
    id: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.registry.deregister(&self.key, self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kick_fires_the_receiver_and_counts_only_revoked() {
        let reg = Arc::new(ConnectionRegistry::new());
        let (_g_a, rx_a) = reg.register("inst-A", None).unwrap();
        let (_g_b, mut rx_b) = reg.register("inst-B", None).unwrap();

        let kicked = reg.kick(&HashSet::from(["inst-A".to_string()]));
        assert_eq!(kicked, 1, "only inst-A is revoked");
        // inst-A's receiver resolved (sender dropped); inst-B's is untouched.
        assert!(rx_a.await.is_err(), "kicked receiver resolves");
        assert!(
            rx_b.try_recv().is_err(),
            "inst-B still pending (not resolved)"
        );
    }

    #[tokio::test]
    async fn dropping_the_guard_deregisters() {
        let reg = Arc::new(ConnectionRegistry::new());
        let (guard, _rx) = reg.register("inst-A", None).unwrap();
        drop(guard);
        // Nothing left to kick: the connection deregistered itself.
        assert_eq!(reg.kick(&HashSet::from(["inst-A".to_string()])), 0);
    }

    #[test]
    fn register_enforces_the_per_key_cap_and_frees_on_drop() {
        let reg = Arc::new(ConnectionRegistry::new().with_max_per_key(2));
        let a = reg.register("k", None).expect("1st is under the cap");
        let _b = reg.register("k", None).expect("2nd is under the cap");
        assert!(
            reg.register("k", None).is_none(),
            "3rd exceeds the cap and is refused"
        );
        assert!(reg.register("other", None).is_some(), "the cap is per-key");
        drop(a);
        assert!(
            reg.register("k", None).is_some(),
            "a freed slot is reusable"
        );
    }

    #[test]
    fn per_key_override_beats_the_fallback_default() {
        // Fallback default is 1, but this key carries its own cap of 3.
        let reg = Arc::new(ConnectionRegistry::new().with_max_per_key(1));
        let _a = reg.register("k", Some(3)).expect("1st under the override");
        let _b = reg.register("k", Some(3)).expect("2nd under the override");
        let _c = reg.register("k", Some(3)).expect("3rd under the override");
        assert!(
            reg.register("k", Some(3)).is_none(),
            "4th exceeds the override"
        );
    }

    #[test]
    fn background_legs_are_capped_below_the_chat_reserve() {
        let reg = Arc::new(ConnectionRegistry::new().with_max_per_key(CHAT_CONN_RESERVE + 2));
        // A background (blob) leg may use only cap - CHAT_CONN_RESERVE = 2 slots.
        let _a = reg
            .register_background("k", None)
            .expect("1st background under the reduced cap");
        let _b = reg
            .register_background("k", None)
            .expect("2nd background under the reduced cap");
        assert!(
            reg.register_background("k", None).is_none(),
            "a 3rd background leg exceeds cap - reserve"
        );
        // ...but a chat (full) register can still claim the reserved slots.
        assert!(
            reg.register("k", None).is_some(),
            "chat still claims a slot the reserve kept for it"
        );
    }

    #[tokio::test]
    async fn control_connection_is_exempt_from_the_cap() {
        let reg = Arc::new(ConnectionRegistry::new().with_max_per_key(1));
        let _leg = reg.register("k", None).expect("under the cap");
        assert!(
            reg.register("k", None).is_none(),
            "a second leg exceeds the cap"
        );
        // The control channel registers even at the cap, and is still kickable.
        let _ctrl = reg.register_unchecked("k");
        assert_eq!(
            reg.kick(&HashSet::from(["k".to_string()])),
            2,
            "both the leg and the control connection drop on revoke"
        );
    }
}
