//! Live, admission-gated relay connections tracked by instance key, so an
//! admission hot-reload can **drop** the connections of a revoked key — not just
//! refuse new ones. Admission is checked once, at connect time; without this a
//! gateway whose key is removed from the allow-list would stay connected until
//! it happened to disconnect on its own.
//!
//! Each live connection (the gateway control channel, and the pairing/content
//! host legs) registers a one-shot "kick" channel under its instance key and
//! drops a [`ConnGuard`] when it ends. A revoke fires every channel for the
//! removed keys; the connection's loop awaits its receiver and closes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// Registry of live connections' kick channels, keyed by instance key. Cheap
/// per-connection registration; the only bulk op is [`kick`](Self::kick) on a
/// poll that removed keys.
#[derive(Default)]
pub struct ConnectionRegistry {
    conns: Mutex<HashMap<String, HashMap<u64, oneshot::Sender<()>>>>,
    next_id: AtomicU64,
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a live connection for `instance_key`. Await the returned receiver
    /// in the connection's loop — it resolves when the key is revoked. Hold the
    /// [`ConnGuard`] for the connection's lifetime; dropping it deregisters.
    pub(crate) fn register(
        self: &Arc<Self>,
        instance_key: &str,
    ) -> (ConnGuard, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.conns
            .lock()
            .entry(instance_key.to_string())
            .or_default()
            .insert(id, tx);
        let guard = ConnGuard {
            registry: Arc::clone(self),
            key: instance_key.to_string(),
            id,
        };
        (guard, rx)
    }

    /// Drop every live connection whose instance key is in `revoked` (called on
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
                "relay: instance key revoked; dropping live connections"
            );
        }
        kicked
    }

    fn deregister(&self, instance_key: &str, id: u64) {
        let mut conns = self.conns.lock();
        if let Some(map) = conns.get_mut(instance_key) {
            map.remove(&id);
            if map.is_empty() {
                conns.remove(instance_key);
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
        let (_g_a, rx_a) = reg.register("inst-A");
        let (_g_b, mut rx_b) = reg.register("inst-B");

        let kicked = reg.kick(&HashSet::from(["inst-A".to_string()]));
        assert_eq!(kicked, 1, "only inst-A is revoked");
        // inst-A's receiver resolved (sender dropped); inst-B's is untouched.
        assert!(rx_a.await.is_err(), "kicked receiver resolves");
        assert!(rx_b.try_recv().is_err(), "inst-B still pending (not resolved)");
    }

    #[tokio::test]
    async fn dropping_the_guard_deregisters() {
        let reg = Arc::new(ConnectionRegistry::new());
        let (guard, _rx) = reg.register("inst-A");
        drop(guard);
        // Nothing left to kick: the connection deregistered itself.
        assert_eq!(reg.kick(&HashSet::from(["inst-A".to_string()])), 0);
    }
}
