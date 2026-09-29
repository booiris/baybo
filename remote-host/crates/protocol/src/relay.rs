//! Relay wire surface: the pairing + content rendezvous, the gateway control
//! channel, and the direct-carrier signalling that rides on them (the offer
//! route, the control-channel offer and report, the UDP rendezvous datagrams,
//! the address policy, and the `DirectOpen` preface).

mod address;
mod ids;
mod probe;
mod rendezvous;

use std::time::Duration;

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer, Serialize};

pub use address::{AddressClass, AddressPolicy, SourceKey};
pub use ids::{
    DIRECT_TOKEN_LEN, DirectToken, PUNCH_ID_LEN, PunchId, RENDEZVOUS_TICKET_LEN, RendezvousTicket,
};
pub use probe::{
    PROBE_DATAGRAM_MAGIC, PUNCH_TAG_LEN, ProbeDatagram, PunchRole, PunchTag, QUIC_FIRST_BYTE_MASK,
    REGISTER_DATAGRAM_LEN, is_probe_datagram,
};

// Keep the original public path source-compatible after the header became a
// relay + push contract at the protocol root.
#[doc(hidden)]
pub use crate::REMOTE_API_KEY_HEADER;

/// Header on a phone's [`CONTENT_JOIN`] dial declaring the leg's traffic class
/// ([`LegClass`]). The phone owns the join, so the phone authors the class; the
/// relay copies it (it never authors it) onto the matching gateway host leg so
/// both spliced halves meter the same way. Absent or unparseable ⇒ [`LegClass::Chat`].
pub const RELAY_LEG_CLASS_HEADER: &str = "x-relay-leg-class";

/// The traffic class of a content leg, declared by the phone on its
/// [`CONTENT_JOIN`] dial ([`RELAY_LEG_CLASS_HEADER`]) and relayed to the gateway
/// in [`ControlSignal::OpenDataLeg`]. `Chat` runs the live frame loop, `Api` runs
/// an interactive API tunnel, and `Blob` runs the same tunnel on background
/// bandwidth for bulk transfer. Defaults to `Chat` for backward-compatible decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegClass {
    /// Interactive chat traffic; the full `wire::Frame` loop.
    #[default]
    Chat,
    /// Small API tunnel traffic; interactive bandwidth.
    Api,
    /// Bulk blob traffic; API tunnel over background bandwidth.
    Blob,
}

impl LegClass {
    /// Parse the [`RELAY_LEG_CLASS_HEADER`] value; absent or unknown values are
    /// `Chat` — the relay must never fail a join on a bad class hint, and an
    /// unknown value is the safe interactive default.
    pub fn from_header_value(value: &str) -> Self {
        match value {
            "api" => LegClass::Api,
            "blob" => LegClass::Blob,
            _ => LegClass::Chat,
        }
    }

    /// The header/wire string for this class.
    pub fn as_str(self) -> &'static str {
        match self {
            LegClass::Chat => "chat",
            LegClass::Api => "api",
            LegClass::Blob => "blob",
        }
    }
}

/// Route templates (axum-style `{param}` tokens). The server registers these
/// directly; clients build a concrete URL via the helpers below.
///
/// The pairing routes key on the **public `rendezvous_id`** (a UUID) — the only
/// pairing value C ever sees. It routes nothing secret: the QR secret (the Noise
/// PSK) never reaches C, so C cannot complete the handshake with either side.
pub const PAIR_HOST: &str = "/pair/host/{rendezvous_id}";
pub const PAIR_JOIN: &str = "/pair/join/{rendezvous_id}";
pub const CONTROL: &str = "/control";
pub const CONTENT_JOIN: &str = "/content/join/{relay_node_id}";
pub const CONTENT_HOST: &str = "/content/host/{relay_key}";
/// P's offer: `POST` with a [`DirectOfferRequest`] body, answered with a
/// [`DirectOfferResponse`].
pub const DIRECT_OFFER: &str = "/direct/{relay_node_id}";

/// The version of direct-carrier signalling this build speaks. A capability of
/// any other version reads as "not direct-capable".
pub const DIRECT_PROTOCOL_VERSION: u16 = 1;
/// Longest `relay_node_id` accepted wherever a node id becomes a map key.
pub const MAX_RELAY_NODE_ID_BYTES: usize = 128;
/// Decode cap on the UDP host candidates of either side's sealed set.
pub const MAX_UDP_HOST_CANDIDATES: usize = 8;
/// Decode cap on the TCP candidates of a sealed answer.
pub const MAX_TCP_CANDIDATES: usize = 4;
/// Fixed plaintext length of every sealed offer, so its ciphertext length
/// reveals nothing about the candidates.
pub const SEALED_OFFER_PLAINTEXT_LEN: usize = 512;
/// Fixed plaintext length of every sealed answer.
pub const SEALED_ANSWER_PLAINTEXT_LEN: usize = 1024;
/// Cap on the combined length of a [`SealedCandidates`]' `n` and `enc`.
pub const MAX_SEALED_CANDIDATES_BYTES: usize = 2048;
/// Cap on a `POST /direct` request body, and on the response body P reads.
pub const MAX_DIRECT_OFFER_BODY_BYTES: usize = 4 * 1024;
/// How long a punch lives: C's registry entry, and A's allowed-IP set.
pub const PUNCH_TTL: Duration = Duration::from_secs(20);
/// Live punches per gateway node, enforced by C and, as defence in depth, by A.
pub const MAX_INFLIGHT_PUNCHES_PER_NODE: usize = 2;
/// First delay after a UDP socket receive error at A, P or C; it doubles per
/// consecutive error up to [`SOCKET_RECV_BACKOFF_MAX`].
pub const SOCKET_RECV_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Longest delay after consecutive UDP socket receive errors.
pub const SOCKET_RECV_BACKOFF_MAX: Duration = Duration::from_secs(4);

/// The delay before the next receive after `consecutive_errors` errors in a
/// row (the first error is `1`): `250ms · 2ⁿ⁻¹`, at most 4 s.
pub fn socket_recv_backoff(consecutive_errors: u32) -> Duration {
    let doublings = consecutive_errors.saturating_sub(1);
    SOCKET_RECV_BACKOFF_INITIAL
        .saturating_mul(2u32.saturating_pow(doublings))
        .min(SOCKET_RECV_BACKOFF_MAX)
}

/// A gateway's direct-carrier capability, sent in its [`ControlHello`] while
/// its carrier runtime is active. It names the families the runtime serves,
/// not what is bound at that instant, so it is the same in every hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectCapability {
    pub version: u16,
    /// A serves an IPv4 UDP family and registers at C's rendezvous on demand.
    #[serde(default)]
    pub udp: bool,
}

/// First binary-JSON frame on [`CONTROL`] (gateway → C): the gateway names
/// itself by `relay_node_id`. Admission rides the `x-remote-api-key` header on the
/// dial (the shared pre-layer), like every other route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHello {
    pub relay_node_id: String,
    /// Decoded leniently: a malformed capability reads as `None` and is logged,
    /// because C closes control on an unparseable hello.
    #[serde(
        default,
        deserialize_with = "lenient_capability",
        skip_serializing_if = "Option::is_none"
    )]
    pub direct: Option<DirectCapability>,
}

impl ControlHello {
    /// The capability C acts on: present and of [`DIRECT_PROTOCOL_VERSION`].
    pub fn supported_direct(&self) -> Option<&DirectCapability> {
        self.direct
            .as_ref()
            .filter(|capability| capability.version == DIRECT_PROTOCOL_VERSION)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LenientCapability {
    Valid(DirectCapability),
    Null,
    Malformed(IgnoredAny),
}

fn lenient_capability<'de, D>(deserializer: D) -> Result<Option<DirectCapability>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match LenientCapability::deserialize(deserializer)? {
        LenientCapability::Valid(capability) => Some(capability),
        LenientCapability::Null => None,
        LenientCapability::Malformed(_) => {
            tracing::warn!("control hello: malformed direct capability; treating as absent");
            None
        }
    })
}

/// A sealed candidate set: base64 of a 24-byte XChaCha20 nonce (`n`) and of
/// ciphertext‖tag (`enc`), mirroring `NotifyRequest`'s `n`/`enc`. C forwards it
/// opaque and bounds only its size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedCandidates {
    pub n: String,
    pub enc: String,
}

impl SealedCandidates {
    /// Whether `n` and `enc` together fit [`MAX_SEALED_CANDIDATES_BYTES`].
    pub fn is_within_bounds(&self) -> bool {
        self.n.len().saturating_add(self.enc.len()) <= MAX_SEALED_CANDIDATES_BYTES
    }
}

/// C's UDP rendezvous, as handed to one role of one punch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpRendezvous {
    /// `host:port` as normalised by [`UdpRendezvous::normalize_address`];
    /// resolved by its recipient with [`UdpRendezvous::resolve_public_v4`].
    pub address: String,
    /// The recipient's role ticket only.
    pub ticket: RendezvousTicket,
}

/// A → C over [`CONTROL`], only ever in reply to a [`ControlSignal::DirectOffer`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlReport {
    DirectAnswer {
        punch_id: PunchId,
        answer: SealedCandidates,
    },
    DirectDeclined {
        punch_id: PunchId,
    },
}

impl ControlReport {
    pub fn punch_id(&self) -> &PunchId {
        match self {
            Self::DirectAnswer { punch_id, .. } | Self::DirectDeclined { punch_id } => punch_id,
        }
    }
}

/// Body of `POST /direct/{relay_node_id}`: P's sealed offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectOfferRequest {
    pub offer: SealedCandidates,
}

/// `200` body of `POST /direct/{relay_node_id}`: A's sealed answer, and the
/// device-role rendezvous when C runs one and A registers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectOfferResponse {
    pub punch_id: PunchId,
    pub answer: SealedCandidates,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous: Option<UdpRendezvous>,
}

/// First framed record on every direct stream or TCP connection. The token is
/// an availability gate; the Noise IK handshake that follows authenticates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectOpen {
    pub token: DirectToken,
    pub class: LegClass,
}

/// C → gateway signal, pushed over [`CONTROL`] as binary-JSON
/// (`{"t":"open_data_leg","relay_key":"…"}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlSignal {
    /// Tell the gateway to open a content data leg under `relay_key`. `class`
    /// (defaulted for a pre-class sender) tells the gateway whether to run the
    /// chat frame loop ([`LegClass::Chat`]) or the API tunnel
    /// ([`LegClass::Api`] / [`LegClass::Blob`]) over the leg, and to meter it
    /// accordingly.
    OpenDataLeg {
        relay_key: String,
        #[serde(default)]
        class: LegClass,
    },
    /// Forward P's sealed offer. Sent only to a control connection whose hello
    /// carried a supported [`DirectCapability`]. An absent `register` means C
    /// runs no UDP rendezvous for this punch: host candidates only.
    DirectOffer {
        punch_id: PunchId,
        offer: SealedCandidates,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        register: Option<UdpRendezvous>,
    },
}

/// `{base}/pair/host/{rendezvous_id}` — the gateway's pairing host leg.
pub fn pair_host_url(base: &str, rendezvous_id: &str) -> String {
    crate::join(base, &PAIR_HOST.replace("{rendezvous_id}", rendezvous_id))
}
/// `{base}/pair/join/{rendezvous_id}` — the app's pairing join leg.
pub fn pair_join_url(base: &str, rendezvous_id: &str) -> String {
    crate::join(base, &PAIR_JOIN.replace("{rendezvous_id}", rendezvous_id))
}
/// `{base}/control` — the gateway's persistent control connection.
pub fn control_url(base: &str) -> String {
    crate::join(base, CONTROL)
}
/// `{base}/content/join/{relay_node_id}` — the app's content join leg.
pub fn content_join_url(base: &str, relay_node_id: &str) -> String {
    crate::join(
        base,
        &CONTENT_JOIN.replace("{relay_node_id}", relay_node_id),
    )
}
/// `{base}/content/host/{relay_key}` — the gateway's content data leg.
pub fn content_host_url(base: &str, relay_key: &str) -> String {
    crate::join(base, &CONTENT_HOST.replace("{relay_key}", relay_key))
}
/// `{base}/direct/{relay_node_id}` — P's offer. The route is plain HTTP(S) on
/// the relay's origin, so a `wss://` / `ws://` base maps to `https://` /
/// `http://`.
pub fn direct_offer_url(base: &str, relay_node_id: &str) -> String {
    let origin = if let Some(rest) = base.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = base.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        base.to_owned()
    };
    crate::join(
        &origin,
        &DIRECT_OFFER.replace("{relay_node_id}", relay_node_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed(tag: &str) -> SealedCandidates {
        SealedCandidates {
            n: format!("nonce-{tag}"),
            enc: format!("ciphertext-{tag}"),
        }
    }

    fn punch_id() -> PunchId {
        PunchId::from_bytes([0x11; PUNCH_ID_LEN])
    }

    fn rendezvous() -> UdpRendezvous {
        UdpRendezvous {
            address: "rdv.example:7777".into(),
            ticket: RendezvousTicket::from_bytes([0x22; RENDEZVOUS_TICKET_LEN]),
        }
    }

    fn round_trip<T>(value: &T) -> serde_json::Value
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let bytes = serde_json::to_vec(value).unwrap();
        assert_eq!(&serde_json::from_slice::<T>(&bytes).unwrap(), value);
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn old_control_hello_decodes_without_a_capability() {
        let hello: ControlHello = serde_json::from_str(r#"{"relay_node_id":"node-1"}"#).unwrap();
        assert_eq!(hello.relay_node_id, "node-1");
        assert_eq!(hello.direct, None);
        assert_eq!(hello.supported_direct(), None);
    }

    #[test]
    fn hello_without_a_capability_serializes_without_the_field() {
        let hello = ControlHello {
            relay_node_id: "node-1".into(),
            direct: None,
        };
        let json = round_trip(&hello);
        assert_eq!(json, serde_json::json!({ "relay_node_id": "node-1" }));
    }

    #[test]
    fn hello_with_a_capability_round_trips() {
        let hello = ControlHello {
            relay_node_id: "node-1".into(),
            direct: Some(DirectCapability {
                version: DIRECT_PROTOCOL_VERSION,
                udp: true,
            }),
        };
        let json = round_trip(&hello);
        assert_eq!(json["direct"]["version"], DIRECT_PROTOCOL_VERSION);
        assert_eq!(json["direct"]["udp"], true);
        assert_eq!(hello.supported_direct(), hello.direct.as_ref());
    }

    #[test]
    fn malformed_capability_decodes_as_absent_and_keeps_the_hello() {
        for direct in [
            r#""yes""#,
            "42",
            "[]",
            r#"{"udp":true}"#,
            r#"{"version":"1"}"#,
            r#"{"version":70000}"#,
            r#"{"version":1,"udp":"yes"}"#,
            "null",
        ] {
            let json = format!(r#"{{"relay_node_id":"node-1","direct":{direct}}}"#);
            let hello: ControlHello = serde_json::from_str(&json).unwrap();
            assert_eq!(hello.relay_node_id, "node-1", "{direct}");
            assert_eq!(hello.direct, None, "{direct}");
        }
    }

    #[test]
    fn capability_defaults_udp_and_ignores_unknown_fields() {
        let hello: ControlHello =
            serde_json::from_str(r#"{"relay_node_id":"n","direct":{"version":1,"future":[1,2]}}"#)
                .unwrap();
        assert_eq!(
            hello.direct,
            Some(DirectCapability {
                version: 1,
                udp: false
            })
        );
    }

    #[test]
    fn unknown_capability_version_is_not_supported() {
        let hello: ControlHello =
            serde_json::from_str(r#"{"relay_node_id":"n","direct":{"version":2,"udp":true}}"#)
                .unwrap();
        assert!(hello.direct.is_some());
        assert_eq!(hello.supported_direct(), None);
    }

    #[test]
    fn direct_offer_signal_round_trips_with_and_without_register() {
        let with_register = ControlSignal::DirectOffer {
            punch_id: punch_id(),
            offer: sealed("offer"),
            register: Some(rendezvous()),
        };
        let json = round_trip(&with_register);
        assert_eq!(json["t"], "direct_offer");
        assert_eq!(json["punch_id"], "11".repeat(PUNCH_ID_LEN));
        assert_eq!(json["offer"]["n"], "nonce-offer");
        assert_eq!(json["register"]["address"], "rdv.example:7777");
        assert_eq!(
            json["register"]["ticket"],
            "22".repeat(RENDEZVOUS_TICKET_LEN)
        );

        let without_register = ControlSignal::DirectOffer {
            punch_id: punch_id(),
            offer: sealed("offer"),
            register: None,
        };
        let json = round_trip(&without_register);
        assert!(json.get("register").is_none());
    }

    #[test]
    fn open_data_leg_still_decodes_without_a_class() {
        let signal: ControlSignal =
            serde_json::from_str(r#"{"t":"open_data_leg","relay_key":"k"}"#).unwrap();
        assert_eq!(
            signal,
            ControlSignal::OpenDataLeg {
                relay_key: "k".into(),
                class: LegClass::Chat,
            }
        );
    }

    #[test]
    fn control_reports_round_trip() {
        let answer = ControlReport::DirectAnswer {
            punch_id: punch_id(),
            answer: sealed("answer"),
        };
        let json = round_trip(&answer);
        assert_eq!(json["t"], "direct_answer");
        assert_eq!(answer.punch_id(), &punch_id());

        let declined = ControlReport::DirectDeclined {
            punch_id: punch_id(),
        };
        let json = round_trip(&declined);
        assert_eq!(json["t"], "direct_declined");
        assert_eq!(declined.punch_id(), &punch_id());
    }

    #[test]
    fn offer_request_and_response_round_trip() {
        round_trip(&DirectOfferRequest {
            offer: sealed("offer"),
        });
        let with_rendezvous = DirectOfferResponse {
            punch_id: punch_id(),
            answer: sealed("answer"),
            rendezvous: Some(rendezvous()),
        };
        round_trip(&with_rendezvous);
        let without = DirectOfferResponse {
            rendezvous: None,
            ..with_rendezvous
        };
        assert!(round_trip(&without).get("rendezvous").is_none());
    }

    #[test]
    fn direct_open_round_trips_and_redacts_its_token() {
        let open = DirectOpen {
            token: DirectToken::generate(),
            class: LegClass::Api,
        };
        let json = round_trip(&open);
        assert_eq!(json["class"], "api");
        assert!(!format!("{open:?}").contains(json["token"].as_str().unwrap()));
    }

    #[test]
    fn sealed_candidates_are_bounded_by_their_combined_length() {
        let half = MAX_SEALED_CANDIDATES_BYTES / 2;
        let at_cap = SealedCandidates {
            n: "n".repeat(half),
            enc: "e".repeat(MAX_SEALED_CANDIDATES_BYTES - half),
        };
        assert!(at_cap.is_within_bounds());
        let over = SealedCandidates {
            enc: format!("{}e", at_cap.enc),
            ..at_cap
        };
        assert!(!over.is_within_bounds());
    }

    #[test]
    fn direct_offer_url_uses_the_relay_http_origin() {
        assert_eq!(
            direct_offer_url("wss://relay.example/", "node-1"),
            "https://relay.example/direct/node-1"
        );
        assert_eq!(
            direct_offer_url("ws://127.0.0.1:8080", "node-1"),
            "http://127.0.0.1:8080/direct/node-1"
        );
        assert_eq!(
            direct_offer_url("https://relay.example", "node-1"),
            "https://relay.example/direct/node-1"
        );
    }

    #[test]
    fn socket_recv_backoff_doubles_to_its_cap() {
        let steps: Vec<Duration> = (1..=7).map(socket_recv_backoff).collect();
        let ms = Duration::from_millis;
        assert_eq!(
            steps,
            [
                ms(250),
                ms(500),
                ms(1000),
                ms(2000),
                ms(4000),
                ms(4000),
                ms(4000)
            ]
        );
        assert_eq!(socket_recv_backoff(0), SOCKET_RECV_BACKOFF_INITIAL);
        assert_eq!(socket_recv_backoff(u32::MAX), SOCKET_RECV_BACKOFF_MAX);
    }
}
