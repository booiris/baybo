//! Wire frames for the `/v1/tool-ws` endpoint. Encoded as msgpack
//! (matching the channel-ws codec) so a Rust-side change is mirrored
//! in the TS-side `msgpackr` decoder by ts-rs.
//!
//! Protocol shape:
//!
//! 1. Sidecar → Gateway: `Register { token, sidecar_id, protocol_version }`.
//! 2. Gateway → Sidecar: `RegisterAck { ok, reason? }`. On `ok=false`
//!    the connection is closed.
//! 3. Steady-state. Synchronous RPC:
//!    Gateway → Sidecar: `RpcRequest { id, method, params }`
//!    Sidecar → Gateway: `RpcResult { id, result }`  OR
//!    `RpcError { id, code, message }`. Fire-and-forget events go
//!    Sidecar → Gateway as `Event { name, data }`. Heartbeat
//!    (either direction): `Ping` / `Pong`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolFrame {
    Register {
        token: String,
        sidecar_id: String,
        protocol_version: u32,
    },
    RegisterAck {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    RpcRequest {
        id: String,
        method: String,
        params: serde_json::Value,
    },
    RpcResult {
        id: String,
        result: serde_json::Value,
    },
    RpcError {
        id: String,
        code: String,
        message: String,
    },
    Event {
        name: String,
        data: serde_json::Value,
    },
    Ping {
        ts: u64,
    },
    Pong {
        ts: u64,
    },
}

pub fn encode(frame: &ToolFrame) -> Result<Vec<u8>, String> {
    rmp_serde::to_vec_named(frame).map_err(|e| format!("encode tool frame: {e}"))
}

pub fn decode(bytes: &[u8]) -> Result<ToolFrame, String> {
    rmp_serde::from_slice(bytes).map_err(|e| format!("decode tool frame: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_register() {
        let frame = ToolFrame::Register {
            token: "secret".into(),
            sidecar_id: "browser".into(),
            protocol_version: PROTOCOL_VERSION,
        };
        let bytes = encode(&frame).unwrap();
        let decoded = decode(&bytes).unwrap();
        match decoded {
            ToolFrame::Register {
                token,
                sidecar_id,
                protocol_version,
            } => {
                assert_eq!(token, "secret");
                assert_eq!(sidecar_id, "browser");
                assert_eq!(protocol_version, PROTOCOL_VERSION);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_rpc() {
        let frame = ToolFrame::RpcRequest {
            id: "call-1".into(),
            method: "navigate".into(),
            params: serde_json::json!({ "url": "https://example.com/" }),
        };
        let bytes = encode(&frame).unwrap();
        let decoded = decode(&bytes).unwrap();
        match decoded {
            ToolFrame::RpcRequest { id, method, params } => {
                assert_eq!(id, "call-1");
                assert_eq!(method, "navigate");
                assert_eq!(params["url"], "https://example.com/");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
