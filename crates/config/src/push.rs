//! Optional direct-mode (web-identity) push.
//!
//! Scan-to-pair devices carry their remote-host endpoint in the pairing QR,
//! recorded on the device row — so relay push needs no config. The **direct**
//! transport (URL + admin token, no pairing) has no QR to source it from, so an
//! operator who wants direct/web sessions to receive lock-screen pushes points
//! the gateway at a remote host (C) here. Absent → direct-mode push is disabled
//! (the `POST /v1/push/register` endpoint reports it) and direct chat stays
//! foreground-only; relay push is unaffected either way.
//!
//! Push is **keyless** (`/register` + `/notify` are authorized by the
//! device→gateway Ed25519 delegation chain, not an admission key), so this only
//! needs the remote-host endpoint — no `remote_api_key`.

use serde::{Deserialize, Serialize};

/// The remote host (C) a direct-mode push binding registers + notifies through.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushConfig {
    /// Base WebSocket URL of the remote host, e.g. `wss://proxy.baybo.space`.
    /// The push leg POSTs plain HTTP to the same host (`wss→https`, `ws→http`),
    /// matching the relay path's `relay_url`.
    pub relay_url: String,
}

impl PushConfig {
    /// True once `relay_url` is set — i.e. direct-mode push can register.
    pub fn is_usable(&self) -> bool {
        !self.relay_url.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_requires_relay_url() {
        assert!(
            PushConfig {
                relay_url: "wss://h".into(),
            }
            .is_usable()
        );
        assert!(
            !PushConfig {
                relay_url: "  ".into(),
            }
            .is_usable()
        );
    }

    #[test]
    fn round_trip() {
        let c = PushConfig {
            relay_url: "wss://proxy.baybo.space".into(),
        };
        let back: PushConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
}
