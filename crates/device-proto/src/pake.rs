//! SPAKE2 over the untrusted rendezvous.
//!
//! Pairing crosses the operator-run `remote-host` (C), which only relays
//! opaque PAKE blobs: it never learns the short code or the derived master
//! secret, and a balanced PAKE allows at most one online guess of the code per
//! run (no offline dictionary attack). The app drives the [`Pake::start_app`]
//! side and the gateway the [`Pake::start_gateway`] side; both must use the
//! same code and identity labels, then exchange one message each and
//! [`Pake::finish`] to the same key — which seeds [`crate::kdf`].
//!
//! The concrete PAKE is an internal, swappable detail; keep callers on this
//! wrapper rather than the `spake2` types directly.

use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::error::ProtoError;

/// Identity label bound into the app side of the handshake.
pub const ID_APP: &[u8] = b"aura-device-app";
/// Identity label bound into the gateway side of the handshake.
pub const ID_GATEWAY: &[u8] = b"aura-device-gateway";

/// One side of an in-flight SPAKE2 exchange. Construct with
/// [`Pake::start_app`] or [`Pake::start_gateway`], send the returned message
/// to the peer over the rendezvous, then [`finish`](Pake::finish) with the
/// peer's message to obtain the shared master secret.
pub struct Pake(Spake2<Ed25519Group>);

impl Pake {
    /// Start the **app (P)** side. Returns the state and the outbound message
    /// to relay to the gateway through C.
    pub fn start_app(code: &str) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_APP),
            &Identity::new(ID_GATEWAY),
        );
        (Self(state), msg)
    }

    /// Start the **gateway (A)** side. Returns the state and the outbound
    /// message to relay to the app through C.
    pub fn start_gateway(code: &str) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(code.as_bytes()),
            &Identity::new(ID_APP),
            &Identity::new(ID_GATEWAY),
        );
        (Self(state), msg)
    }

    /// Consume the state with the peer's message to derive the shared master
    /// secret. Fails on a code mismatch or a corrupted/forged peer message —
    /// the one online guess C (or any relay) gets per run.
    pub fn finish(self, peer_msg: &[u8]) -> Result<Vec<u8>, ProtoError> {
        self.0
            .finish(peer_msg)
            .map_err(|e| ProtoError::Pake(format!("{e:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_codes_agree_on_key() {
        let (app, app_msg) = Pake::start_app("WORMHOLE-7-foo-bar");
        let (gw, gw_msg) = Pake::start_gateway("WORMHOLE-7-foo-bar");
        let app_key = app.finish(&gw_msg).unwrap();
        let gw_key = gw.finish(&app_msg).unwrap();
        assert_eq!(app_key, gw_key, "both sides derive the same master secret");
        assert!(!app_key.is_empty());
    }

    #[test]
    fn mismatched_codes_diverge_or_fail() {
        let (app, app_msg) = Pake::start_app("code-correct");
        let (gw, gw_msg) = Pake::start_gateway("code-WRONG");
        // SPAKE2 either errors at finish or yields non-equal keys on a code
        // mismatch — never a silent agreement.
        if let (Ok(a), Ok(b)) = (app.finish(&gw_msg), gw.finish(&app_msg)) {
            assert_ne!(a, b, "wrong code must not agree");
        }
    }

    #[test]
    fn corrupt_peer_message_is_rejected() {
        let (app, _app_msg) = Pake::start_app("code");
        let (_gw, mut gw_msg) = Pake::start_gateway("code");
        gw_msg[0] ^= 0xff;
        // A mangled curve point is rejected rather than producing a key.
        assert!(app.finish(&gw_msg).is_err());
    }
}
