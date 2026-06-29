//! Optional direct-mode (web-identity) push.
//!
//! Scan-to-pair devices carry their remote-host endpoint + admission key in the
//! pairing QR, recorded on the device row — so relay push needs no config. The
//! **direct** transport (URL + admin token, no pairing) has no QR to source them
//! from, so an operator who wants direct/web sessions to receive lock-screen
//! pushes points the gateway at a remote host (C) here. Absent → direct-mode
//! push is disabled (the `POST /v1/push/register` endpoint reports it), and
//! direct chat stays foreground-only; relay push is unaffected either way.

use serde::{Deserialize, Serialize};

/// The remote host (C) a direct-mode push binding registers + notifies through.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PushConfig {
    /// Base WebSocket URL of the remote host, e.g. `wss://proxy.baybo.space`.
    /// The push leg POSTs plain HTTP to the same host (`wss→https`, `ws→http`),
    /// matching the relay path's `relay_url`.
    pub relay_url: String,
    /// The remote host admission key the gateway presents on `/register` +
    /// `/notify` (C's tenant boundary). A shared `guest` key is fine — the
    /// per-binding Ed25519 delegation, not this key, isolates one binding from
    /// another at C.
    pub remote_api_key: String,
}

impl PushConfig {
    /// True once both fields are non-empty — i.e. direct-mode push can register.
    pub fn is_usable(&self) -> bool {
        !self.relay_url.trim().is_empty() && !self.remote_api_key.trim().is_empty()
    }
}

impl std::fmt::Debug for PushConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `remote_api_key` is admission-credential material — never log it.
        f.debug_struct("PushConfig")
            .field("relay_url", &self.relay_url)
            .field("remote_api_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_requires_both_fields() {
        assert!(
            PushConfig {
                relay_url: "wss://h".into(),
                remote_api_key: "k".into(),
            }
            .is_usable()
        );
        assert!(
            !PushConfig {
                relay_url: String::new(),
                remote_api_key: "k".into(),
            }
            .is_usable()
        );
        assert!(
            !PushConfig {
                relay_url: "wss://h".into(),
                remote_api_key: "  ".into(),
            }
            .is_usable()
        );
    }

    #[test]
    fn round_trip() {
        let c = PushConfig {
            relay_url: "wss://proxy.baybo.space".into(),
            remote_api_key: "guest".into(),
        };
        let back: PushConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn debug_redacts_admission_key() {
        let c = PushConfig {
            relay_url: "wss://h".into(),
            remote_api_key: "s3cret-key".into(),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("s3cret"), "key leaked: {shown}");
    }
}
