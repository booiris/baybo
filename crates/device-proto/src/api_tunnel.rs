//! HTTP-shaped request/response messages carried inside an E2E Noise content
//! leg.
//!
//! The relay still only sees opaque WebSocket binary frames. The app and
//! gateway encode these messages with MessagePack, then seal them through the
//! same Noise chunking used by chat frames. `method + path + headers` mirror the
//! direct gateway API surface; `Body` messages carry raw binary chunks so the
//! protocol does not need a bespoke blob frame family.

use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// Largest body payload placed in a single tunnel body message.
///
/// The MessagePack envelope plus Noise tag must stay below the relay's frame
/// limit and snow's transport-message ceiling. 60 KiB leaves room for request
/// ids, offsets, headers, and encoding overhead.
pub const MAX_TUNNEL_CHUNK: usize = 60 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelHeader {
    pub name: String,
    pub value: String,
}

impl TunnelHeader {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelRequest {
    Head {
        request_id: u64,
        method: String,
        path: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<TunnelHeader>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_len: Option<u64>,
    },
    Body {
        request_id: u64,
        offset: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        last: bool,
    },
    Cancel {
        request_id: u64,
        reason: String,
    },
}

/// The gateway declaring, on every response head, that it will hold this leg
/// open for another request — and for how long it will wait.
///
/// This is the ONLY capability signal for leg reuse, and it is a typed field
/// rather than a response header on purpose: the gateway forwards every header
/// its router produced into `TunnelResponse::Head::headers` verbatim, so a
/// marker header could be spoofed by any `/v1` route or reverse proxy, letting
/// an OLD gateway advertise reuse it does not implement. The client would then
/// write its next request head into a session that has already returned.
///
/// Absent = a one-shot leg (the original semantics). Both skew directions are
/// safe: an old gateway never sends the field and `#[serde(default)]` reads it
/// as `None`; an old client decodes the msgpack map without
/// `deny_unknown_fields` and silently ignores it, then closes the leg as it
/// always did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelReuse {
    /// How long the gateway will wait for the next request head before closing
    /// the leg. The client must close first — see the client's idle margin.
    pub idle_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelResponse {
    Head {
        request_id: u64,
        status: u16,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<TunnelHeader>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_len: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reuse: Option<TunnelReuse>,
    },
    Body {
        request_id: u64,
        offset: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        last: bool,
    },
    Error {
        request_id: u64,
        status: u16,
        reason: String,
    },
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    rmp_serde::to_vec_named(msg).map_err(|e| ProtoError::Codec(e.to_string()))
}

pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, ProtoError> {
    rmp_serde::from_slice(bytes).map_err(|e| ProtoError::Codec(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_head_round_trips() {
        let msg = TunnelRequest::Head {
            request_id: 7,
            method: "GET".into(),
            path: "/v1/blobs/sha256:abc.token".into(),
            headers: vec![TunnelHeader::new("range", "bytes=10-")],
            body_len: None,
        };

        let bytes = encode(&msg).unwrap();
        assert_eq!(decode::<TunnelRequest>(&bytes).unwrap(), msg);
    }

    #[test]
    fn body_payload_is_msgpack_bin() {
        let msg = TunnelResponse::Body {
            request_id: 1,
            offset: 0,
            data: vec![1, 2, 3, 4],
            last: true,
        };

        let bytes = encode(&msg).unwrap();
        assert!(bytes.windows(5).any(|w| w == [0xc4, 0x04, 1, 2, 3]));
        assert_eq!(decode::<TunnelResponse>(&bytes).unwrap(), msg);
    }

    fn head(reuse: Option<TunnelReuse>) -> TunnelResponse {
        TunnelResponse::Head {
            request_id: 1,
            status: 200,
            headers: vec![TunnelHeader::new("content-type", "application/json")],
            body_len: Some(2),
            reuse,
        }
    }

    /// The `TunnelResponse::Head` that shipped before `reuse` existed — encoder
    /// and decoder both, so the two skew directions can be exercised against the
    /// real msgpack bytes rather than a hand-written buffer.
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum LegacyResponse {
        Head {
            request_id: u64,
            status: u16,
            #[serde(default, skip_serializing_if = "Vec::is_empty")]
            headers: Vec<TunnelHeader>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            body_len: Option<u64>,
        },
    }

    /// NEW app, OLD gateway. The old gateway's head has no `reuse` key at all, so
    /// the new client must read it as "one-shot" and close the leg exactly as it
    /// does today — no speculation, no wasted round trip.
    #[test]
    fn a_head_without_reuse_decodes_as_one_shot() {
        let old_gateway = encode(&LegacyResponse::Head {
            request_id: 1,
            status: 200,
            headers: vec![],
            body_len: Some(2),
        })
        .expect("encode");

        match decode::<TunnelResponse>(&old_gateway).expect("decode") {
            TunnelResponse::Head { reuse, status, .. } => {
                assert_eq!(reuse, None, "an absent field must not mean 'reusable'");
                assert_eq!(status, 200);
            }
            other => panic!("expected a head, got {other:?}"),
        }
    }

    /// OLD app, NEW gateway. The extra `reuse` key must be IGNORED, not rejected:
    /// msgpack named maps with no `deny_unknown_fields` are the whole basis of the
    /// skew story. An old client keeps reading its status/body and closes the leg,
    /// which the gateway's loop sees as EOF.
    #[test]
    fn an_unknown_field_on_a_head_still_decodes() {
        let new_gateway = encode(&head(Some(TunnelReuse { idle_ms: 60_000 }))).expect("encode");

        match decode::<LegacyResponse>(&new_gateway).expect("a shipped client must not choke") {
            LegacyResponse::Head {
                status, body_len, ..
            } => {
                assert_eq!(status, 200);
                assert_eq!(body_len, Some(2));
            }
        }
    }

    /// `reuse` is skipped when absent, so a one-shot head stays byte-identical to
    /// what the gateway has always sent — an old client sees no new bytes at all.
    #[test]
    fn a_one_shot_head_carries_no_reuse_bytes() {
        let without = encode(&head(None)).expect("encode");
        let with = encode(&head(Some(TunnelReuse { idle_ms: 60_000 }))).expect("encode");

        assert!(!contains(&without, b"reuse"));
        assert!(contains(&with, b"reuse"));
        assert!(with.len() > without.len());
        assert_eq!(
            decode::<TunnelResponse>(&with).expect("decode"),
            head(Some(TunnelReuse { idle_ms: 60_000 }))
        );
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
