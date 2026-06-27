//! The app side of a relay **blob leg**: the [`device_proto::blob`] sub-protocol
//! over the leg's Noise transport.
//!
//! A blob leg is the same Noise IK handshake as the chat content session (reuse
//! [`ContentHandshake::finish_blob`](crate::content::ContentHandshake::finish_blob)),
//! but the shell dials it with the `x-relay-leg-class: blob` header so the relay
//! meters it as background bandwidth, and then runs the blob request/response
//! protocol instead of the `Frame` loop. The shell pumps the opaque bytes over the
//! WebSocket and owns the temp file; this module is the transport-agnostic crypto
//! + codec + protocol state.

use baybo_model::SHA256_PREFIX;
use device_proto::blob::{self};
use device_proto::noise::{FrameReassembler, write_chunked};
use snow::TransportState;

use crate::error::MobileError;

pub use device_proto::blob::{BlobRequest, BlobResponse, MAX_BLOB_CHUNK};

/// An established blob leg: seal [`BlobRequest`]s to send and open received
/// [`BlobResponse`]s, all inside the leg's Noise transport.
pub struct BlobSession {
    transport: TransportState,
    reassembler: FrameReassembler,
}

impl BlobSession {
    pub(crate) fn new(transport: TransportState) -> Self {
        Self {
            transport,
            reassembler: FrameReassembler::new(),
        }
    }

    /// Seal a request into the Noise transport messages to send (chunked past the
    /// per-message ceiling — usually one message).
    pub fn seal(&mut self, req: &BlobRequest) -> Result<Vec<Vec<u8>>, MobileError> {
        Ok(write_chunked(&mut self.transport, &blob::encode(req)?)?)
    }

    /// Open one received Noise message into the responses it completes.
    pub fn open(&mut self, message: &[u8]) -> Result<Vec<BlobResponse>, MobileError> {
        let mut out = Vec::new();
        for bytes in self.reassembler.read(&mut self.transport, message)? {
            out.push(blob::decode::<BlobResponse>(&bytes)?);
        }
        Ok(out)
    }
}

/// The hex digest a blob's id encodes — what a download verifies the gateway's
/// reported digest against (so the gateway can't stream a *different* blob than
/// the one the phone holds the id for).
pub fn blob_id_sha256_hex(blob_id: &str) -> Option<&str> {
    blob_id
        .strip_prefix(SHA256_PREFIX)
        .and_then(|rest| rest.split('.').next())
}

/// A download request (resume by passing the bytes already on disk as `offset`).
pub fn pull(request_id: u64, blob_id: &str, offset: u64) -> BlobRequest {
    BlobRequest::Pull {
        request_id,
        blob_id: blob_id.to_owned(),
        offset,
    }
}

/// An upload offer.
pub fn push(
    request_id: u64,
    mime_type: &str,
    size: u64,
    sha256_hex: &str,
    filename: Option<String>,
) -> BlobRequest {
    BlobRequest::Push {
        request_id,
        mime_type: mime_type.to_owned(),
        size,
        sha256_hex: sha256_hex.to_owned(),
        filename,
    }
}

/// An upload chunk (after a [`BlobResponse::PushGrant`]).
pub fn chunk(request_id: u64, offset: u64, data: Vec<u8>, last: bool) -> BlobRequest {
    BlobRequest::Chunk {
        request_id,
        offset,
        data,
        last,
    }
}

/// Cancel an in-flight transfer.
pub fn abort(request_id: u64, reason: &str) -> BlobRequest {
    BlobRequest::Abort {
        request_id,
        reason: reason.to_owned(),
    }
}

/// What the shell should do with one fed [`BlobResponse`] while downloading.
#[derive(Debug, PartialEq, Eq)]
pub enum DownloadStep {
    /// Append `data` to the temp file at `offset`, then keep reading.
    Write { offset: u64, data: Vec<u8> },
    /// Transfer complete and the gateway's digest matched the requested id.
    Done,
    /// Nothing actionable (a response for another request).
    Idle,
}

/// Drives a resumable download: the shell feeds each [`BlobResponse`] and gets a
/// [`DownloadStep`], so a 100 MiB transfer streams straight to disk without
/// buffering. Validates that chunks are contiguous from the resume offset, the
/// total size matches, and the gateway's digest matches the id requested.
pub struct BlobDownload {
    request_id: u64,
    expected_hex: String,
    next_offset: u64,
}

impl BlobDownload {
    /// Start (or resume) a download. `resume_from` is the length of the partial
    /// file already on disk; the shell should have sent `pull(request_id, blob_id,
    /// resume_from)`.
    pub fn new(request_id: u64, blob_id: &str, resume_from: u64) -> Self {
        Self {
            request_id,
            expected_hex: blob_id_sha256_hex(blob_id).unwrap_or_default().to_owned(),
            next_offset: resume_from,
        }
    }

    /// Feed one response and learn what to do with it.
    pub fn on_response(&mut self, resp: BlobResponse) -> Result<DownloadStep, MobileError> {
        match resp {
            BlobResponse::Chunk {
                request_id,
                offset,
                data,
                ..
            } if request_id == self.request_id => {
                if offset != self.next_offset {
                    return Err(MobileError::Blob(format!(
                        "chunk offset {offset} != expected {}",
                        self.next_offset
                    )));
                }
                self.next_offset += data.len() as u64;
                Ok(DownloadStep::Write { offset, data })
            }
            BlobResponse::Done {
                request_id,
                total_size,
                sha256_hex,
            } if request_id == self.request_id => {
                if self.next_offset != total_size {
                    return Err(MobileError::Blob(format!(
                        "size {} != {total_size}",
                        self.next_offset
                    )));
                }
                if !self.expected_hex.is_empty() && sha256_hex != self.expected_hex {
                    return Err(MobileError::Blob("content digest mismatch".into()));
                }
                Ok(DownloadStep::Done)
            }
            BlobResponse::Err { reason, .. } => Err(MobileError::Blob(reason)),
            _ => Ok(DownloadStep::Idle),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentHandshake;
    use device_proto::noise::{NOISE_MAX_MESSAGE, StaticKeypair};
    use snow::TransportState;

    /// App-initiator + gateway-responder IK handshake, finalized into a blob
    /// session; returns it plus the gateway's raw transport (the test drives the
    /// gateway by hand, as the real gateway blob responder would).
    fn established_blob_pair() -> (BlobSession, TransportState) {
        let app = StaticKeypair::generate().unwrap();
        let gw = StaticKeypair::generate().unwrap();
        let (handshake, msg1) = ContentHandshake::start(&app, &gw.public()).unwrap();
        let mut gw_hs = gw.ik_responder().unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        gw_hs.read_message(&msg1, &mut buf).unwrap();
        let n = gw_hs.write_message(&[], &mut buf).unwrap();
        let session = handshake.finish_blob(&buf[..n]).unwrap();
        (session, gw_hs.into_transport_mode().unwrap())
    }

    #[test]
    fn download_streams_chunks_and_verifies_the_digest() {
        let (mut session, mut gw_t) = established_blob_pair();
        let mut gw_reasm = FrameReassembler::new();

        // App → gateway: Pull. The gateway (driven by hand) decodes it.
        let blob_id = "sha256:deadbeef.tok";
        let sealed = session.seal(&pull(1, blob_id, 0)).unwrap();
        let mut got_req = None;
        for m in &sealed {
            for b in gw_reasm.read(&mut gw_t, m).unwrap() {
                got_req = Some(blob::decode::<BlobRequest>(&b).unwrap());
            }
        }
        assert!(matches!(
            got_req,
            Some(BlobRequest::Pull { request_id: 1, .. })
        ));

        // Gateway → app: two chunks + Done; the app reassembles + verifies.
        let payload = vec![7u8; (MAX_BLOB_CHUNK as f64 * 1.5) as usize];
        let mut download = BlobDownload::new(1, blob_id, 0);
        let mut assembled = Vec::new();
        let responses = {
            let mut v = Vec::new();
            for (i, c) in payload.chunks(MAX_BLOB_CHUNK).enumerate() {
                let last = (i + 1) * MAX_BLOB_CHUNK >= payload.len();
                v.push(BlobResponse::Chunk {
                    request_id: 1,
                    offset: (i * MAX_BLOB_CHUNK) as u64,
                    data: c.to_vec(),
                    last,
                });
            }
            v.push(BlobResponse::Done {
                request_id: 1,
                total_size: payload.len() as u64,
                sha256_hex: "deadbeef".into(),
            });
            v
        };
        let mut done = false;
        for resp in responses {
            // Seal on the gateway side, open on the app side (exercises Noise).
            for m in write_chunked(&mut gw_t, &blob::encode(&resp).unwrap()).unwrap() {
                for opened in session.open(&m).unwrap() {
                    match download.on_response(opened).unwrap() {
                        DownloadStep::Write { offset, data } => {
                            assert_eq!(offset as usize, assembled.len());
                            assembled.extend_from_slice(&data);
                        }
                        DownloadStep::Done => done = true,
                        DownloadStep::Idle => {}
                    }
                }
            }
        }
        assert!(done, "the download completed");
        assert_eq!(assembled, payload, "streamed bytes match");
    }

    #[test]
    fn download_rejects_a_digest_mismatch() {
        let mut download = BlobDownload::new(3, "sha256:aaaa.tok", 0);
        // A Done whose digest is not the requested id's hex is refused.
        let err = download.on_response(BlobResponse::Done {
            request_id: 3,
            total_size: 0,
            sha256_hex: "bbbb".into(),
        });
        assert!(matches!(err, Err(MobileError::Blob(_))));
    }

    #[test]
    fn download_surfaces_a_server_error() {
        let mut download = BlobDownload::new(4, "sha256:aaaa.tok", 0);
        let err = download.on_response(BlobResponse::Err {
            request_id: 4,
            reason: "unknown or unauthorized blob".into(),
        });
        assert!(matches!(err, Err(MobileError::Blob(r)) if r.contains("unauthorized")));
    }
}
