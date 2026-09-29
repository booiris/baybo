//! Identifiers and secrets of direct-carrier signalling: C's per-punch id and
//! role tickets, and A's per-runtime `DirectOpen` token. Each travels as
//! lowercase hex on the JSON wire and is validated on decode.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Raw length of a [`PunchId`].
pub const PUNCH_ID_LEN: usize = 16;
/// Raw length of a [`RendezvousTicket`].
pub const RENDEZVOUS_TICKET_LEN: usize = 16;
/// Raw length of a [`DirectToken`] before hex encoding.
pub const DIRECT_TOKEN_LEN: usize = 32;

const LOWER_HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const REDACTED: &str = "<redacted>";

/// Names one punch: 16 CSPRNG bytes minted by C per punch.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PunchId([u8; PUNCH_ID_LEN]);

impl PunchId {
    pub fn generate() -> Self {
        Self(rand::random())
    }

    pub fn from_bytes(bytes: [u8; PUNCH_ID_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; PUNCH_ID_LEN] {
        &self.0
    }

    /// Log form, the [`crate::key_tag`] of the hex id, so C and A log the
    /// same tag for one punch.
    pub fn tag(&self) -> String {
        crate::key_tag(&encode_lower_hex(&self.0))
    }
}

impl fmt::Debug for PunchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PunchId({})", self.tag())
    }
}

impl Serialize for PunchId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode_lower_hex(&self.0))
    }
}

impl<'de> Deserialize<'de> for PunchId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_str(LowerHexBytes::<PUNCH_ID_LEN>)
            .map(Self)
    }
}

/// C's per-punch, per-role registration credential: 16 CSPRNG bytes. Each
/// role receives only its own ticket. Redacted by `Debug`, compared in
/// constant time.
#[derive(Clone)]
pub struct RendezvousTicket([u8; RENDEZVOUS_TICKET_LEN]);

impl RendezvousTicket {
    pub fn generate() -> Self {
        Self(rand::random())
    }

    pub fn from_bytes(bytes: [u8; RENDEZVOUS_TICKET_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; RENDEZVOUS_TICKET_LEN] {
        &self.0
    }
}

impl PartialEq for RendezvousTicket {
    fn eq(&self, other: &Self) -> bool {
        self.0[..].ct_eq(&other.0[..]).into()
    }
}

impl Eq for RendezvousTicket {}

impl fmt::Debug for RendezvousTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RendezvousTicket({REDACTED})")
    }
}

impl Serialize for RendezvousTicket {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode_lower_hex(&self.0))
    }
}

impl<'de> Deserialize<'de> for RendezvousTicket {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_str(LowerHexBytes::<RENDEZVOUS_TICKET_LEN>)
            .map(Self)
    }
}

/// The `DirectOpen` gate: 32 CSPRNG bytes as 64 lowercase hex, minted by A per
/// binding runtime. Zeroized on drop, redacted by `Debug`, compared in
/// constant time.
#[derive(Clone)]
pub struct DirectToken(Zeroizing<String>);

impl DirectToken {
    pub fn generate() -> Self {
        let mut bytes = Zeroizing::new([0u8; DIRECT_TOKEN_LEN]);
        rand::fill(bytes.as_mut_slice());
        let mut hex = Zeroizing::new(String::with_capacity(DIRECT_TOKEN_LEN * 2));
        push_lower_hex(&mut hex, bytes.as_slice());
        Self(hex)
    }
}

impl PartialEq for DirectToken {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes().ct_eq(other.0.as_bytes()).into()
    }
}

impl Eq for DirectToken {}

impl fmt::Debug for DirectToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DirectToken({REDACTED})")
    }
}

impl Serialize for DirectToken {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DirectToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(DirectTokenVisitor)
    }
}

struct DirectTokenVisitor;

impl DirectTokenVisitor {
    fn check<E: de::Error>(&self, value: &str) -> Result<(), E> {
        if is_lower_hex(value, DIRECT_TOKEN_LEN) {
            Ok(())
        } else {
            Err(E::invalid_value(
                de::Unexpected::Other("malformed token"),
                self,
            ))
        }
    }
}

impl Visitor<'_> for DirectTokenVisitor {
    type Value = DirectToken;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} lowercase hex characters", DIRECT_TOKEN_LEN * 2)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<DirectToken, E> {
        self.check(value)?;
        let mut token = Zeroizing::new(String::with_capacity(value.len()));
        token.push_str(value);
        Ok(DirectToken(token))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<DirectToken, E> {
        let value = Zeroizing::new(value);
        self.check(&value)?;
        Ok(DirectToken(value))
    }
}

struct LowerHexBytes<const N: usize>;

impl<const N: usize> Visitor<'_> for LowerHexBytes<N> {
    type Value = [u8; N];

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} lowercase hex characters", N * 2)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<[u8; N], E> {
        decode_lower_hex(value)
            .ok_or_else(|| E::invalid_value(de::Unexpected::Other("malformed hex id"), &self))
    }
}

fn encode_lower_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    push_lower_hex(&mut out, bytes);
    out
}

fn push_lower_hex(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        out.push(char::from(LOWER_HEX_DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(LOWER_HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
}

fn is_lower_hex(value: &str, raw_len: usize) -> bool {
    value.len() == raw_len * 2 && value.bytes().all(|byte| lower_hex_nibble(byte).is_some())
}

fn decode_lower_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if value.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let [high, low] = pair else { return None };
        *slot = (lower_hex_nibble(*high)? << 4) | lower_hex_nibble(*low)?;
    }
    Some(out)
}

fn lower_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punch_id_travels_as_32_lowercase_hex_and_round_trips() {
        let id = PunchId::from_bytes([0xab; PUNCH_ID_LEN]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", "ab".repeat(PUNCH_ID_LEN)));
        assert_eq!(serde_json::from_str::<PunchId>(&json).unwrap(), id);
    }

    #[test]
    fn malformed_hex_ids_are_rejected() {
        for bad in [
            "AB".repeat(PUNCH_ID_LEN),
            "ab".repeat(PUNCH_ID_LEN - 1),
            "ab".repeat(PUNCH_ID_LEN + 1),
            "zz".repeat(PUNCH_ID_LEN),
            String::new(),
        ] {
            let json = format!("\"{bad}\"");
            assert!(serde_json::from_str::<PunchId>(&json).is_err(), "{bad}");
            assert!(
                serde_json::from_str::<RendezvousTicket>(&json).is_err(),
                "{bad}"
            );
        }
        assert!(serde_json::from_str::<PunchId>("42").is_err());
    }

    #[test]
    fn generated_ids_differ() {
        assert_ne!(PunchId::generate(), PunchId::generate());
        assert_ne!(RendezvousTicket::generate(), RendezvousTicket::generate());
        assert_ne!(DirectToken::generate(), DirectToken::generate());
    }

    #[test]
    fn punch_id_debug_shows_only_its_tag() {
        let id = PunchId::from_bytes([0x5a; PUNCH_ID_LEN]);
        let debug = format!("{id:?}");
        assert_eq!(debug, format!("PunchId({})", id.tag()));
        assert!(!debug.contains(&"5a".repeat(PUNCH_ID_LEN)));
    }

    #[test]
    fn ticket_is_redacted_and_compared_by_value() {
        let ticket = RendezvousTicket::from_bytes([7; RENDEZVOUS_TICKET_LEN]);
        assert_eq!(format!("{ticket:?}"), "RendezvousTicket(<redacted>)");
        assert_eq!(
            ticket,
            RendezvousTicket::from_bytes([7; RENDEZVOUS_TICKET_LEN])
        );
        assert_ne!(
            ticket,
            RendezvousTicket::from_bytes([8; RENDEZVOUS_TICKET_LEN])
        );
        let json = serde_json::to_string(&ticket).unwrap();
        assert_eq!(
            serde_json::from_str::<RendezvousTicket>(&json).unwrap(),
            ticket
        );
    }

    #[test]
    fn direct_token_is_64_lowercase_hex_redacted_and_round_trips() {
        let token = DirectToken::generate();
        assert_eq!(format!("{token:?}"), "DirectToken(<redacted>)");
        let json = serde_json::to_string(&token).unwrap();
        let hex = json.trim_matches('"');
        assert!(is_lower_hex(hex, DIRECT_TOKEN_LEN), "{hex}");
        assert_eq!(serde_json::from_str::<DirectToken>(&json).unwrap(), token);
    }

    #[test]
    fn malformed_direct_tokens_are_rejected() {
        for bad in [
            "A".repeat(DIRECT_TOKEN_LEN * 2),
            "a".repeat(DIRECT_TOKEN_LEN * 2 - 1),
            "a".repeat(DIRECT_TOKEN_LEN * 2 + 1),
            "g".repeat(DIRECT_TOKEN_LEN * 2),
        ] {
            let json = format!("\"{bad}\"");
            assert!(serde_json::from_str::<DirectToken>(&json).is_err(), "{bad}");
        }
    }

    #[test]
    fn direct_tokens_of_different_values_are_unequal() {
        let a: DirectToken = serde_json::from_str(&format!("\"{}\"", "a".repeat(64))).unwrap();
        let b: DirectToken = serde_json::from_str(&format!("\"{}\"", "b".repeat(64))).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, a.clone());
    }
}
