//! Sealed candidate sets and punch authentication for direct carriers.
//!
//! P and A exchange their host candidates through C, which must learn nothing
//! from the sets. Both sides derive the keys from the Noise IK statics they
//! learned at pairing (X25519 static-static, then HKDF-SHA256), so no key
//! crosses the wire and C, holding neither static secret, can neither read nor
//! forge a set.
//!
//! - P holds a [`DeviceSealer`] and A a [`GatewaySealer`]. Each type carries
//!   only its own side's verbs.
//! - P seals a [`DeviceOffer`] under `k_p2a`, A seals a [`GatewayAnswer`] under
//!   `k_a2p`. The keys are directional, so a set reflected back to its sender
//!   never opens as if the peer had sent it.
//! - Each set is XChaCha20-Poly1305 with a fresh random 24-byte nonce. The
//!   associated data binds the relay node id, and for an answer also the punch
//!   id.
//! - The plaintext is `u16be(len) ‖ msgpack(body) ‖ zero padding` to one fixed
//!   length per message kind, so the ciphertext length reveals neither the
//!   candidate count nor whether A offers TCP.
//! - P's punch datagrams carry `HMAC-SHA256(k_punch, offer_id ‖ u16be(seq))`
//!   truncated to [`PUNCH_TAG_LEN`]. P mints them and A only verifies them, so
//!   A's own punches never carry a valid tag.
//!
//! Any seal, length, framing, version or cap failure refuses the whole set.

use std::fmt;
use std::io;
use std::net::SocketAddr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::generic_array::typenum::Unsigned;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use hmac::digest::OutputSizeUser;
use hmac::{Hmac, Mac};
use remote_host_protocol::relay::{
    DirectToken, MAX_SEALED_CANDIDATES_BYTES, MAX_TCP_CANDIDATES, MAX_UDP_HOST_CANDIDATES,
    PUNCH_TAG_LEN, PunchId, PunchTag, SEALED_ANSWER_PLAINTEXT_LEN, SEALED_OFFER_PLAINTEXT_LEN,
    SealedCandidates,
};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snow::params::DHChoice;
use snow::resolvers::{CryptoResolver, DefaultResolver};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::aead::{KEY_LEN, TAG_LEN};
use crate::error::{CandidateRejection, ProtoError};

/// The version of the sealed candidate body this build speaks. A set of any
/// other version is refused whole.
pub const CANDIDATE_SET_VERSION: u8 = 1;
/// Raw length of an [`OfferId`].
pub const OFFER_ID_LEN: usize = 16;
/// Raw length of a [`CertHash`]: one SHA-256 digest.
pub const CERT_HASH_LEN: usize = 32;
const SEALED_NONCE_LEN: usize = <<XChaCha20Poly1305 as AeadCore>::NonceSize as Unsigned>::USIZE;

const STATIC_DH_SALT: &[u8] = b"baybo/direct/static-dh/v1";
const SEAL_DEVICE_TO_GATEWAY_INFO: &[u8] = b"baybo/direct/seal/device-to-gateway/v1";
const SEAL_GATEWAY_TO_DEVICE_INFO: &[u8] = b"baybo/direct/seal/gateway-to-device/v1";
const PUNCH_DEVICE_TO_GATEWAY_INFO: &[u8] = b"baybo/direct/punch/device-to-gateway/v1";
const OFFER_AAD_DOMAIN: &[u8] = b"baybo/direct/offer/v1";
const ANSWER_AAD_DOMAIN: &[u8] = b"baybo/direct/answer/v1";

const BODY_LEN_PREFIX_LEN: usize = size_of::<u16>();
const UDP_LIST: &str = "udp";
const TCP_LIST: &str = "tcp";
/// Whether [`BASE64`] pads, which [`sealed_wire_len`] must know at compile time.
const BASE64_PADDED: bool = true;

type HmacSha256 = Hmac<Sha256>;

/// The base64 length of a sealed set whose plaintext is `plaintext_len` bytes,
/// as [`SealedCandidates::is_within_bounds`] counts it.
const fn sealed_wire_len(plaintext_len: usize) -> Option<usize> {
    match (
        base64::encoded_len(SEALED_NONCE_LEN, BASE64_PADDED),
        base64::encoded_len(plaintext_len + TAG_LEN, BASE64_PADDED),
    ) {
        (Some(nonce), Some(ciphertext)) => nonce.checked_add(ciphertext),
        _ => None,
    }
}

const fn within_the_wire_cap(plaintext_len: usize) -> bool {
    matches!(sealed_wire_len(plaintext_len), Some(len) if len <= MAX_SEALED_CANDIDATES_BYTES)
}

const _: () = assert!(within_the_wire_cap(SEALED_OFFER_PLAINTEXT_LEN));
const _: () = assert!(within_the_wire_cap(SEALED_ANSWER_PLAINTEXT_LEN));
const _: () = assert!(SEALED_ANSWER_PLAINTEXT_LEN <= u16::MAX as usize);
const _: () = assert!(SEALED_OFFER_PLAINTEXT_LEN <= u16::MAX as usize);
const _: () =
    assert!(PUNCH_TAG_LEN <= <<HmacSha256 as OutputSizeUser>::OutputSize as Unsigned>::USIZE);

/// P's per-probe freshness challenge: 16 CSPRNG bytes. A keys its replay
/// cache on it, P requires A's answer to echo it, and it is the punch-tag
/// input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OfferId(#[serde(with = "serde_bytes")] [u8; OFFER_ID_LEN]);

impl OfferId {
    pub fn generate() -> Self {
        Self(rand::random())
    }

    pub(crate) fn as_bytes(&self) -> &[u8; OFFER_ID_LEN] {
        &self.0
    }
}

/// SHA-256 of A's per-process QUIC certificate (DER), which P pins. Compared
/// in constant time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CertHash(#[serde(with = "serde_bytes")] [u8; CERT_HASH_LEN]);

impl CertHash {
    pub fn of_certificate(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }
}

impl PartialEq for CertHash {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for CertHash {}

/// P → A, sealed under `k_p2a`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceOffer {
    pub v: u8,
    /// P's wall clock, unix ms.
    pub issued_at_ms: u64,
    pub offer_id: OfferId,
    /// P's host candidates on its probe sockets.
    pub udp: Vec<SocketAddr>,
}

impl DeviceOffer {
    /// A current-version offer with a fresh [`OfferId`].
    pub fn new(issued_at_ms: u64, udp: Vec<SocketAddr>) -> Self {
        Self {
            v: CANDIDATE_SET_VERSION,
            issued_at_ms,
            offer_id: OfferId::generate(),
            udp,
        }
    }

    fn check(&self) -> Result<(), CandidateRejection> {
        check_version(self.v)?;
        check_count(UDP_LIST, self.udp.len(), MAX_UDP_HOST_CANDIDATES)
    }
}

/// A → P, sealed under `k_a2p`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayAnswer {
    pub v: u8,
    /// Informational: A's wall clock, used only to log skew.
    pub issued_at_ms: u64,
    /// Echo of [`DeviceOffer::offer_id`].
    pub offer_id: OfferId,
    /// The `DirectOpen` gate, minted per binding runtime.
    pub token: DirectToken,
    pub quic_cert_sha256: CertHash,
    /// A's host candidates on its stable sockets.
    pub udp: Vec<SocketAddr>,
    /// Empty unless `gateway.direct_tcp` is configured.
    #[serde(default)]
    pub tcp: Vec<SocketAddr>,
}

impl GatewayAnswer {
    fn check(&self) -> Result<(), CandidateRejection> {
        check_version(self.v)?;
        check_count(UDP_LIST, self.udp.len(), MAX_UDP_HOST_CANDIDATES)?;
        check_count(TCP_LIST, self.tcp.len(), MAX_TCP_CANDIDATES)
    }
}

fn check_version(v: u8) -> Result<(), CandidateRejection> {
    if v == CANDIDATE_SET_VERSION {
        Ok(())
    } else {
        Err(CandidateRejection::Version { got: v })
    }
}

fn check_count(list: &'static str, len: usize, max: usize) -> Result<(), CandidateRejection> {
    if len <= max {
        Ok(())
    } else {
        Err(CandidateRejection::Count { list, len, max })
    }
}

/// The three candidate keys of one pairing, named by direction. Both sides
/// derive the same three; which of them a side may use is fixed by its sealer
/// type.
struct PairingKeys {
    p2a: Zeroizing<[u8; KEY_LEN]>,
    a2p: Zeroizing<[u8; KEY_LEN]>,
    punch: Zeroizing<[u8; KEY_LEN]>,
}

impl fmt::Debug for PairingKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl PairingKeys {
    fn derive(own_secret: &[u8; KEY_LEN], peer_public: &[u8; KEY_LEN]) -> Result<Self, ProtoError> {
        let shared = static_dh(own_secret, peer_public)?;
        let hkdf = Hkdf::<Sha256>::new(Some(STATIC_DH_SALT), shared.as_slice());
        Ok(Self {
            p2a: expand(&hkdf, SEAL_DEVICE_TO_GATEWAY_INFO)?,
            a2p: expand(&hkdf, SEAL_GATEWAY_TO_DEVICE_INFO)?,
            punch: expand(&hkdf, PUNCH_DEVICE_TO_GATEWAY_INFO)?,
        })
    }
}

/// P's candidate keys for one pairing: it seals offers, opens answers and tags
/// its punches. P derives one per probe.
#[derive(Debug)]
pub struct DeviceSealer {
    keys: PairingKeys,
}

impl DeviceSealer {
    /// Derive from P's static secret and A's static public key. Fails closed on
    /// a low-order key.
    pub fn derive(
        own_secret: &[u8; KEY_LEN],
        gateway_public: &[u8; KEY_LEN],
    ) -> Result<Self, ProtoError> {
        PairingKeys::derive(own_secret, gateway_public).map(|keys| Self { keys })
    }

    /// Seal P's offer for `relay_node_id`.
    pub fn seal_offer(
        &self,
        relay_node_id: &str,
        offer: &DeviceOffer,
    ) -> Result<SealedCandidates, ProtoError> {
        offer.check()?;
        seal_body(
            &self.keys.p2a,
            &offer_aad(relay_node_id),
            offer,
            SEALED_OFFER_PLAINTEXT_LEN,
        )
    }

    /// Open the answer C returned for `punch_id`, and accept it only when it
    /// echoes `offer_id`, the id of P's own in-flight offer.
    pub fn open_answer(
        &self,
        relay_node_id: &str,
        punch_id: &PunchId,
        offer_id: &OfferId,
        sealed: &SealedCandidates,
    ) -> Result<GatewayAnswer, ProtoError> {
        let answer: GatewayAnswer = open_body(
            &self.keys.a2p,
            &answer_aad(relay_node_id, punch_id),
            sealed,
            SEALED_ANSWER_PLAINTEXT_LEN,
        )?;
        answer.check()?;
        if answer.offer_id != *offer_id {
            return Err(CandidateRejection::OfferIdMismatch.into());
        }
        Ok(answer)
    }

    /// The tag of P's punch number `seq` for the offer `offer_id`.
    pub fn punch_tag(&self, offer_id: &OfferId, seq: u16) -> Result<PunchTag, ProtoError> {
        punch_tag(&self.keys.punch, offer_id, seq)
    }
}

/// A's candidate keys for one pairing: it opens offers, seals answers and
/// verifies P's punch tags. It cannot mint a tag, so A's own punches carry a
/// random one. A holds one per binding runtime.
#[derive(Debug)]
pub struct GatewaySealer {
    keys: PairingKeys,
}

impl GatewaySealer {
    /// Derive from A's static secret and P's static public key. Fails closed on
    /// a low-order key.
    pub fn derive(
        own_secret: &[u8; KEY_LEN],
        device_public: &[u8; KEY_LEN],
    ) -> Result<Self, ProtoError> {
        PairingKeys::derive(own_secret, device_public).map(|keys| Self { keys })
    }

    /// Open an offer C forwarded for `relay_node_id`. Freshness and replay are
    /// A's to check on the result.
    pub fn open_offer(
        &self,
        relay_node_id: &str,
        sealed: &SealedCandidates,
    ) -> Result<DeviceOffer, ProtoError> {
        let offer: DeviceOffer = open_body(
            &self.keys.p2a,
            &offer_aad(relay_node_id),
            sealed,
            SEALED_OFFER_PLAINTEXT_LEN,
        )?;
        offer.check()?;
        Ok(offer)
    }

    /// Seal A's answer to the offer C delivered under `punch_id`.
    pub fn seal_answer(
        &self,
        relay_node_id: &str,
        punch_id: &PunchId,
        answer: &GatewayAnswer,
    ) -> Result<SealedCandidates, ProtoError> {
        answer.check()?;
        seal_body(
            &self.keys.a2p,
            &answer_aad(relay_node_id, punch_id),
            answer,
            SEALED_ANSWER_PLAINTEXT_LEN,
        )
    }

    /// Whether `tag` is P's tag for punch `seq` of `offer_id`, compared in
    /// constant time.
    pub fn verify_punch_tag(&self, offer_id: &OfferId, seq: u16, tag: &PunchTag) -> bool {
        punch_tag(&self.keys.punch, offer_id, seq).is_ok_and(|expected| expected == *tag)
    }
}

fn punch_tag(key: &[u8; KEY_LEN], offer_id: &OfferId, seq: u16) -> Result<PunchTag, ProtoError> {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key.as_slice()).map_err(|_| ProtoError::KeyLen {
            expected: KEY_LEN,
            got: key.len(),
        })?;
    mac.update(offer_id.as_bytes());
    mac.update(&seq.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let mut tag = [0u8; PUNCH_TAG_LEN];
    for (out, byte) in tag.iter_mut().zip(digest.iter()) {
        *out = *byte;
    }
    Ok(PunchTag::from_bytes(tag))
}

fn static_dh(
    own_secret: &[u8; KEY_LEN],
    peer_public: &[u8; KEY_LEN],
) -> Result<Zeroizing<[u8; KEY_LEN]>, ProtoError> {
    let mut dh = DefaultResolver
        .resolve_dh(&DHChoice::Curve25519)
        .ok_or_else(|| ProtoError::Handshake("X25519 resolver unavailable".to_string()))?;
    dh.set(own_secret);
    let mut shared = Zeroizing::new([0u8; KEY_LEN]);
    let result = dh.dh(peer_public, shared.as_mut_slice());
    // snow's X25519 keeps its own copy of the secret and does not wipe it on drop.
    dh.set(&[0u8; KEY_LEN]);
    result?;
    if bool::from(shared.ct_eq(&[0u8; KEY_LEN])) {
        return Err(ProtoError::WeakPeerKey);
    }
    Ok(shared)
}

fn expand(hkdf: &Hkdf<Sha256>, info: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>, ProtoError> {
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    hkdf.expand(info, key.as_mut_slice())
        .map_err(|_| ProtoError::Hkdf)?;
    Ok(key)
}

fn offer_aad(relay_node_id: &str) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(OFFER_AAD_DOMAIN.len() + size_of::<u64>() + relay_node_id.len());
    aad.extend_from_slice(OFFER_AAD_DOMAIN);
    push_node_id(&mut aad, relay_node_id);
    aad
}

fn answer_aad(relay_node_id: &str, punch_id: &PunchId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        ANSWER_AAD_DOMAIN.len()
            + size_of::<u64>()
            + relay_node_id.len()
            + punch_id.as_bytes().len(),
    );
    aad.extend_from_slice(ANSWER_AAD_DOMAIN);
    push_node_id(&mut aad, relay_node_id);
    aad.extend_from_slice(punch_id.as_bytes());
    aad
}

fn push_node_id(aad: &mut Vec<u8>, relay_node_id: &str) {
    let len = u64::try_from(relay_node_id.len()).unwrap_or(u64::MAX);
    aad.extend_from_slice(&len.to_be_bytes());
    aad.extend_from_slice(relay_node_id.as_bytes());
}

fn seal_body<T: Serialize>(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    body: &T,
    plaintext_len: usize,
) -> Result<SealedCandidates, ProtoError> {
    let mut plaintext = Zeroizing::new(vec![0u8; plaintext_len]);
    encode_padded(body, &mut plaintext)?;
    seal_plaintext(key, aad, &plaintext)
}

fn seal_plaintext(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<SealedCandidates, ProtoError> {
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher(key)
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| ProtoError::Aead { stage: "seal" })?;
    Ok(SealedCandidates {
        n: BASE64.encode(nonce),
        enc: BASE64.encode(ciphertext),
    })
}

fn open_body<T: DeserializeOwned>(
    key: &[u8; KEY_LEN],
    aad: &[u8],
    sealed: &SealedCandidates,
    plaintext_len: usize,
) -> Result<T, ProtoError> {
    if !sealed.is_within_bounds() {
        return Err(CandidateRejection::Oversized.into());
    }
    let nonce = BASE64
        .decode(&sealed.n)
        .map_err(|_| CandidateRejection::Encoding)?;
    if nonce.len() != SEALED_NONCE_LEN {
        return Err(CandidateRejection::NonceLength {
            expected: SEALED_NONCE_LEN,
            got: nonce.len(),
        }
        .into());
    }
    let ciphertext = BASE64
        .decode(&sealed.enc)
        .map_err(|_| CandidateRejection::Encoding)?;
    let expected = plaintext_len + TAG_LEN;
    if ciphertext.len() != expected {
        return Err(CandidateRejection::CiphertextLength {
            expected,
            got: ciphertext.len(),
        }
        .into());
    }
    let plaintext = Zeroizing::new(
        cipher(key)
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad,
                },
            )
            .map_err(|_| ProtoError::Aead { stage: "open" })?,
    );
    decode_padded(&plaintext)
}

fn cipher(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(Key::from_slice(key))
}

/// Writes `u16be(len) ‖ msgpack(body)` into the zeroed `plaintext`, whose
/// remaining bytes stay as the padding. The body is sized first and then
/// encoded straight into `plaintext`, so no growable copy of it is left behind.
fn encode_padded<T: Serialize>(body: &T, plaintext: &mut [u8]) -> Result<(), ProtoError> {
    let mut sizer = ByteCount::default();
    rmp_serde::encode::write_named(&mut sizer, body)
        .map_err(|e| ProtoError::Codec(e.to_string()))?;
    let len = sizer.0;
    let capacity = plaintext.len().saturating_sub(BODY_LEN_PREFIX_LEN);
    let too_large = CandidateRejection::BodyTooLarge { len, capacity };
    let (prefix, rest) = plaintext
        .split_first_chunk_mut::<BODY_LEN_PREFIX_LEN>()
        .ok_or(too_large)?;
    let mut slot = rest.get_mut(..len).ok_or(too_large)?;
    rmp_serde::encode::write_named(&mut slot, body)
        .map_err(|e| ProtoError::Codec(e.to_string()))?;
    *prefix = u16::try_from(len).map_err(|_| too_large)?.to_be_bytes();
    Ok(())
}

fn decode_padded<T: DeserializeOwned>(plaintext: &[u8]) -> Result<T, ProtoError> {
    let (prefix, rest) = plaintext
        .split_first_chunk::<BODY_LEN_PREFIX_LEN>()
        .ok_or(CandidateRejection::BodyLength)?;
    let (body, padding) = rest
        .split_at_checked(usize::from(u16::from_be_bytes(*prefix)))
        .ok_or(CandidateRejection::BodyLength)?;
    if padding.iter().any(|byte| *byte != 0) {
        return Err(CandidateRejection::Padding.into());
    }
    decode_exact(body)
}

/// Decodes one msgpack value that must end exactly where `body` ends. Decoding
/// borrows from `body`, so the token is copied only into its `Zeroizing` home.
fn decode_exact<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtoError> {
    let mut decoder = rmp_serde::Deserializer::from_read_ref(body);
    let value = T::deserialize(&mut decoder).map_err(|e| ProtoError::Codec(e.to_string()))?;
    // rmp_serde exposes no remaining-input accessor; reading one more value
    // fails on its very first byte exactly when no byte is left.
    match IgnoredAny::deserialize(&mut decoder) {
        Err(rmp_serde::decode::Error::InvalidMarkerRead(e))
            if e.kind() == io::ErrorKind::UnexpectedEof =>
        {
            Ok(value)
        }
        _ => Err(CandidateRejection::BodyLength.into()),
    }
}

#[derive(Default)]
struct ByteCount(usize);

impl io::Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use remote_host_protocol::relay::PUNCH_ID_LEN;

    use super::*;
    use crate::noise::StaticKeypair;

    const NODE: &str = "node-1";
    const WIDEST_OFFER_BODY_LEN: usize = 402;
    const WIDEST_ANSWER_BODY_LEN: usize = 702;
    const SEALED_OFFER_WIRE_LEN: usize = 736;
    const SEALED_ANSWER_WIRE_LEN: usize = 1420;

    struct Pair {
        device: DeviceSealer,
        gateway: GatewaySealer,
    }

    fn pair() -> Pair {
        let device = StaticKeypair::generate().unwrap();
        let gateway = StaticKeypair::generate().unwrap();
        Pair {
            device: DeviceSealer::derive(&Zeroizing::new(device.secret()), &gateway.public())
                .unwrap(),
            gateway: GatewaySealer::derive(&Zeroizing::new(gateway.secret()), &device.public())
                .unwrap(),
        }
    }

    fn punch_id(byte: u8) -> PunchId {
        PunchId::from_bytes([byte; PUNCH_ID_LEN])
    }

    fn v4(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port)
    }

    fn v6(ip: [u8; 16], port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port)
    }

    /// Every entry IPv6 with every byte and the port at their longest msgpack
    /// encodings.
    fn widest(count: usize) -> Vec<SocketAddr> {
        vec![v6([0xff; 16], u16::MAX); count]
    }

    fn answer(offer_id: OfferId, udp: Vec<SocketAddr>, tcp: Vec<SocketAddr>) -> GatewayAnswer {
        GatewayAnswer {
            v: CANDIDATE_SET_VERSION,
            issued_at_ms: 1_700_000_000_000,
            offer_id,
            token: DirectToken::generate(),
            quic_cert_sha256: CertHash::of_certificate(b"certificate"),
            udp,
            tcp,
        }
    }

    fn widest_offer() -> DeviceOffer {
        DeviceOffer {
            v: CANDIDATE_SET_VERSION,
            issued_at_ms: u64::MAX,
            offer_id: OfferId([0xff; OFFER_ID_LEN]),
            udp: widest(MAX_UDP_HOST_CANDIDATES),
        }
    }

    fn widest_answer() -> GatewayAnswer {
        GatewayAnswer {
            issued_at_ms: u64::MAX,
            quic_cert_sha256: CertHash([0xff; CERT_HASH_LEN]),
            ..answer(
                OfferId([0xff; OFFER_ID_LEN]),
                widest(MAX_UDP_HOST_CANDIDATES),
                widest(MAX_TCP_CANDIDATES),
            )
        }
    }

    fn flip_byte(field: &str, index: usize) -> String {
        let mut bytes = BASE64.decode(field).unwrap();
        bytes[index] ^= 0x01;
        BASE64.encode(bytes)
    }

    fn wire_len(sealed: &SealedCandidates) -> usize {
        sealed.n.len() + sealed.enc.len()
    }

    fn rejection(result: Result<impl fmt::Debug, ProtoError>) -> CandidateRejection {
        match result {
            Err(ProtoError::Candidates(rejection)) => rejection,
            other => panic!("expected a candidate rejection, got {other:?}"),
        }
    }

    #[test]
    fn paired_sides_derive_the_same_directional_keys() {
        let Pair { device, gateway } = pair();
        assert_eq!(*device.keys.p2a, *gateway.keys.p2a);
        assert_eq!(*device.keys.a2p, *gateway.keys.a2p);
        assert_eq!(*device.keys.punch, *gateway.keys.punch);
        assert_ne!(*device.keys.p2a, *device.keys.a2p);
        assert_ne!(*device.keys.p2a, *device.keys.punch);
        assert_ne!(*device.keys.a2p, *device.keys.punch);
    }

    #[test]
    fn an_offer_round_trips_from_device_to_gateway() {
        let Pair { device, gateway } = pair();
        let offer = DeviceOffer::new(
            1_700_000_000_000,
            vec![v4([192, 168, 1, 20], 42124), v6([0xfd; 16], 42123)],
        );
        let sealed = device.seal_offer(NODE, &offer).unwrap();
        assert!(sealed.is_within_bounds());
        assert_eq!(gateway.open_offer(NODE, &sealed).unwrap(), offer);
    }

    #[test]
    fn an_answer_round_trips_from_gateway_to_device() {
        let Pair { device, gateway } = pair();
        let offer_id = OfferId::generate();
        let sent = answer(
            offer_id,
            vec![v4([10, 0, 1, 2], 5000), v6([0x20; 16], 5001)],
            vec![v4([203, 0, 113, 7], 443)],
        );
        let sealed = gateway.seal_answer(NODE, &punch_id(1), &sent).unwrap();
        let opened = device
            .open_answer(NODE, &punch_id(1), &offer_id, &sealed)
            .unwrap();
        assert_eq!(opened, sent);
    }

    #[test]
    fn a_set_sealed_under_the_other_direction_never_opens() {
        let Pair { device, gateway } = pair();
        let offer = DeviceOffer::new(1, vec![v4([10, 0, 0, 2], 1)]);
        let offer_under_a2p = seal_body(
            &gateway.keys.a2p,
            &offer_aad(NODE),
            &offer,
            SEALED_OFFER_PLAINTEXT_LEN,
        )
        .unwrap();
        assert!(matches!(
            gateway.open_offer(NODE, &offer_under_a2p),
            Err(ProtoError::Aead { .. })
        ));

        let sent = answer(offer.offer_id, vec![], vec![]);
        let answer_under_p2a = seal_body(
            &device.keys.p2a,
            &answer_aad(NODE, &punch_id(1)),
            &sent,
            SEALED_ANSWER_PLAINTEXT_LEN,
        )
        .unwrap();
        assert!(matches!(
            device.open_answer(NODE, &punch_id(1), &offer.offer_id, &answer_under_p2a),
            Err(ProtoError::Aead { .. })
        ));
    }

    #[test]
    fn a_set_reflected_to_its_sender_is_refused() {
        let Pair { device, gateway } = pair();
        let offer = DeviceOffer::new(1, vec![v4([10, 0, 0, 2], 1)]);
        let sealed_offer = device.seal_offer(NODE, &offer).unwrap();
        assert!(matches!(
            rejection(device.open_answer(NODE, &punch_id(1), &offer.offer_id, &sealed_offer)),
            CandidateRejection::CiphertextLength { .. }
        ));

        let sent = answer(offer.offer_id, vec![], vec![]);
        let sealed_answer = gateway.seal_answer(NODE, &punch_id(1), &sent).unwrap();
        assert!(matches!(
            rejection(gateway.open_offer(NODE, &sealed_answer)),
            CandidateRejection::CiphertextLength { .. }
        ));
    }

    #[test]
    fn the_associated_data_binds_the_node_id_and_the_punch_id() {
        let Pair { device, gateway } = pair();
        let offer = DeviceOffer::new(1, vec![]);
        let sealed_offer = device.seal_offer(NODE, &offer).unwrap();
        assert!(matches!(
            gateway.open_offer("node-2", &sealed_offer),
            Err(ProtoError::Aead { .. })
        ));

        let sent = answer(offer.offer_id, vec![], vec![]);
        let sealed_answer = gateway.seal_answer(NODE, &punch_id(1), &sent).unwrap();
        assert!(matches!(
            device.open_answer("node-2", &punch_id(1), &offer.offer_id, &sealed_answer),
            Err(ProtoError::Aead { .. })
        ));
        assert!(matches!(
            device.open_answer(NODE, &punch_id(2), &offer.offer_id, &sealed_answer),
            Err(ProtoError::Aead { .. })
        ));
    }

    #[test]
    fn low_order_peer_keys_are_rejected() {
        let own = StaticKeypair::generate().unwrap();
        let own_secret = Zeroizing::new(own.secret());
        let mut one = [0u8; KEY_LEN];
        one[0] = 1;
        let order_eight: [u8; KEY_LEN] =
            hex::decode("e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800")
                .unwrap()
                .try_into()
                .unwrap();
        for peer in [[0u8; KEY_LEN], one, order_eight] {
            assert!(matches!(
                DeviceSealer::derive(&own_secret, &peer),
                Err(ProtoError::WeakPeerKey)
            ));
            assert!(matches!(
                GatewaySealer::derive(&own_secret, &peer),
                Err(ProtoError::WeakPeerKey)
            ));
        }
    }

    #[test]
    fn tampering_fails_to_open() {
        let Pair { device, gateway } = pair();
        let offer = DeviceOffer::new(1, vec![v4([10, 0, 0, 2], 1)]);
        let sealed = device.seal_offer(NODE, &offer).unwrap();

        for index in [
            0,
            SEALED_OFFER_PLAINTEXT_LEN / 2,
            SEALED_OFFER_PLAINTEXT_LEN + 1,
        ] {
            let tampered = SealedCandidates {
                n: sealed.n.clone(),
                enc: flip_byte(&sealed.enc, index),
            };
            assert!(matches!(
                gateway.open_offer(NODE, &tampered),
                Err(ProtoError::Aead { .. })
            ));
        }
        let tampered_nonce = SealedCandidates {
            n: flip_byte(&sealed.n, 0),
            enc: sealed.enc.clone(),
        };
        assert!(matches!(
            gateway.open_offer(NODE, &tampered_nonce),
            Err(ProtoError::Aead { .. })
        ));
    }

    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let Pair { device, .. } = pair();
        let offer = DeviceOffer::new(1, vec![]);
        let first = device.seal_offer(NODE, &offer).unwrap();
        let second = device.seal_offer(NODE, &offer).unwrap();
        assert_ne!(first.n, second.n);
        assert_ne!(first.enc, second.enc);
    }

    #[test]
    fn sealed_lengths_hide_the_candidate_count_and_tcp() {
        let Pair { device, gateway } = pair();
        let empty = device
            .seal_offer(NODE, &DeviceOffer::new(0, vec![]))
            .unwrap();
        let full = device.seal_offer(NODE, &widest_offer()).unwrap();
        assert_eq!(empty.n.len(), full.n.len());
        assert_eq!(empty.enc.len(), full.enc.len());

        let offer_id = OfferId::generate();
        let without_tcp = gateway
            .seal_answer(
                NODE,
                &punch_id(1),
                &answer(offer_id, widest(MAX_UDP_HOST_CANDIDATES), vec![]),
            )
            .unwrap();
        let with_tcp = gateway
            .seal_answer(NODE, &punch_id(1), &widest_answer())
            .unwrap();
        let bare = gateway
            .seal_answer(NODE, &punch_id(1), &answer(offer_id, vec![], vec![]))
            .unwrap();
        assert_eq!(without_tcp.enc.len(), with_tcp.enc.len());
        assert_eq!(bare.enc.len(), with_tcp.enc.len());
        assert_eq!(bare.n.len(), with_tcp.n.len());
    }

    #[test]
    fn the_pinned_plaintext_lengths_hold_the_full_cap_bodies() {
        let offer_len = rmp_serde::to_vec_named(&widest_offer()).unwrap().len();
        assert_eq!(offer_len, WIDEST_OFFER_BODY_LEN);
        assert!(BODY_LEN_PREFIX_LEN + offer_len <= SEALED_OFFER_PLAINTEXT_LEN);
        let answer_len = rmp_serde::to_vec_named(&widest_answer()).unwrap().len();
        assert_eq!(answer_len, WIDEST_ANSWER_BODY_LEN);
        assert!(BODY_LEN_PREFIX_LEN + answer_len <= SEALED_ANSWER_PLAINTEXT_LEN);

        let Pair { device, gateway } = pair();
        let offer = widest_offer();
        let sealed_offer = device.seal_offer(NODE, &offer).unwrap();
        assert!(sealed_offer.is_within_bounds());
        assert_eq!(gateway.open_offer(NODE, &sealed_offer).unwrap(), offer);

        let sent = widest_answer();
        let sealed_answer = gateway.seal_answer(NODE, &punch_id(1), &sent).unwrap();
        assert!(sealed_answer.is_within_bounds());
        assert_eq!(
            device
                .open_answer(NODE, &punch_id(1), &sent.offer_id, &sealed_answer)
                .unwrap(),
            sent
        );

        assert_eq!(
            sealed_wire_len(SEALED_OFFER_PLAINTEXT_LEN),
            Some(SEALED_OFFER_WIRE_LEN)
        );
        assert_eq!(wire_len(&sealed_offer), SEALED_OFFER_WIRE_LEN);
        assert_eq!(
            sealed_wire_len(SEALED_ANSWER_PLAINTEXT_LEN),
            Some(SEALED_ANSWER_WIRE_LEN)
        );
        assert_eq!(wire_len(&sealed_answer), SEALED_ANSWER_WIRE_LEN);
    }

    #[test]
    fn fixed_size_byte_fields_encode_at_a_fixed_length() {
        let low = DeviceOffer {
            offer_id: OfferId([0; OFFER_ID_LEN]),
            ..widest_offer()
        };
        assert_eq!(
            rmp_serde::to_vec_named(&low).unwrap().len(),
            rmp_serde::to_vec_named(&widest_offer()).unwrap().len()
        );
        let low = GatewayAnswer {
            quic_cert_sha256: CertHash([0; CERT_HASH_LEN]),
            ..widest_answer()
        };
        assert_eq!(
            rmp_serde::to_vec_named(&low).unwrap().len(),
            rmp_serde::to_vec_named(&widest_answer()).unwrap().len()
        );
    }

    #[test]
    fn a_body_that_does_not_fit_fails_to_seal() {
        let offer = widest_offer();
        let mut plaintext = [0u8; SEALED_OFFER_PLAINTEXT_LEN / 2];
        let error = encode_padded(&offer, &mut plaintext).unwrap_err();
        assert!(matches!(
            error,
            ProtoError::Candidates(CandidateRejection::BodyTooLarge { .. })
        ));
        assert!(plaintext.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn over_cap_sets_fail_to_seal_and_to_open() {
        let Pair { device, gateway } = pair();
        let over = DeviceOffer {
            udp: widest(MAX_UDP_HOST_CANDIDATES + 1),
            ..DeviceOffer::new(1, vec![])
        };
        assert!(matches!(
            rejection(device.seal_offer(NODE, &over)),
            CandidateRejection::Count { list: UDP_LIST, .. }
        ));
        let smuggled = seal_body(
            &device.keys.p2a,
            &offer_aad(NODE),
            &over,
            SEALED_OFFER_PLAINTEXT_LEN,
        )
        .unwrap();
        assert!(matches!(
            rejection(gateway.open_offer(NODE, &smuggled)),
            CandidateRejection::Count { list: UDP_LIST, .. }
        ));

        let over = GatewayAnswer {
            tcp: widest(MAX_TCP_CANDIDATES + 1),
            ..answer(over.offer_id, vec![], vec![])
        };
        assert!(matches!(
            rejection(gateway.seal_answer(NODE, &punch_id(1), &over)),
            CandidateRejection::Count { list: TCP_LIST, .. }
        ));
        let smuggled = seal_body(
            &gateway.keys.a2p,
            &answer_aad(NODE, &punch_id(1)),
            &over,
            SEALED_ANSWER_PLAINTEXT_LEN,
        )
        .unwrap();
        assert!(matches!(
            rejection(device.open_answer(NODE, &punch_id(1), &over.offer_id, &smuggled)),
            CandidateRejection::Count { list: TCP_LIST, .. }
        ));
    }

    #[test]
    fn a_wrong_length_plaintext_fails_to_open() {
        let Pair { device, gateway } = pair();
        let offer = DeviceOffer::new(1, vec![]);
        for len in [
            SEALED_OFFER_PLAINTEXT_LEN - 1,
            SEALED_OFFER_PLAINTEXT_LEN + 1,
        ] {
            let sealed = seal_body(&device.keys.p2a, &offer_aad(NODE), &offer, len).unwrap();
            assert!(matches!(
                rejection(gateway.open_offer(NODE, &sealed)),
                CandidateRejection::CiphertextLength { .. }
            ));
        }
    }

    #[test]
    fn a_bad_frame_inside_a_genuine_seal_fails_to_open() {
        let Pair { device, gateway } = pair();
        let mut plaintext = vec![0u8; SEALED_OFFER_PLAINTEXT_LEN];
        encode_padded(&DeviceOffer::new(1, vec![]), &mut plaintext).unwrap();

        let mut dirty = plaintext.clone();
        if let Some(last) = dirty.last_mut() {
            *last = 1;
        }
        let sealed = seal_plaintext(&device.keys.p2a, &offer_aad(NODE), &dirty).unwrap();
        assert_eq!(
            rejection(gateway.open_offer(NODE, &sealed)),
            CandidateRejection::Padding
        );

        let mut overlong = plaintext;
        overlong[..BODY_LEN_PREFIX_LEN].copy_from_slice(&u16::MAX.to_be_bytes());
        let sealed = seal_plaintext(&device.keys.p2a, &offer_aad(NODE), &overlong).unwrap();
        assert_eq!(
            rejection(gateway.open_offer(NODE, &sealed)),
            CandidateRejection::BodyLength
        );

        let mut framed = vec![0u8; SEALED_OFFER_PLAINTEXT_LEN];
        framed[..BODY_LEN_PREFIX_LEN].copy_from_slice(&4u16.to_be_bytes());
        framed[BODY_LEN_PREFIX_LEN..BODY_LEN_PREFIX_LEN + 4].fill(0xc1);
        let sealed = seal_plaintext(&device.keys.p2a, &offer_aad(NODE), &framed).unwrap();
        assert!(matches!(
            gateway.open_offer(NODE, &sealed),
            Err(ProtoError::Codec(_))
        ));
    }

    #[test]
    fn bytes_after_the_body_inside_the_declared_length_fail_to_open() {
        let Pair { device, gateway } = pair();
        let mut plaintext = vec![0u8; SEALED_OFFER_PLAINTEXT_LEN];
        encode_padded(&DeviceOffer::new(1, vec![]), &mut plaintext).unwrap();
        let body_len = usize::from(u16::from_be_bytes([plaintext[0], plaintext[1]]));
        let body_end = BODY_LEN_PREFIX_LEN + body_len;

        let trailers: [&[u8]; 4] = [&[0x00], &[0x01, 0x02, 0x03], &[0xc1], &[0xd9]];
        for trailer in trailers {
            let mut framed = plaintext.clone();
            framed[body_end..body_end + trailer.len()].copy_from_slice(trailer);
            let declared = u16::try_from(body_len + trailer.len()).unwrap();
            framed[..BODY_LEN_PREFIX_LEN].copy_from_slice(&declared.to_be_bytes());
            let sealed = seal_plaintext(&device.keys.p2a, &offer_aad(NODE), &framed).unwrap();
            assert_eq!(
                rejection(gateway.open_offer(NODE, &sealed)),
                CandidateRejection::BodyLength,
                "trailer {trailer:02x?}"
            );
        }
    }

    #[test]
    fn a_version_mismatch_fails_to_open() {
        let Pair { device, gateway } = pair();
        let future = DeviceOffer {
            v: CANDIDATE_SET_VERSION + 1,
            ..DeviceOffer::new(1, vec![])
        };
        assert_eq!(
            rejection(device.seal_offer(NODE, &future)),
            CandidateRejection::Version {
                got: CANDIDATE_SET_VERSION + 1
            }
        );
        let sealed = seal_body(
            &device.keys.p2a,
            &offer_aad(NODE),
            &future,
            SEALED_OFFER_PLAINTEXT_LEN,
        )
        .unwrap();
        assert_eq!(
            rejection(gateway.open_offer(NODE, &sealed)),
            CandidateRejection::Version {
                got: CANDIDATE_SET_VERSION + 1
            }
        );
    }

    #[test]
    fn an_answer_that_does_not_echo_the_offer_id_is_refused() {
        let Pair { device, gateway } = pair();
        let sent = answer(OfferId::generate(), vec![], vec![]);
        let sealed = gateway.seal_answer(NODE, &punch_id(1), &sent).unwrap();
        assert_eq!(
            rejection(device.open_answer(NODE, &punch_id(1), &OfferId::generate(), &sealed)),
            CandidateRejection::OfferIdMismatch
        );
    }

    #[test]
    fn malformed_sets_are_refused_before_the_aead() {
        let Pair { device, gateway } = pair();
        let sealed = device
            .seal_offer(NODE, &DeviceOffer::new(1, vec![]))
            .unwrap();

        let oversized = SealedCandidates {
            n: sealed.n.clone(),
            enc: "A".repeat(MAX_SEALED_CANDIDATES_BYTES),
        };
        assert_eq!(
            rejection(gateway.open_offer(NODE, &oversized)),
            CandidateRejection::Oversized
        );
        let not_base64 = SealedCandidates {
            n: "!".repeat(sealed.n.len()),
            enc: sealed.enc.clone(),
        };
        assert_eq!(
            rejection(gateway.open_offer(NODE, &not_base64)),
            CandidateRejection::Encoding
        );
        let short_nonce = SealedCandidates {
            n: BASE64.encode([0u8; SEALED_NONCE_LEN - 1]),
            enc: sealed.enc.clone(),
        };
        assert!(matches!(
            rejection(gateway.open_offer(NODE, &short_nonce)),
            CandidateRejection::NonceLength { .. }
        ));
    }

    #[test]
    fn punch_tags_verify_and_reject() {
        let Pair { device, gateway } = pair();
        let offer_id = OfferId::generate();
        let tag = device.punch_tag(&offer_id, 7).unwrap();
        assert!(gateway.verify_punch_tag(&offer_id, 7, &tag));
        assert_eq!(device.punch_tag(&offer_id, 7).unwrap(), tag);

        assert!(!gateway.verify_punch_tag(&offer_id, 8, &tag));
        assert!(!gateway.verify_punch_tag(&OfferId::generate(), 7, &tag));
        let mut flipped = *tag.as_bytes();
        flipped[PUNCH_TAG_LEN - 1] ^= 0x01;
        assert!(!gateway.verify_punch_tag(&offer_id, 7, &PunchTag::from_bytes(flipped)));
        assert_ne!(device.punch_tag(&offer_id, 8).unwrap(), tag);

        let stranger = pair();
        assert!(!stranger.gateway.verify_punch_tag(&offer_id, 7, &tag));
    }

    #[test]
    fn cert_hash_pins_one_certificate() {
        let pinned = CertHash::of_certificate(b"certificate a");
        assert_eq!(pinned, CertHash::of_certificate(b"certificate a"));
        assert_ne!(pinned, CertHash::of_certificate(b"certificate b"));
        assert_eq!(pinned, CertHash(Sha256::digest(b"certificate a").into()));
    }

    #[test]
    fn debug_never_prints_key_material() {
        let Pair { device, gateway } = pair();
        assert_eq!(format!("{device:?}"), "DeviceSealer { keys: <redacted> }");
        assert_eq!(format!("{gateway:?}"), "GatewaySealer { keys: <redacted> }");
    }
}
