//! Noise IK sessions for the post-pairing E2E channel.
//!
//! After pairing each side holds the other's X25519 static public key. Every
//! content/blob connection runs a Noise IK handshake whose static-key
//! authentication blocks an active MITM — including by the operator — once the
//! statics were authentically exchanged at pairing.
//!
//! C holds no static private key, so it can never impersonate either end. The
//! `snow` crate provides the primitives; callers stay on these helpers rather
//! than reaching for `snow` directly.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::aead::KEY_LEN;
use crate::error::ProtoError;

/// Plain XX pattern, retained for protocol tests and keypair generation.
pub const NOISE_XX: &str = "Noise_XX_25519_ChaChaPoly_SHA256";
/// IK pattern for production post-pairing content/blob legs.
pub const NOISE_IK: &str = "Noise_IK_25519_ChaChaPoly_SHA256";

/// A long-term X25519 static identity. The gateway persists one in its
/// `SecretVault`; the app persists one in its keystore. The public half is
/// learned by the peer in-band, as an XX handshake token, during the
/// [`crate::psk_pair`] pairing handshake.
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

    /// XX initiator for protocol tests.
    pub fn xx_initiator(&self) -> Result<HandshakeState, ProtoError> {
        Ok(builder(NOISE_XX)?
            .local_private_key(&self.secret)
            .build_initiator()?)
    }

    /// XX responder for protocol tests.
    pub fn xx_responder(&self) -> Result<HandshakeState, ProtoError> {
        Ok(builder(NOISE_XX)?
            .local_private_key(&self.secret)
            .build_responder()?)
    }

    /// IK initiator (reconnect) — needs the responder's known static pubkey.
    pub fn ik_initiator(
        &self,
        remote_static: &[u8; KEY_LEN],
    ) -> Result<HandshakeState, ProtoError> {
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
        .map_err(|_| ProtoError::Handshake(format!("bad noise pattern: {pattern}")))?;
    Ok(Builder::new(params))
}

pub(crate) fn to_key(bytes: &[u8]) -> Result<[u8; KEY_LEN], ProtoError> {
    bytes.try_into().map_err(|_| ProtoError::KeyLen {
        expected: KEY_LEN,
        got: bytes.len(),
    })
}

// --- Framed transport: chunk a frame across Noise messages -------------------
//
// A Noise transport message is capped at 65535 bytes and is atomic (you cannot
// split one ciphertext), so a `Frame` larger than the ceiling is chunked at the
// *plaintext* layer: each frame rides a u32-BE length prefix, and that
// `[len][frame]` record is split into <= MAX_PLAINTEXT_CHUNK pieces, each sealed
// as its own Noise message. In-order, reliable delivery (TCP/WS) + Noise's
// per-message nonce keep both ends in lockstep; the peer reassembles with a
// `FrameReassembler`. Both sides MUST use this codec (they upgrade together).

/// snow's per-transport-message ceiling (ciphertext, including the AEAD tag).
pub const NOISE_MAX_MESSAGE: usize = 65535;
/// The ChaCha20-Poly1305 tag every Noise message carries (ciphertext overhead).
pub const NOISE_TAG_LEN: usize = 16;
/// The most plaintext we seal into one Noise message, so the ciphertext stays
/// within [`NOISE_MAX_MESSAGE`].
pub const MAX_PLAINTEXT_CHUNK: usize = NOISE_MAX_MESSAGE - NOISE_TAG_LEN;
/// Reassembly guard: refuse a length-prefixed frame larger than this rather than
/// honor a garbled or hostile length and allocate unbounded. Far above any real
/// content frame.
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Seal one already-encoded frame as a sequence of Noise transport messages,
/// chunking past [`MAX_PLAINTEXT_CHUNK`]. Returns the messages to send in order.
/// The peer feeds each to a [`FrameReassembler`].
pub fn write_chunked(
    transport: &mut snow::TransportState,
    frame: &[u8],
) -> Result<Vec<Vec<u8>>, ProtoError> {
    if frame.len() > MAX_FRAME_LEN {
        return Err(ProtoError::FrameTooLarge { len: frame.len() });
    }
    let mut record = Vec::with_capacity(4 + frame.len());
    record.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    record.extend_from_slice(frame);

    let mut messages = Vec::new();
    for chunk in record.chunks(MAX_PLAINTEXT_CHUNK) {
        let mut buf = vec![0u8; chunk.len() + NOISE_TAG_LEN];
        let n = transport.write_message(chunk, &mut buf)?;
        buf.truncate(n);
        messages.push(buf);
    }
    Ok(messages)
}

/// Reassembles [`write_chunked`] output: decrypt each inbound Noise message and
/// yield every complete encoded frame it finishes (zero for a partial, one, or
/// several if the peer pipelined). Hold one per session.
#[derive(Default)]
pub struct FrameReassembler {
    buf: Vec<u8>,
}

impl FrameReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decrypt one Noise transport message and return the encoded frames it
    /// completes. Errors (the caller must tear the session down) on a decrypt
    /// failure or an oversized length — both mean a desynced or hostile peer.
    pub fn read(
        &mut self,
        transport: &mut snow::TransportState,
        message: &[u8],
    ) -> Result<Vec<Vec<u8>>, ProtoError> {
        if message.len() > NOISE_MAX_MESSAGE {
            return Err(ProtoError::FrameTooLarge { len: message.len() });
        }
        let mut plain = vec![0u8; message.len()];
        let n = transport.read_message(message, &mut plain)?;
        self.buf.extend_from_slice(&plain[..n]);

        let mut frames = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let len =
                u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
            if len > MAX_FRAME_LEN {
                return Err(ProtoError::FrameTooLarge { len });
            }
            if self.buf.len() < 4 + len {
                break;
            }
            frames.push(self.buf[4..4 + len].to_vec());
            self.buf.drain(..4 + len);
        }
        Ok(frames)
    }
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

    #[test]
    fn chunked_round_trips_small_boundary_and_oversized_frames() {
        let a = StaticKeypair::generate().unwrap();
        let b = StaticKeypair::generate().unwrap();
        let (mut at, mut bt) = complete(
            a.ik_initiator(&b.public()).unwrap(),
            b.ik_responder().unwrap(),
            2,
        );

        let mut reasm = FrameReassembler::new();
        // Sizes around the per-message ceiling: under, at the boundary, and
        // several times over (the case the old one-frame-per-message path dropped).
        for size in [
            1usize,
            4096,
            MAX_PLAINTEXT_CHUNK - 4,
            MAX_PLAINTEXT_CHUNK,
            MAX_PLAINTEXT_CHUNK + 1,
            5 * MAX_PLAINTEXT_CHUNK + 123,
        ] {
            let frame: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let messages = write_chunked(&mut at, &frame).unwrap();
            assert!(
                messages.iter().all(|m| m.len() <= NOISE_MAX_MESSAGE),
                "every chunk fits the Noise ceiling (size {size})",
            );
            let mut got = Vec::new();
            for m in &messages {
                got.extend(reasm.read(&mut bt, m).unwrap());
            }
            assert_eq!(got.len(), 1, "one frame reassembled (size {size})");
            assert_eq!(got[0], frame, "frame intact (size {size})");
        }
    }
}
