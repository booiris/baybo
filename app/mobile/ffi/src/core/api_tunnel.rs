//! Shared client side of an E2E API tunnel leg.

use device_proto::api_tunnel;
use device_proto::noise::{FrameReassembler, write_chunked};
use snow::TransportState;

use super::error::MobileError;

pub use device_proto::api_tunnel::{MAX_TUNNEL_CHUNK, TunnelHeader, TunnelRequest, TunnelResponse};

pub struct ApiTunnelSession {
    transport: TransportState,
    reassembler: FrameReassembler,
}

impl ApiTunnelSession {
    pub(crate) fn new(transport: TransportState) -> Self {
        Self {
            transport,
            reassembler: FrameReassembler::new(),
        }
    }

    pub fn seal(&mut self, req: &TunnelRequest) -> Result<Vec<Vec<u8>>, MobileError> {
        let plaintext = api_tunnel::encode(req)?;
        Ok(write_chunked(&mut self.transport, &plaintext)?)
    }

    pub fn open(&mut self, message: &[u8]) -> Result<Vec<TunnelResponse>, MobileError> {
        let mut out = Vec::new();
        for bytes in self.reassembler.read(&mut self.transport, message)? {
            out.push(api_tunnel::decode::<TunnelResponse>(&bytes)?);
        }
        Ok(out)
    }
}

pub fn blob_id_sha256_hex(blob_id: &str) -> Option<&str> {
    let rest = blob_id.strip_prefix("sha256:")?;
    rest.split('.').next()
}
