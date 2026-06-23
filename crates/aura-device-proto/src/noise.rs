//! Noise sessions for the post-pairing E2E channel.
//!
//! After pairing each side holds the other's X25519 static public key. Every
//! connection (direct or relayed through C) runs a Noise handshake whose
//! static-key authentication blocks an active MITM — including by the operator
//! — once the statics were authentically exchanged at pairing. We use:
//!
//! - **XX** for first contact (mutual static exchange in-band), and
//! - **IK** for reconnects (the initiator already knows the responder's
//!   static, so it is near-0-RTT).
//!
//! C holds no static private key, so it can never impersonate either end. The
//! `snow` crate provides the primitives; callers stay on these helpers rather
//! than reaching for `snow` directly.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::aead::KEY_LEN;
use crate::error::ProtoError;

/// Noise pattern for first contact (mutual static exchange).
pub const NOISE_XX: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
/// Noise pattern for reconnects (initiator knows responder's static).
pub const NOISE_IK: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

/// A long-term X25519 static identity. The gateway persists one in its
/// `SecretVault`; the app persists one in its keystore. Only the public half
/// crosses the wire (inside the SPAKE2 K-channel at pairing).
pub struct StaticKeypair {
    public: [u8; KEY_LEN],
    secret: [u8; KEY_LEN],
}

impl StaticKeypair {
    /// Generate a fresh X25519 static keypair.
    pub fn generate() -> Result<Self, ProtoError> {
        let kp = builder(NOISE_XX)?.generate_keypair()?;
        Ok(Self {
            public: to_key(&kp.public)?,
            secret: to_key(&kp.private)?,
        })
    }

    /// Reconstruct from a persisted `(public, secret)` pair.
    pub fn from_parts(public: [u8; KEY_LEN], secret: [u8; KEY_LEN]) -> Self {
        Self { public, secret }
    }

    /// The public half to advertise at pairing.
    pub fn public(&self) -> [u8; KEY_LEN] {
        self.public
    }

    /// The secret half — keep in a `SecretVault` / keystore, never on the wire.
    pub fn secret(&self) -> [u8; KEY_LEN] {
        self.secret
    }

    /// XX initiator (first contact).
    pub fn xx_initiator(&self) -> Result<HandshakeState, ProtoError> {
        Ok(builder(NOISE_XX)?
            .local_private_key(&self.secret)
            .build_initiator()?)
    }

    /// XX responder (first contact).
    pub fn xx_responder(&self) -> Result<HandshakeState, ProtoError> {
        Ok(builder(NOISE_XX)?
            .local_private_key(&self.secret)
            .build_responder()?)
    }

    /// IK initiator (reconnect) — needs the responder's known static pubkey.
    pub fn ik_initiator(&self, remote_static: &[u8; KEY_LEN]) -> Result<HandshakeState, ProtoError> {
        Ok(builder(NOISE_IK)?
            .local_private_key(&self.secret)
            .remote_public_key(remote_static)
            .build_initiator()?)
    }

    /// IK responder (reconnect).
    pub fn ik_responder(&self) -> Result<HandshakeState, ProtoError> {
        Ok(builder(NOISE_IK)?
            .local_private_key(&self.secret)
            .build_responder()?)
    }
}

fn builder(pattern: &str) -> Result<Builder<'static>, ProtoError> {
    let params: NoiseParams = pattern
        .parse()
        .map_err(|_| ProtoError::Pake(format!("bad noise pattern: {pattern}")))?;
    Ok(Builder::new(params))
}

fn to_key(bytes: &[u8]) -> Result<[u8; KEY_LEN], ProtoError> {
    bytes.try_into().map_err(|_| ProtoError::KeyLen {
        expected: KEY_LEN,
        got: bytes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a full handshake to completion, returning both transport states.
    fn complete(
        mut ini: HandshakeState,
        mut res: HandshakeState,
        messages: usize,
    ) -> (snow::TransportState, snow::TransportState) {
        let mut buf = [0u8; 4096];
        let mut out = [0u8; 4096];
        for i in 0..messages {
            // Even index: initiator → responder; odd: responder → initiator.
            let (writer, reader) = if i % 2 == 0 {
                (&mut ini, &mut res)
            } else {
                (&mut res, &mut ini)
            };
            let n = writer.write_message(&[], &mut buf).unwrap();
            reader.read_message(&buf[..n], &mut out).unwrap();
        }
        (
            ini.into_transport_mode().unwrap(),
            res.into_transport_mode().unwrap(),
        )
    }

    #[test]
    fn xx_handshake_authenticates_both_statics() {
        let a = StaticKeypair::generate().unwrap();
        let b = StaticKeypair::generate().unwrap();
        let ini = a.xx_initiator().unwrap();
        let res = b.xx_responder().unwrap();

        // Run the 3-message XX exchange, checking learned statics before the
        // handshake states are consumed into transport mode.
        let mut ini = ini;
        let mut res = res;
        let mut buf = [0u8; 4096];
        let mut out = [0u8; 4096];
        // -> e
        let n = ini.write_message(&[], &mut buf).unwrap();
        res.read_message(&buf[..n], &mut out).unwrap();
        // <- e, ee, s, es
        let n = res.write_message(&[], &mut buf).unwrap();
        ini.read_message(&buf[..n], &mut out).unwrap();
        // -> s, se
        let n = ini.write_message(&[], &mut buf).unwrap();
        res.read_message(&buf[..n], &mut out).unwrap();

        assert_eq!(
            ini.get_remote_static().unwrap(),
            b.public(),
            "initiator learned responder's static"
        );
        assert_eq!(
            res.get_remote_static().unwrap(),
            a.public(),
            "responder learned initiator's static"
        );

        let mut at = ini.into_transport_mode().unwrap();
        let mut bt = res.into_transport_mode().unwrap();
        let n = at.write_message(b"hello over noise", &mut buf).unwrap();
        let m = bt.read_message(&buf[..n], &mut out).unwrap();
        assert_eq!(&out[..m], b"hello over noise");
    }

    #[test]
    fn ik_reconnect_round_trips() {
        let a = StaticKeypair::generate().unwrap();
        let b = StaticKeypair::generate().unwrap();
        // Initiator A already knows responder B's static (learned at pairing).
        let ini = a.ik_initiator(&b.public()).unwrap();
        let res = b.ik_responder().unwrap();
        let (mut at, mut bt) = complete(ini, res, 2);

        let mut buf = [0u8; 4096];
        let mut out = [0u8; 4096];
        let n = at.write_message(b"reconnect frame", &mut buf).unwrap();
        let m = bt.read_message(&buf[..n], &mut out).unwrap();
        assert_eq!(&out[..m], b"reconnect frame");
        // Reverse direction too.
        let n = bt.write_message(b"reply frame", &mut buf).unwrap();
        let m = at.read_message(&buf[..n], &mut out).unwrap();
        assert_eq!(&out[..m], b"reply frame");
    }

    #[test]
    fn persisted_keypair_round_trips() {
        let kp = StaticKeypair::generate().unwrap();
        let restored = StaticKeypair::from_parts(kp.public(), kp.secret());
        assert_eq!(kp.public(), restored.public());
        // A reconstructed keypair completes a handshake against a peer that
        // knows its static.
        let peer = StaticKeypair::generate().unwrap();
        let ini = peer.ik_initiator(&restored.public()).unwrap();
        let res = restored.ik_responder().unwrap();
        let (mut pt, mut rt) = complete(ini, res, 2);
        let mut buf = [0u8; 4096];
        let mut out = [0u8; 4096];
        let n = pt.write_message(b"ping", &mut buf).unwrap();
        let m = rt.read_message(&buf[..n], &mut out).unwrap();
        assert_eq!(&out[..m], b"ping");
    }
}
