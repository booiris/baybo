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

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Bounded backlog of control signals toward one gateway.
const CONTROL_CHANNEL_CAP: usize = 32;

/// First frame the gateway (A) sends on the control WS: its `relay_node_id`
/// (routing key) + its admission `instance_key`. Mirrors the gateway-side
/// `ControlClientHello`; decoded from the binary JSON A sends.
#[derive(Debug, Clone, Deserialize)]
pub struct ControlHello {
    pub relay_node_id: String,
    pub instance_key: String,
}

/// A control-plane signal C sends to a registered gateway. Serialized to the
/// exact JSON the gateway's `ControlServerMsg` decodes (`{"t":"open_data_leg",
/// "relay_key":"…"}`), so the two halves agree byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlSignal {
    /// Open a data leg under `relay_key` and join the relay — a phone is
    /// waiting there to reach you.
    OpenDataLeg { relay_key: String },
}

/// Registry of gateways' live control connections, keyed by `relay_node_id`.
#[derive(Default)]
pub struct ControlRegistry {
    instances: Mutex<HashMap<String, mpsc::Sender<ControlSignal>>>,
}

impl ControlRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A gateway registers its control connection under `relay_node_id` and
    /// gets the receiver its control loop acts on. A re-register supersedes a
    /// stale connection (reconnect wins).
    pub fn register(&self, relay_node_id: &str) -> mpsc::Receiver<ControlSignal> {
        let (tx, rx) = mpsc::channel(CONTROL_CHANNEL_CAP);
        self.instances.lock().insert(relay_node_id.to_string(), tx);
        rx
    }

    /// Signal a registered gateway to open a data leg under `relay_key`.
    /// Returns `false` if the gateway isn't connected (the phone's relay
    /// attempt then fails fast rather than hanging) or its control channel is
    /// closed.
    pub async fn signal_open(&self, relay_node_id: &str, relay_key: &str) -> bool {
        // Clone the sender out of the lock so the await never holds it.
        let tx = self.instances.lock().get(relay_node_id).cloned();
        match tx {
            Some(tx) => tx
                .send(ControlSignal::OpenDataLeg {
                    relay_key: relay_key.to_string(),
                })
                .await
                .is_ok(),
            None => false,
        }
    }

    /// Drop a gateway's control connection (on disconnect).
    pub fn unregister(&self, relay_node_id: &str) {
        self.instances.lock().remove(relay_node_id);
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
        let mut rx = reg.register("node-1");
        assert_eq!(reg.connected(), 1);

        assert!(reg.signal_open("node-1", "leg-abc").await);
        assert_eq!(
            rx.recv().await.unwrap(),
            ControlSignal::OpenDataLeg {
                relay_key: "leg-abc".into(),
            },
        );
    }

    #[test]
    fn open_data_leg_serializes_to_gateway_wire_shape() {
        // Must match the gateway's `ControlServerMsg` JSON byte-for-byte.
        let v = serde_json::to_value(ControlSignal::OpenDataLeg {
            relay_key: "k".into(),
        })
        .unwrap();
        assert_eq!(v["t"], "open_data_leg");
        assert_eq!(v["relay_key"], "k");
    }

    #[tokio::test]
    async fn signal_to_unknown_gateway_fails_fast() {
        let reg = ControlRegistry::new();
        assert!(!reg.signal_open("ghost", "k").await);
    }

    #[tokio::test]
    async fn unregister_drops_the_connection() {
        let reg = ControlRegistry::new();
        let _rx = reg.register("n");
        reg.unregister("n");
        assert_eq!(reg.connected(), 0);
        assert!(!reg.signal_open("n", "k").await);
    }

    /// The control plane + the byte-pipe compose: C signals A to open a leg
    /// under a key, both legs join the broker under that key, and bytes flow.
    #[tokio::test]
    async fn control_signal_drives_a_relay_match() {
        let reg = ControlRegistry::new();
        let broker = RelayBroker::new();

        // Gateway A registers its control connection.
        let mut a_control = reg.register("node-1");

        // A phone arrives at the relay for node-1: it joins the broker and C
        // signals A to open the matching data leg.
        let phone = broker.join("leg-xyz");
        assert!(reg.signal_open("node-1", "leg-xyz").await);

        // A acts on the signal: opens a data leg under the same key.
        let ControlSignal::OpenDataLeg { relay_key } = a_control.recv().await.unwrap();
        let mut gateway = broker.join(&relay_key);

        // The two legs are matched; opaque frames flow blind.
        phone.to_peer.send(b"noise-frame".to_vec()).await.unwrap();
        assert_eq!(gateway.from_peer.recv().await.unwrap(), b"noise-frame");
    }
}
