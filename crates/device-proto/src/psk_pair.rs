//! The XXpsk0 device-pairing handshake over the untrusted rendezvous.
//!
//! Pairing crosses the operator-run `remote-host` (**C**), which a hostile
//! operator can fully control. The previous SPAKE2 design was broken against
//! that threat: C had to read the pairing *code* to route the rendezvous, and
//! that code was *also* the SPAKE2 password, so C held the password by
//! construction and could mount a persistent man-in-the-middle (see
//! `docs/todo/mobile/pairing-mitm-xxpsk.md`).
//!
//! The fix splits the one value into two:
//!
//! - a public **rendezvous id** (a UUID) that C sees and routes on, and
//! - a high-entropy **secret** ([`PairingSecret`]) carried *only* in the QR,
//!   never over C, used as the Noise **PSK**.
//!
//! Pairing then runs `Noise_XXpsk0_25519_ChaChaPoly_SHA256`: the app is the
//! initiator, the gateway the responder, and the QR secret authenticates the
//! otherwise-anonymous in-band static exchange. A relay that lacks the PSK
//! cannot complete the handshake with either side — it is reduced from MITM to
//! denial-of-service.
//!
//! ## The load-bearing invariant: the secret must be high-entropy
//!
//! A Noise PSK is **not** a PAKE. Against C — an *active* participant — one
//! captured transcript is enough to mount an **offline** dictionary attack on
//! the PSK. SPAKE2's "one online guess, no offline attack" property does not
//! survive the switch, so a short / typeable secret would be *worse* than the
//! old code. [`PairingSecret`] is therefore a 256-bit CSPRNG value with **no
//! string constructor** and no typeable form — it only ever travels in the QR.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, TransportState};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::aead::KEY_LEN;
use crate::error::ProtoError;
use crate::noise::{NOISE_MAX_MESSAGE, NOISE_TAG_LEN, to_key};

/// Noise pattern for the device-pairing handshake. `XX` exchanges both statics
/// in-band (neither side knows the other's static ahead of time); `psk0` mixes
/// the QR secret in before the first ephemeral, authenticating that exchange.
pub const NOISE_XXPSK0: &str = "Noise_XXpsk0_25519_ChaChaPoly_SHA256";

/// Versioned prologue tag. Folded into the handshake hash `h` (not secret), so a
/// one-byte disagreement aborts the handshake. Bump on any prologue layout
/// change so an old and new peer can never silently agree on a key.
pub const PROLOGUE_VERSION: &[u8] = b"baybo/device/pair/v1";

/// Role label for the app side of the handshake, bound into the prologue
/// (replaces SPAKE2's `ID_APP`; blocks reflection / role confusion).
pub const ROLE_APP: &[u8] = b"baybo-device-app";
/// Role label for the gateway side of the handshake, bound into the prologue.
pub const ROLE_GATEWAY: &[u8] = b"baybo-device-gateway";

/// The most plaintext a single pairing transport message carries. The pairing
/// messages (`DeviceConfirm`, `GatewayWelcome`) are tiny, so — unlike the
/// content channel — pairing never needs the chunking codec; one Noise message
/// per frame, guarded by this ceiling.
const MAX_PAIR_PLAINTEXT: usize = NOISE_MAX_MESSAGE - NOISE_TAG_LEN;

/// Bytes a single XXpsk0 handshake message can add on top of its payload: an
/// ephemeral pubkey (32), the encrypted static (32 + 16 tag), and the payload's
/// own AEAD tag (16). Sizing the output buffer by `payload + this` keeps `snow`
/// from rejecting a too-small buffer.
const HANDSHAKE_OVERHEAD: usize = KEY_LEN + (KEY_LEN + NOISE_TAG_LEN) + NOISE_TAG_LEN;

/// The 256-bit pairing secret used as the Noise PSK.
///
/// **Invariant (load-bearing — see the module docs):** this is CSPRNG-generated
/// and high-entropy. It has no `From<String>` / `FromStr` and no typeable form —
/// the only constructors are [`generate`](Self::generate) (fresh CSPRNG) and
/// [`from_bytes`](Self::from_bytes) (32 raw bytes decoded from the QR). A short
/// or human-typed secret would be offline-crackable from a single transcript by
/// the relay and is therefore *forbidden* by construction.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct PairingSecret([u8; KEY_LEN]);

impl PairingSecret {
    /// Mint a fresh secret from the OS CSPRNG. `rand::rng()` is the ChaCha-based
    /// thread RNG (a CSPRNG), reseeded from the OS — suitable for a 256-bit key.
    pub fn generate() -> Self {
        use rand::RngExt;
        let mut rng = rand::rng();
        Self(std::array::from_fn(|_| rng.random_range(0..=255) as u8))
    }

    /// Reconstruct from the 32 raw bytes decoded out of the QR. The decode +
    /// length check (exactly [`KEY_LEN`] bytes) is the gate that keeps a short
    /// secret off this path — callers must decode to a `[u8; KEY_LEN]` first.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw secret bytes, for use as the Noise PSK. Never log or persist
    /// these outside the QR / the in-memory pairing slot.
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// Redacted on purpose: the slot DTO that holds a `PairingSecret` derives
/// `Debug`, and a secret must never reach a log line through it.
impl std::fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingSecret(***)")
    }
}

/// Build the canonical, versioned, length-prefixed prologue both ends must
/// construct identically: `version ‖ rendezvous_id ‖ endpoint ‖ role-app ‖
/// role-gateway`. Authenticated (folded into `h`) but **not** secret — the PSK is
/// mixed separately and never appears here.
///
/// Binding `rendezvous_id` + `endpoint` gives anti-splice / anti-cross-binding
/// (the relay can't wire app-of-rendezvous-X to gateway-of-rendezvous-Y, nor
/// cross-bind two different relays), and the role labels replace SPAKE2's
/// identity labels (blocking reflection / role confusion). `endpoint` is
/// normalized (trailing `/` trimmed) so the app's QR `h=` and the gateway's
/// configured relay URL agree byte-for-byte.
pub fn build_prologue(rendezvous_id: &str, endpoint: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for field in [
        PROLOGUE_VERSION,
        rendezvous_id.as_bytes(),
        endpoint.trim_end_matches('/').as_bytes(),
        ROLE_APP,
        ROLE_GATEWAY,
    ] {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// An in-flight XXpsk0 handshake. Drive it with [`write_handshake`] /
/// [`read_handshake`] in the pattern's message order, then
/// [`into_transport`](Self::into_transport) once it is finished.
///
/// [`write_handshake`]: Self::write_handshake
/// [`read_handshake`]: Self::read_handshake
pub struct PskHandshake {
    hs: HandshakeState,
}

impl PskHandshake {
    /// Start the **app (initiator)** side: writes `msg1` (`e`). `static_secret`
    /// is the app's long-term Noise static secret; `secret` is the QR PSK;
    /// `prologue` comes from [`build_prologue`].
    pub fn start_initiator(
        static_secret: &[u8; KEY_LEN],
        secret: &PairingSecret,
        prologue: &[u8],
    ) -> Result<Self, ProtoError> {
        Self::start(true, static_secret, secret, prologue)
    }

    /// Start the **gateway (responder)** side.
    pub fn start_responder(
        static_secret: &[u8; KEY_LEN],
        secret: &PairingSecret,
        prologue: &[u8],
    ) -> Result<Self, ProtoError> {
        Self::start(false, static_secret, secret, prologue)
    }

    fn start(
        initiator: bool,
        static_secret: &[u8; KEY_LEN],
        secret: &PairingSecret,
        prologue: &[u8],
    ) -> Result<Self, ProtoError> {
        let params: NoiseParams = NOISE_XXPSK0
            .parse()
            .map_err(|_| ProtoError::Handshake(format!("bad noise pattern: {NOISE_XXPSK0}")))?;
        let builder = Builder::new(params)
            .local_private_key(static_secret)
            .prologue(prologue)
            .psk(0, secret.as_bytes());
        let hs = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        Ok(Self { hs })
    }

    /// Write the next handshake message, carrying `payload` (empty for `msg1` /
    /// `msg2`; the `DeviceHello` body for the app's `msg3`).
    pub fn write_handshake(&mut self, payload: &[u8]) -> Result<Vec<u8>, ProtoError> {
        if payload.len() > MAX_PAIR_PLAINTEXT {
            return Err(ProtoError::FrameTooLarge { len: payload.len() });
        }
        let mut buf = vec![0u8; payload.len() + HANDSHAKE_OVERHEAD];
        let n = self.hs.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Read the next handshake message, returning its (decrypted) payload. A
    /// wrong PSK, tampered frame, or mismatched prologue fails here — fail
    /// closed, no key.
    pub fn read_handshake(&mut self, message: &[u8]) -> Result<Vec<u8>, ProtoError> {
        if message.len() > NOISE_MAX_MESSAGE {
            return Err(ProtoError::FrameTooLarge { len: message.len() });
        }
        let mut buf = vec![0u8; message.len()];
        let n = self.hs.read_message(message, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Whether every handshake message has been processed (transport mode is
    /// reachable).
    pub fn is_finished(&self) -> bool {
        self.hs.is_handshake_finished()
    }

    /// Finish the handshake and capture the channel-binding material the
    /// confirm code + push key derive from. Errors if called before the
    /// pattern's last message.
    pub fn into_transport(self) -> Result<PskTransport, ProtoError> {
        let handshake_hash = to_key(self.hs.get_handshake_hash())?;
        let remote_static = self
            .hs
            .get_remote_static()
            .map(to_key)
            .transpose()?
            .ok_or_else(|| ProtoError::Handshake("peer static not learned".into()))?;
        let transport = self.hs.into_transport_mode()?;
        Ok(PskTransport {
            transport,
            handshake_hash,
            remote_static,
        })
    }
}

/// A completed pairing session in transport mode, plus the channel-binding
/// values both ends derive identically from the handshake.
pub struct PskTransport {
    transport: TransportState,
    handshake_hash: [u8; KEY_LEN],
    remote_static: [u8; KEY_LEN],
}

impl PskTransport {
    /// The Noise handshake hash `h`. It commits to the whole transcript —
    /// prologue, both ephemerals, the PSK mix, **and both statics** — so a
    /// confirm code derived from it proves a single, live, untampered session
    /// and is not grindable or precomputable (see [`crate::kdf`]).
    pub fn handshake_hash(&self) -> &[u8; KEY_LEN] {
        &self.handshake_hash
    }

    /// The peer's authenticated static public key, learned in-band as an XX
    /// token: the gateway's static for the app, the app's static for the
    /// gateway. This is the identity the post-pairing content channel pins.
    pub fn remote_static(&self) -> &[u8; KEY_LEN] {
        &self.remote_static
    }

    /// Seal one small pairing message (`DeviceConfirm` / `GatewayWelcome`) as a
    /// single Noise transport message (implicit nonce).
    pub fn write(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, ProtoError> {
        if plaintext.len() > MAX_PAIR_PLAINTEXT {
            return Err(ProtoError::FrameTooLarge {
                len: plaintext.len(),
            });
        }
        let mut buf = vec![0u8; plaintext.len() + NOISE_TAG_LEN];
        let n = self.transport.write_message(plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Open one sealed pairing message.
    pub fn read(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, ProtoError> {
        if ciphertext.len() > NOISE_MAX_MESSAGE {
            return Err(ProtoError::FrameTooLarge {
                len: ciphertext.len(),
            });
        }
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self.transport.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::StaticKeypair;

    const RID: &str = "11111111-2222-4333-8444-555555555555";
    const ENDPOINT: &str = "wss://proxy.baybo.space";

    /// Run the 3-message XXpsk0 handshake between an app and a gateway sharing
    /// the same secret + prologue. Returns both transports plus the `msg3`
    /// payload the gateway recovered (the app's `DeviceHello` body).
    fn run_handshake(
        secret: &PairingSecret,
        app_static: &StaticKeypair,
        gw_static: &StaticKeypair,
        msg3_payload: &[u8],
    ) -> (PskTransport, PskTransport, Vec<u8>) {
        let prologue = build_prologue(RID, ENDPOINT);
        let mut app =
            PskHandshake::start_initiator(&app_static.secret(), secret, &prologue).unwrap();
        let mut gw = PskHandshake::start_responder(&gw_static.secret(), secret, &prologue).unwrap();

        let msg1 = app.write_handshake(&[]).unwrap();
        assert!(gw.read_handshake(&msg1).unwrap().is_empty());
        let msg2 = gw.write_handshake(&[]).unwrap();
        assert!(app.read_handshake(&msg2).unwrap().is_empty());
        let msg3 = app.write_handshake(msg3_payload).unwrap();
        let recovered = gw.read_handshake(&msg3).unwrap();

        let app_t = app.into_transport().unwrap();
        let gw_t = gw.into_transport().unwrap();
        (app_t, gw_t, recovered)
    }

    #[test]
    fn matching_secret_agrees_on_h_statics_and_transport() {
        let secret = PairingSecret::generate();
        let app_kp = StaticKeypair::generate().unwrap();
        let gw_kp = StaticKeypair::generate().unwrap();

        let (mut app_t, mut gw_t, payload) =
            run_handshake(&secret, &app_kp, &gw_kp, b"device-hello-body");

        // Both ends derive the same channel-binding hash...
        assert_eq!(app_t.handshake_hash(), gw_t.handshake_hash());
        // ...and each learned the other's real static in-band.
        assert_eq!(app_t.remote_static(), &gw_kp.public());
        assert_eq!(gw_t.remote_static(), &app_kp.public());
        // ...and msg3 carried the DeviceHello body to the gateway.
        assert_eq!(payload, b"device-hello-body");

        // Transport flows both ways (DeviceConfirm up, GatewayWelcome down).
        let up = app_t.write(b"confirm").unwrap();
        assert_eq!(gw_t.read(&up).unwrap(), b"confirm");
        let down = gw_t.write(b"welcome").unwrap();
        assert_eq!(app_t.read(&down).unwrap(), b"welcome");
    }

    #[test]
    fn wrong_secret_fails_the_handshake() {
        let app_kp = StaticKeypair::generate().unwrap();
        let gw_kp = StaticKeypair::generate().unwrap();
        let prologue = build_prologue(RID, ENDPOINT);

        let mut app =
            PskHandshake::start_initiator(&app_kp.secret(), &PairingSecret::generate(), &prologue)
                .unwrap();
        let mut gw =
            PskHandshake::start_responder(&gw_kp.secret(), &PairingSecret::generate(), &prologue)
                .unwrap();

        // msg1 / msg2 carry no MAC the responder/initiator can yet check, but
        // the PSK mismatch surfaces by msg3 at the latest — the gateway cannot
        // open the app's static + payload. Drive until it fails.
        let msg1 = app.write_handshake(&[]).unwrap();
        let _ = gw.read_handshake(&msg1);
        let msg2 = match gw.write_handshake(&[]) {
            Ok(m) => m,
            Err(_) => return, // already failed — fine, fail closed
        };
        if app.read_handshake(&msg2).is_err() {
            return;
        }
        let msg3 = app.write_handshake(b"hello").unwrap();
        assert!(
            gw.read_handshake(&msg3).is_err(),
            "a wrong PSK must abort the handshake, never agree"
        );
    }

    #[test]
    fn mismatched_prologue_fails_the_handshake() {
        let secret = PairingSecret::generate();
        let app_kp = StaticKeypair::generate().unwrap();
        let gw_kp = StaticKeypair::generate().unwrap();

        let app_pro = build_prologue(RID, ENDPOINT);
        // The relay tries to cross-bind the app to a gateway on a different relay
        // endpoint — the prologues then disagree.
        let gw_pro = build_prologue(RID, "wss://evil.example");

        let mut app = PskHandshake::start_initiator(&app_kp.secret(), &secret, &app_pro).unwrap();
        let mut gw = PskHandshake::start_responder(&gw_kp.secret(), &secret, &gw_pro).unwrap();

        let msg1 = app.write_handshake(&[]).unwrap();
        let _ = gw.read_handshake(&msg1);
        let msg2 = match gw.write_handshake(&[]) {
            Ok(m) => m,
            Err(_) => return,
        };
        if app.read_handshake(&msg2).is_err() {
            return;
        }
        let msg3 = app.write_handshake(b"hello").unwrap();
        assert!(
            gw.read_handshake(&msg3).is_err(),
            "a one-byte prologue disagreement must abort the handshake"
        );
    }

    #[test]
    fn endpoint_trailing_slash_is_normalized() {
        assert_eq!(
            build_prologue(RID, "wss://x/"),
            build_prologue(RID, "wss://x"),
        );
    }

    #[test]
    fn pairing_secret_redacts_in_debug() {
        let s = PairingSecret::from_bytes([7u8; KEY_LEN]);
        assert_eq!(format!("{s:?}"), "PairingSecret(***)");
    }

    #[test]
    fn generated_secrets_are_distinct() {
        // A 256-bit CSPRNG value collides with negligible probability.
        assert_ne!(
            PairingSecret::generate().as_bytes(),
            PairingSecret::generate().as_bytes(),
        );
    }
}
