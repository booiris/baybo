//! The C-side of the A↔C control connection.
//!
//! A NAT'd gateway (A) can't be dialed by C, so each A instead holds a
//! **persistent outbound control connection** to C. When a phone arrives at the
//! relay for A's `relay_node_id`, C signals A over that control connection to
//! open a **data leg**, which then joins the blind byte-pipe
//! [`super::RelayBroker`] under a shared key and meets the phone's leg.
//!
//! This is the C-side registry + signaling core (keyed by `relay_node_id`); the
//! production WebSocket transport — A dialing out, C accepting and pumping the
//! signal stream — layers on top. Host-testable over `mpsc`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use remote_host_protocol::key_tag;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

/// Bounded backlog of control signals toward one gateway.
const CONTROL_CHANNEL_CAP: usize = 32;

/// The control-plane wire types ([`ControlHello`] in, [`ControlSignal`] out)
/// live in the shared protocol crate, so the gateway encodes/decodes the exact
/// same definitions.
pub use remote_host_protocol::relay::{ControlHello, ControlSignal, LegClass};

/// A gateway's live control connection: the signal channel plus its admitted
/// `remote_api_key`, so the relay can attribute the anonymous phone-side content
/// leg (which names only the `relay_node_id`) back to the owning gateway.
struct ControlEntry {
    tx: mpsc::Sender<ControlSignal>,
    remote_api_key: String,
    /// Unique per accepted control connection. A stale connection's cleanup
    /// removes its slot only when this still matches, so it can't evict the
    /// entry a faster reconnect already installed under the same
    /// `relay_node_id` + `remote_api_key`.
    token: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRegisterError {
    /// The `relay_node_id` is already registered to a different key; `owner` is
    /// the registered entry's `remote_api_key` (log it only via [`key_tag`]).
    OwnerMismatch { owner: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOpenError {
    NotConnected,
    OwnerMismatch,
    Backpressure,
}

/// Registry of gateways' live control connections, keyed by `relay_node_id`.
#[derive(Default)]
pub struct ControlRegistry {
    instances: Mutex<HashMap<String, ControlEntry>>,
    next_token: AtomicU64,
}

impl ControlRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A gateway registers its control connection under `relay_node_id` (with its
    /// admitted `remote_api_key`) and gets the receiver its control loop acts on
    /// plus a connection token to pass to [`Self::unregister_if_owned`]. A
    /// re-register supersedes a stale connection (reconnect wins).
    pub fn register(
        &self,
        relay_node_id: &str,
        remote_api_key: &str,
    ) -> Result<(mpsc::Receiver<ControlSignal>, u64), ControlRegisterError> {
        let (tx, rx) = mpsc::channel(CONTROL_CHANNEL_CAP);
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let mut instances = self.instances.lock();
        if let Some(existing) = instances.get(relay_node_id) {
            if existing.remote_api_key != remote_api_key {
                return Err(ControlRegisterError::OwnerMismatch {
                    owner: existing.remote_api_key.clone(),
                });
            }
            tracing::debug!(
                relay_node_id = %relay_node_id,
                key_tag = %key_tag(remote_api_key),
                "control: new registration superseded a live control connection for this relay_node_id"
            );
        }
        instances.insert(
            relay_node_id.to_string(),
            ControlEntry {
                tx,
                remote_api_key: remote_api_key.to_string(),
                token,
            },
        );
        Ok((rx, token))
    }

    /// Signal a registered gateway to open a data leg under `relay_key` of the
    /// given `class` (so the gateway runs the chat loop or the blob sub-protocol
    /// and meters accordingly). Returns the gateway's `remote_api_key` on success
    /// (so the caller can meter the resulting leg against it), or `None` if the
    /// gateway isn't connected (the phone's relay attempt then fails fast rather
    /// than hanging) or its control channel is closed.
    pub fn signal_open(
        &self,
        relay_node_id: &str,
        expected_remote_api_key: &str,
        relay_key: &str,
        class: LegClass,
    ) -> Result<String, SignalOpenError> {
        let entry = self
            .instances
            .lock()
            .get(relay_node_id)
            .map(|e| (e.tx.clone(), e.remote_api_key.clone()));
        let Some((tx, remote_api_key)) = entry else {
            return Err(SignalOpenError::NotConnected);
        };
        if remote_api_key != expected_remote_api_key {
            return Err(SignalOpenError::OwnerMismatch);
        }
        let signal = ControlSignal::OpenDataLeg {
            relay_key: relay_key.to_string(),
            class,
        };
        match tx.try_send(signal) {
            Ok(()) => Ok(remote_api_key),
            Err(TrySendError::Closed(_)) => Err(SignalOpenError::NotConnected),
            Err(TrySendError::Full(_)) => Err(SignalOpenError::Backpressure),
        }
    }

    /// Drop a gateway's control connection on disconnect — but only if `token`
    /// still owns the slot. A stale connection's cleanup firing *after* a faster
    /// reconnect already replaced the entry (both share the same `relay_node_id` +
    /// `remote_api_key`, so the key can't tell them apart) must not evict the live
    /// owner, which would leave the gateway connected but unroutable until its next
    /// control redial. The connection token is the identity that distinguishes them.
    pub fn unregister_if_owned(&self, relay_node_id: &str, token: u64) {
        let mut instances = self.instances.lock();
        if instances
            .get(relay_node_id)
            .is_some_and(|entry| entry.token == token)
        {
            instances.remove(relay_node_id);
        }
    }

    /// Number of gateways with a live control connection.
    pub fn connected(&self) -> usize {
        self.instances.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelayBroker;

    #[tokio::test]
    async fn signals_a_registered_gateway_to_open_a_leg() {
        let reg = ControlRegistry::new();
        let (mut rx, _token) = reg.register("node-1", "inst-A").unwrap();
        assert_eq!(reg.connected(), 1);

        // The signal succeeds and reports the gateway's owning remote_api_key.
        assert_eq!(
            reg.signal_open("node-1", "inst-A", "leg-abc", LegClass::Chat),
            Ok("inst-A".to_string())
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            ControlSignal::OpenDataLeg {
                relay_key: "leg-abc".into(),
                class: LegClass::Chat,
            },
        );
    }

    #[test]
    fn open_data_leg_serializes_to_gateway_wire_shape() {
        // Must match the gateway's `ControlServerMsg` JSON byte-for-byte. A Chat
        // leg omits no field the gateway requires; `class` rides as a snake_case
        // string (and a pre-class gateway tolerates it via `#[serde(default)]`).
        let v = serde_json::to_value(ControlSignal::OpenDataLeg {
            relay_key: "k".into(),
            class: LegClass::Blob,
        })
        .unwrap();
        assert_eq!(v["t"], "open_data_leg");
        assert_eq!(v["relay_key"], "k");
        assert_eq!(v["class"], "blob");
    }

    #[tokio::test]
    async fn signal_to_unknown_gateway_fails_fast() {
        let reg = ControlRegistry::new();
        assert!(matches!(
            reg.signal_open("ghost", "inst-A", "k", LegClass::Chat),
            Err(SignalOpenError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn unregister_drops_the_connection() {
        let reg = ControlRegistry::new();
        let (_rx, token) = reg.register("n", "inst-A").unwrap();
        reg.unregister_if_owned("n", token);
        assert_eq!(reg.connected(), 0);
        assert!(matches!(
            reg.signal_open("n", "inst-A", "k", LegClass::Chat),
            Err(SignalOpenError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn stale_unregister_does_not_evict_a_reconnected_owner() {
        // A fast gateway reconnect (same node + same remote_api_key) supersedes the
        // old slot. The old connection's cleanup then fires with its stale token —
        // it must NOT evict the live (reconnected) owner, or the gateway would be
        // connected yet unroutable until its next control redial. This is the exact
        // race the connection token was added to close (mirrors the gateway-side
        // `ChannelControlRegistry::unregister_if_owned`).
        let reg = ControlRegistry::new();
        let (_rx_old, old_token) = reg.register("node-1", "inst-A").unwrap();
        let (_rx_new, _new_token) = reg.register("node-1", "inst-A").unwrap();
        assert_eq!(reg.connected(), 1, "reconnect supersedes, not duplicates");

        // Stale cleanup with the old token is a no-op against the new owner.
        reg.unregister_if_owned("node-1", old_token);
        assert_eq!(reg.connected(), 1, "live owner survives stale cleanup");
        assert!(matches!(
            reg.signal_open("node-1", "inst-A", "k", LegClass::Chat),
            Ok(key) if key == "inst-A"
        ));
    }

    #[test]
    fn different_key_cannot_replace_registered_node() {
        let reg = ControlRegistry::new();
        let (_rx, _token) = reg.register("node-1", "inst-A").unwrap();
        assert!(matches!(
            reg.register("node-1", "inst-B"),
            Err(ControlRegisterError::OwnerMismatch { owner }) if owner == "inst-A"
        ));
        assert_eq!(
            reg.signal_open("node-1", "inst-B", "leg-abc", LegClass::Chat),
            Err(SignalOpenError::OwnerMismatch)
        );
    }

    /// The control plane + the byte-pipe compose: C signals A to open a leg
    /// under a key, both legs join the broker under that key, and bytes flow.
    #[tokio::test]
    async fn control_signal_drives_a_relay_match() {
        let reg = ControlRegistry::new();
        let broker = RelayBroker::new();

        // Gateway A registers its control connection.
        let (mut a_control, _token) = reg.register("node-1", "inst-A").unwrap();

        // A phone arrives at the relay for node-1: it joins the broker and C
        // signals A to open the matching data leg.
        let phone = broker.join("leg-xyz").expect("phone leg parks");
        assert_eq!(
            reg.signal_open("node-1", "inst-A", "leg-xyz", LegClass::Chat),
            Ok("inst-A".to_string())
        );

        // A acts on the signal: opens a data leg under the same key.
        let ControlSignal::OpenDataLeg { relay_key, .. } = a_control.recv().await.unwrap();
        let mut gateway = broker.join(&relay_key).expect("gateway leg matches");

        // The two legs are matched; opaque frames flow blind.
        phone.to_peer.send(b"noise-frame".to_vec()).await.unwrap();
        assert_eq!(gateway.from_peer.recv().await.unwrap(), b"noise-frame");
    }
}
