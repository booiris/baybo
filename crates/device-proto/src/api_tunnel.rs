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
}
