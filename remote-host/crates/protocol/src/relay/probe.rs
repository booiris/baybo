//! Probe datagrams: the UDP rendezvous exchange with C and the punches between
//! A and P. They share each side's UDP socket with QUIC, so their first byte
//! never looks like QUIC.

use std::net::{Ipv4Addr, SocketAddrV4};

use subtle::ConstantTimeEq;

use super::ids::{PUNCH_ID_LEN, PunchId, RENDEZVOUS_TICKET_LEN, RendezvousTicket};
use crate::error::ProbeDecodeError;

/// First byte of every probe datagram.
pub const PROBE_DATAGRAM_MAGIC: u8 = 0x0b;
/// First-byte bits that mark QUIC: the long-header form bit and the fixed bit.
/// A datagram with either set belongs to QUIC; a probe datagram has both clear.
pub const QUIC_FIRST_BYTE_MASK: u8 = 0x80 | 0x40;
/// A `Register` is zero-padded to this length, which exceeds every reply C
/// sends, so C never emits more bytes than it receives.
pub const REGISTER_DATAGRAM_LEN: usize = 64;
/// Length of a punch authentication tag.
pub const PUNCH_TAG_LEN: usize = 16;

const HEADER_LEN: usize = 2;
const KIND_REGISTER: u8 = 1;
const KIND_REGISTERED: u8 = 2;
const KIND_PEER: u8 = 3;
const KIND_PUNCH: u8 = 4;
const ROLE_GATEWAY: u8 = 1;
const ROLE_DEVICE: u8 = 2;
const ROLE_LEN: usize = 1;
const IPV4_LEN: usize = 4;
const PORT_LEN: usize = 2;
const SEQ_LEN: usize = 2;
const REGISTER_UNPADDED_LEN: usize = HEADER_LEN + PUNCH_ID_LEN + ROLE_LEN + RENDEZVOUS_TICKET_LEN;
const REGISTERED_LEN: usize = HEADER_LEN + PUNCH_ID_LEN;
const PEER_LEN: usize = HEADER_LEN + PUNCH_ID_LEN + IPV4_LEN + PORT_LEN;
const PUNCH_LEN: usize = HEADER_LEN + SEQ_LEN + PUNCH_TAG_LEN;

const _: () = assert!(PROBE_DATAGRAM_MAGIC & QUIC_FIRST_BYTE_MASK == 0);
const _: () = assert!(REGISTER_UNPADDED_LEN <= REGISTER_DATAGRAM_LEN);
const _: () = assert!(REGISTERED_LEN < REGISTER_DATAGRAM_LEN);
const _: () = assert!(PEER_LEN < REGISTER_DATAGRAM_LEN);

/// Whether a received datagram is a probe datagram rather than QUIC.
pub fn is_probe_datagram(datagram: &[u8]) -> bool {
    datagram
        .first()
        .is_some_and(|first| first & QUIC_FIRST_BYTE_MASK == 0)
}

/// Which side of a direct attempt an endpoint is: the role a `Register` speaks
/// for, each with its own ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PunchRole {
    Gateway,
    Device,
}

impl PunchRole {
    fn to_byte(self) -> u8 {
        match self {
            Self::Gateway => ROLE_GATEWAY,
            Self::Device => ROLE_DEVICE,
        }
    }

    fn from_byte(role: u8) -> Result<Self, ProbeDecodeError> {
        match role {
            ROLE_GATEWAY => Ok(Self::Gateway),
            ROLE_DEVICE => Ok(Self::Device),
            role => Err(ProbeDecodeError::Role { role }),
        }
    }
}

/// `HMAC-SHA256(k_punch, offer_id ‖ u16be(seq))[..16]` on P's punches, random
/// on A's. Compared in constant time.
#[derive(Debug, Clone, Copy)]
pub struct PunchTag([u8; PUNCH_TAG_LEN]);

impl PunchTag {
    pub fn from_bytes(bytes: [u8; PUNCH_TAG_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; PUNCH_TAG_LEN] {
        &self.0
    }
}

impl PartialEq for PunchTag {
    fn eq(&self, other: &Self) -> bool {
        self.0[..].ct_eq(&other.0[..]).into()
    }
}

impl Eq for PunchTag {}

/// One probe datagram.
///
/// ```text
/// byte 0   PROBE_DATAGRAM_MAGIC
/// byte 1   kind
///   1 Register     punch_id[16] role[1] ticket[16]   zero-padded to REGISTER_DATAGRAM_LEN
///   2 Registered   punch_id[16]
///   3 Peer         punch_id[16] ipv4[4] port[2]
///   4 Punch        seq[2] tag[16]
/// ```
///
/// Integers are big-endian; `role` is 1 for the gateway and 2 for the device.
/// Decoding requires the exact length of each kind and zero padding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeDatagram {
    /// A or P → C: register this socket for one role of one punch.
    Register {
        punch_id: PunchId,
        role: PunchRole,
        ticket: RendezvousTicket,
    },
    /// C → sender: the ticket is valid; the other role has not registered yet.
    Registered { punch_id: PunchId },
    /// C → sender: the other role's mapping, as C observed it.
    Peer {
        punch_id: PunchId,
        srflx: SocketAddrV4,
    },
    /// A ↔ P: opens the sender's NAT or firewall toward the receiver.
    Punch { seq: u16, tag: PunchTag },
}

impl ProbeDatagram {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Register {
                punch_id,
                role,
                ticket,
            } => {
                let mut out = header(KIND_REGISTER, REGISTER_DATAGRAM_LEN);
                out.extend_from_slice(punch_id.as_bytes());
                out.push(role.to_byte());
                out.extend_from_slice(ticket.as_bytes());
                out.resize(REGISTER_DATAGRAM_LEN, 0);
                out
            }
            Self::Registered { punch_id } => {
                let mut out = header(KIND_REGISTERED, REGISTERED_LEN);
                out.extend_from_slice(punch_id.as_bytes());
                out
            }
            Self::Peer { punch_id, srflx } => {
                let mut out = header(KIND_PEER, PEER_LEN);
                out.extend_from_slice(punch_id.as_bytes());
                out.extend_from_slice(&srflx.ip().octets());
                out.extend_from_slice(&srflx.port().to_be_bytes());
                out
            }
            Self::Punch { seq, tag } => {
                let mut out = header(KIND_PUNCH, PUNCH_LEN);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(tag.as_bytes());
                out
            }
        }
    }

    pub fn decode(datagram: &[u8]) -> Result<Self, ProbeDecodeError> {
        let [PROBE_DATAGRAM_MAGIC, kind, body @ ..] = datagram else {
            return Err(ProbeDecodeError::NotProbe);
        };
        let kind = *kind;
        let expected = match kind {
            KIND_REGISTER => REGISTER_DATAGRAM_LEN,
            KIND_REGISTERED => REGISTERED_LEN,
            KIND_PEER => PEER_LEN,
            KIND_PUNCH => PUNCH_LEN,
            kind => return Err(ProbeDecodeError::UnknownKind { kind }),
        };
        let length_error = || ProbeDecodeError::Length {
            kind,
            len: datagram.len(),
            expected,
        };
        if datagram.len() != expected {
            return Err(length_error());
        }
        match kind {
            KIND_REGISTER => {
                let (punch_id, rest) = body
                    .split_first_chunk::<PUNCH_ID_LEN>()
                    .ok_or_else(length_error)?;
                let (&[role], rest) = rest
                    .split_first_chunk::<ROLE_LEN>()
                    .ok_or_else(length_error)?;
                let (ticket, padding) = rest
                    .split_first_chunk::<RENDEZVOUS_TICKET_LEN>()
                    .ok_or_else(length_error)?;
                if padding.iter().any(|byte| *byte != 0) {
                    return Err(ProbeDecodeError::Padding);
                }
                Ok(Self::Register {
                    punch_id: PunchId::from_bytes(*punch_id),
                    role: PunchRole::from_byte(role)?,
                    ticket: RendezvousTicket::from_bytes(*ticket),
                })
            }
            KIND_REGISTERED => {
                let punch_id = body
                    .first_chunk::<PUNCH_ID_LEN>()
                    .ok_or_else(length_error)?;
                Ok(Self::Registered {
                    punch_id: PunchId::from_bytes(*punch_id),
                })
            }
            KIND_PEER => {
                let (punch_id, rest) = body
                    .split_first_chunk::<PUNCH_ID_LEN>()
                    .ok_or_else(length_error)?;
                let (ip, rest) = rest
                    .split_first_chunk::<IPV4_LEN>()
                    .ok_or_else(length_error)?;
                let port = rest.first_chunk::<PORT_LEN>().ok_or_else(length_error)?;
                Ok(Self::Peer {
                    punch_id: PunchId::from_bytes(*punch_id),
                    srflx: SocketAddrV4::new(Ipv4Addr::from(*ip), u16::from_be_bytes(*port)),
                })
            }
            _ => {
                let (seq, rest) = body
                    .split_first_chunk::<SEQ_LEN>()
                    .ok_or_else(length_error)?;
                let tag = rest
                    .first_chunk::<PUNCH_TAG_LEN>()
                    .ok_or_else(length_error)?;
                Ok(Self::Punch {
                    seq: u16::from_be_bytes(*seq),
                    tag: PunchTag::from_bytes(*tag),
                })
            }
        }
    }
}

fn header(kind: u8, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    out.push(PROBE_DATAGRAM_MAGIC);
    out.push(kind);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROPERTY_CASES: usize = 4096;
    const MAX_FUZZ_LEN: usize = REGISTER_DATAGRAM_LEN + 8;

    fn punch_id() -> PunchId {
        PunchId::from_bytes(std::array::from_fn(|i| i as u8))
    }

    fn samples() -> Vec<ProbeDatagram> {
        vec![
            ProbeDatagram::Register {
                punch_id: punch_id(),
                role: PunchRole::Gateway,
                ticket: RendezvousTicket::from_bytes([0xee; RENDEZVOUS_TICKET_LEN]),
            },
            ProbeDatagram::Register {
                punch_id: punch_id(),
                role: PunchRole::Device,
                ticket: RendezvousTicket::from_bytes([0xdd; RENDEZVOUS_TICKET_LEN]),
            },
            ProbeDatagram::Registered {
                punch_id: punch_id(),
            },
            ProbeDatagram::Peer {
                punch_id: punch_id(),
                srflx: "203.0.113.7:40000".parse().unwrap(),
            },
            ProbeDatagram::Punch {
                seq: 0x0102,
                tag: PunchTag::from_bytes([0xaa; PUNCH_TAG_LEN]),
            },
        ]
    }

    fn random_datagram() -> ProbeDatagram {
        let punch_id = PunchId::from_bytes(rand::random());
        match rand::random_range(0..4u8) {
            0 => ProbeDatagram::Register {
                punch_id,
                role: if rand::random() {
                    PunchRole::Gateway
                } else {
                    PunchRole::Device
                },
                ticket: RendezvousTicket::from_bytes(rand::random()),
            },
            1 => ProbeDatagram::Registered { punch_id },
            2 => ProbeDatagram::Peer {
                punch_id,
                srflx: SocketAddrV4::new(Ipv4Addr::from(rand::random::<u32>()), rand::random()),
            },
            _ => ProbeDatagram::Punch {
                seq: rand::random(),
                tag: PunchTag::from_bytes(rand::random()),
            },
        }
    }

    #[test]
    fn every_kind_round_trips_at_its_fixed_length() {
        let lengths: Vec<usize> = samples()
            .iter()
            .map(|datagram| {
                let bytes = datagram.encode();
                assert_eq!(&ProbeDatagram::decode(&bytes).unwrap(), datagram);
                bytes.len()
            })
            .collect();
        assert_eq!(
            lengths,
            [
                REGISTER_DATAGRAM_LEN,
                REGISTER_DATAGRAM_LEN,
                REGISTERED_LEN,
                PEER_LEN,
                PUNCH_LEN
            ]
        );
    }

    #[test]
    fn peer_layout_is_big_endian_ipv4_then_port() {
        let bytes = ProbeDatagram::Peer {
            punch_id: punch_id(),
            srflx: "1.2.3.4:258".parse().unwrap(),
        }
        .encode();
        assert_eq!(&bytes[..2], &[PROBE_DATAGRAM_MAGIC, KIND_PEER]);
        assert_eq!(&bytes[HEADER_LEN + PUNCH_ID_LEN..], &[1, 2, 3, 4, 1, 2]);
    }

    #[test]
    fn register_padding_role_and_lengths_are_checked() {
        let register = samples().remove(0).encode();

        let mut dirty = register.clone();
        if let Some(last) = dirty.last_mut() {
            *last = 1;
        }
        assert_eq!(
            ProbeDatagram::decode(&dirty),
            Err(ProbeDecodeError::Padding)
        );

        let mut bad_role = register.clone();
        bad_role[HEADER_LEN + PUNCH_ID_LEN] = 0;
        assert_eq!(
            ProbeDatagram::decode(&bad_role),
            Err(ProbeDecodeError::Role { role: 0 })
        );

        assert_eq!(
            ProbeDatagram::decode(&register[..REGISTER_UNPADDED_LEN]),
            Err(ProbeDecodeError::Length {
                kind: KIND_REGISTER,
                len: REGISTER_UNPADDED_LEN,
                expected: REGISTER_DATAGRAM_LEN,
            })
        );
        let mut long = register;
        long.push(0);
        assert!(matches!(
            ProbeDatagram::decode(&long),
            Err(ProbeDecodeError::Length { .. })
        ));
    }

    #[test]
    fn foreign_datagrams_are_rejected() {
        assert_eq!(ProbeDatagram::decode(&[]), Err(ProbeDecodeError::NotProbe));
        assert_eq!(
            ProbeDatagram::decode(&[PROBE_DATAGRAM_MAGIC]),
            Err(ProbeDecodeError::NotProbe)
        );
        assert_eq!(
            ProbeDatagram::decode(&[0xc3, KIND_PUNCH]),
            Err(ProbeDecodeError::NotProbe)
        );
        assert_eq!(
            ProbeDatagram::decode(&[PROBE_DATAGRAM_MAGIC, 9, 0, 0]),
            Err(ProbeDecodeError::UnknownKind { kind: 9 })
        );
    }

    #[test]
    fn demux_predicate_separates_probe_from_quic() {
        for datagram in samples() {
            assert!(is_probe_datagram(&datagram.encode()));
        }
        assert!(!is_probe_datagram(&[0xc0, 0, 0]));
        assert!(!is_probe_datagram(&[0x40, 0, 0]));
        assert!(!is_probe_datagram(&[0x80, 0, 0]));
        assert!(!is_probe_datagram(&[]));
    }

    #[test]
    fn property_encoded_datagrams_look_unlike_quic_and_replies_never_exceed_register() {
        for _ in 0..PROPERTY_CASES {
            let datagram = random_datagram();
            let bytes = datagram.encode();
            assert_eq!(bytes[0] & QUIC_FIRST_BYTE_MASK, 0, "{datagram:?}");
            assert!(is_probe_datagram(&bytes));
            assert_eq!(ProbeDatagram::decode(&bytes).as_ref(), Ok(&datagram));
            match datagram {
                ProbeDatagram::Register { .. } => assert_eq!(bytes.len(), REGISTER_DATAGRAM_LEN),
                ProbeDatagram::Registered { .. } | ProbeDatagram::Peer { .. } => {
                    assert!(bytes.len() < REGISTER_DATAGRAM_LEN, "{datagram:?}")
                }
                ProbeDatagram::Punch { .. } => {}
            }
        }
    }

    #[test]
    fn property_decode_accepts_only_canonical_encodings() {
        for _ in 0..PROPERTY_CASES {
            let len = rand::random_range(0..=MAX_FUZZ_LEN);
            let mut bytes: Vec<u8> = (0..len).map(|_| rand::random()).collect();
            if let Some(first) = bytes.first_mut() {
                *first = PROBE_DATAGRAM_MAGIC;
            }
            if let Some(kind) = bytes.get_mut(1) {
                *kind = rand::random_range(KIND_REGISTER..=KIND_PUNCH);
            }
            if let Ok(datagram) = ProbeDatagram::decode(&bytes) {
                assert_eq!(datagram.encode(), bytes);
            }
        }
    }
}
