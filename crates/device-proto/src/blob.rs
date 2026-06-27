//! The blob-transfer sub-protocol carried over a dedicated relay **blob leg**.
//!
//! A blob leg is an ordinary Noise-IK content leg (same handshake + device auth +
//! [`crate::noise`] chunking as chat), but instead of the `wire::Frame` loop it
//! runs this small request/response protocol so a bulk transfer rides its own
//! TCP + Noise session — physically isolated from chat. One transfer per leg;
//! `request_id` only correlates a resume.
//!
//! Direction is fixed per message: [`BlobRequest`] is phone → gateway,
//! [`BlobResponse`] is gateway → phone. Each is msgpack-encoded via
//! [`encode`] / [`decode`] and sealed through the leg's Noise transport. Chunk
//! payloads are `serde_bytes` (`bin`, never a base64 / int-array blow-up) and
//! capped at [`MAX_BLOB_CHUNK`] so each seals to ~one Noise message.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::error::ProtoError;

/// Maximum plaintext bytes in one [`BlobResponse::Chunk`] / [`BlobRequest::Chunk`]
/// payload. 60 KiB leaves room for the msgpack envelope so the whole sealed
/// message still fits a single Noise message ([`crate::noise::MAX_PLAINTEXT_CHUNK`]
/// ≈ 65519) — no reassembly per chunk — and a single relay frame (< the relay's
/// 128 KiB cap), and is comfortably under the relay chat-headroom so a chunk in
/// flight never fully eats chat's reserve.
pub const MAX_BLOB_CHUNK: usize = 60 * 1024;

/// Phone → gateway messages on a blob leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum BlobRequest {
    /// Start (or resume from `offset`) a download of `blob_id`. The id carries
    /// its `read_token` — the phone only ever holds ids the gateway sent it over
    /// its authenticated chat leg, so possession is the read capability.
    Pull {
        request_id: u64,
        blob_id: String,
        offset: u64,
    },
    /// Offer an upload. The gateway replies [`BlobResponse::PushGrant`] (with the
    /// resume offset) or [`BlobResponse::Err`]. `sha256_hex` lets the gateway
    /// verify the finalized content address matches what the phone claimed.
    Push {
        request_id: u64,
        mime_type: String,
        size: u64,
        sha256_hex: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    /// An upload chunk (after a [`BlobResponse::PushGrant`]).
    Chunk {
        request_id: u64,
        offset: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        last: bool,
    },
    /// Cancel an in-flight transfer (user dismissed, decode error).
    Abort { request_id: u64, reason: String },
}

/// Gateway → phone messages on a blob leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum BlobResponse {
    /// A download chunk. `offset` is the byte position; `last` marks the final
    /// chunk (followed by [`Done`](Self::Done)).
    Chunk {
        request_id: u64,
        offset: u64,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        last: bool,
    },
    /// Download complete. `sha256_hex` is the content digest the phone verifies
    /// against the hex prefix of the `blob_id` it requested.
    Done {
        request_id: u64,
        total_size: u64,
        sha256_hex: String,
    },
    /// Upload authorized; the phone (re)starts sending [`BlobRequest::Chunk`] at
    /// `resume_offset`.
    PushGrant { request_id: u64, resume_offset: u64 },
    /// Upload finalized; `blob_id` is the content-addressed id the phone then
    /// references in its next message.
    PushDone { request_id: u64, blob_id: String },
    /// The request failed (unknown/unauthorized id, over quota, etc.). Terminal.
    Err { request_id: u64, reason: String },
}

/// Encode a blob message to msgpack (named fields, so the wire is stable across
/// field reordering).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtoError> {
    rmp_serde::to_vec_named(msg).map_err(|e| ProtoError::Codec(e.to_string()))
}

/// Decode a blob message from msgpack.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtoError> {
    rmp_serde::from_slice(bytes).map_err(|e| ProtoError::Codec(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_round_trips() {
        let msg = BlobRequest::Pull {
            request_id: 7,
            blob_id: "sha256:abcd.tok".into(),
            offset: 1024,
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode::<BlobRequest>(&bytes).unwrap(), msg);
    }

    #[test]
    fn chunk_payload_is_msgpack_bin_not_an_int_array() {
        // 200 bytes as serde_bytes encode to a compact `bin` header + raw bytes,
        // far smaller than a msgpack array of 200 integers would.
        let msg = BlobResponse::Chunk {
            request_id: 1,
            offset: 0,
            data: vec![0xABu8; 200],
            last: false,
        };
        let bytes = encode(&msg).unwrap();
        assert!(
            bytes.len() < 260,
            "serde_bytes keeps the chunk compact ({} bytes)",
            bytes.len()
        );
        assert_eq!(decode::<BlobResponse>(&bytes).unwrap(), msg);
    }

    #[test]
    fn done_and_err_round_trip() {
        let done = BlobResponse::Done {
            request_id: 3,
            total_size: 9000,
            sha256_hex: "ff".repeat(32),
        };
        assert_eq!(
            decode::<BlobResponse>(&encode(&done).unwrap()).unwrap(),
            done
        );
        let err = BlobResponse::Err {
            request_id: 3,
            reason: "unknown blob".into(),
        };
        assert_eq!(decode::<BlobResponse>(&encode(&err).unwrap()).unwrap(), err);
    }
}
