//! Browser-link framing: `[u32 BE total_len][u8 kind][body]`, where
//! `total_len` counts the kind byte plus the body.
//!
//! - kind [`KIND_JSON`]: body is one JSON `LinkUp` / `LinkDown`.
//! - kind [`KIND_FRAME`]: body is `[u32 BE hdr_len][FrameHeader JSON][JPEG]`,
//!   kept as one `Bytes` so the gateway forwards it verbatim to viewers.
//!
//! Every length is checked against its limit as soon as the bytes that carry
//! it are buffered, before any buffer is grown for the message.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::Serialize;
use tokio_util::codec::{Decoder, Encoder};

use crate::error::CodecError;
use crate::limits::{
    KIND_BYTES, LEN_PREFIX_BYTES, MAX_FRAME_HEADER_BYTES, MAX_LINK_JSON_BYTES,
    MAX_LINK_MESSAGE_BYTES, MAX_SCREENCAST_FRAME_BYTES,
};
use crate::wire::{FrameHeader, LinkDown, LinkUp};

pub(crate) const KIND_JSON: u8 = 0;
pub(crate) const KIND_FRAME: u8 = 1;

const MESSAGE_PREFIX_BYTES: usize = LEN_PREFIX_BYTES + KIND_BYTES;

/// A decoded sidecar → gateway message.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LinkMessage {
    Json(LinkUp),
    Frame(FrameMsg),
    /// A well-framed message whose JSON did not parse (an unknown variant
    /// from a newer sidecar, a `null` field, …). Its bytes are already
    /// consumed, so the link stays usable; the caller logs and skips it.
    Malformed {
        kind: MalformedKind,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MalformedKind {
    Json,
    FrameHeader,
}

/// One screencast frame whose header parsed as a [`FrameHeader`].
/// `header_and_jpeg` is the exact `[u32 BE hdr_len][FrameHeader JSON][JPEG]`
/// slice off the link, shared zero-copy and sent to viewers as a WS binary
/// message unchanged. The header is parsed only to validate it; nothing on
/// the gateway reads it. Holding the bytes keeps the whole read-buffer
/// allocation they were split from alive, which can be up to about twice the
/// frame size.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FrameMsg {
    pub(crate) header_and_jpeg: Bytes,
}

/// Gateway side of the link: decodes [`LinkMessage`]s, encodes [`LinkDown`].
/// Only framing violations are errors; JSON content failures come back as
/// [`LinkMessage::Malformed`].
#[derive(Debug, Default)]
pub(crate) struct GatewayLinkCodec;

impl Decoder for GatewayLinkCodec {
    type Item = LinkMessage;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let message = match decode_raw(src)? {
            None => return Ok(None),
            Some(RawMessage::Json(body)) => match serde_json::from_slice(&body) {
                Ok(up) => LinkMessage::Json(up),
                Err(e) => LinkMessage::Malformed {
                    kind: MalformedKind::Json,
                    reason: e.to_string(),
                },
            },
            Some(RawMessage::Frame { header_len, body }) => decode_frame(header_len, body),
        };
        Ok(Some(message))
    }
}

impl Encoder<LinkDown> for GatewayLinkCodec {
    type Error = CodecError;

    fn encode(&mut self, item: LinkDown, dst: &mut BytesMut) -> Result<(), Self::Error> {
        encode_json(&item, dst)
    }
}

enum RawMessage {
    Json(BytesMut),
    /// `body` is `[u32 BE hdr_len][header][JPEG]`, with `header_len` already
    /// checked to fit inside it.
    Frame {
        header_len: usize,
        body: BytesMut,
    },
}

fn decode_raw(src: &mut BytesMut) -> Result<Option<RawMessage>, CodecError> {
    let Some(len) = peek_u32(src, 0) else {
        return Ok(None);
    };
    if len == 0 {
        return Err(CodecError::EmptyMessage);
    }
    if len > MAX_LINK_MESSAGE_BYTES {
        return Err(CodecError::MessageTooLarge {
            len,
            max: MAX_LINK_MESSAGE_BYTES,
        });
    }
    let Some(&kind) = src.get(LEN_PREFIX_BYTES) else {
        return Ok(None);
    };
    let body_len = len - KIND_BYTES;
    let frame_header_len = match kind {
        KIND_JSON => {
            if body_len > MAX_LINK_JSON_BYTES {
                return Err(CodecError::JsonTooLarge {
                    len: body_len,
                    max: MAX_LINK_JSON_BYTES,
                });
            }
            None
        }
        KIND_FRAME => {
            if body_len < LEN_PREFIX_BYTES {
                return Err(CodecError::FrameTruncated);
            }
            let Some(header_len) = peek_u32(src, MESSAGE_PREFIX_BYTES) else {
                return Ok(None);
            };
            check_frame_lengths(body_len, header_len)?;
            Some(header_len)
        }
        other => return Err(CodecError::UnknownKind { kind: other }),
    };
    let total = LEN_PREFIX_BYTES + len;
    if src.len() < total {
        src.reserve(total - src.len());
        return Ok(None);
    }
    src.advance(MESSAGE_PREFIX_BYTES);
    let body = src.split_to(body_len);
    Ok(Some(match frame_header_len {
        None => RawMessage::Json(body),
        Some(header_len) => RawMessage::Frame { header_len, body },
    }))
}

fn peek_u32(src: &[u8], at: usize) -> Option<usize> {
    let bytes: [u8; LEN_PREFIX_BYTES] = src.get(at..at + LEN_PREFIX_BYTES)?.try_into().ok()?;
    usize::try_from(u32::from_be_bytes(bytes)).ok()
}

fn check_frame_lengths(body_len: usize, header_len: usize) -> Result<(), CodecError> {
    if header_len > MAX_FRAME_HEADER_BYTES {
        return Err(CodecError::FrameHeaderTooLarge {
            len: header_len,
            max: MAX_FRAME_HEADER_BYTES,
        });
    }
    let after_prefix = body_len - LEN_PREFIX_BYTES;
    if header_len > after_prefix {
        return Err(CodecError::FrameHeaderOverrun {
            header_len,
            body_len,
        });
    }
    let jpeg_len = after_prefix - header_len;
    if jpeg_len == 0 {
        return Err(CodecError::EmptyJpeg);
    }
    if jpeg_len > MAX_SCREENCAST_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge {
            len: jpeg_len,
            max: MAX_SCREENCAST_FRAME_BYTES,
        });
    }
    Ok(())
}

fn decode_frame(header_len: usize, body: BytesMut) -> LinkMessage {
    let header_json = &body[LEN_PREFIX_BYTES..LEN_PREFIX_BYTES + header_len];
    match serde_json::from_slice::<FrameHeader>(header_json) {
        Ok(_) => LinkMessage::Frame(FrameMsg {
            header_and_jpeg: body.freeze(),
        }),
        Err(e) => LinkMessage::Malformed {
            kind: MalformedKind::FrameHeader,
            reason: e.to_string(),
        },
    }
}

fn encode_json<T: Serialize>(item: &T, dst: &mut BytesMut) -> Result<(), CodecError> {
    let body = serde_json::to_vec(item).map_err(|e| CodecError::Encode {
        reason: e.to_string(),
    })?;
    if body.len() > MAX_LINK_JSON_BYTES {
        return Err(CodecError::JsonTooLarge {
            len: body.len(),
            max: MAX_LINK_JSON_BYTES,
        });
    }
    put_message(KIND_JSON, &[&body], dst)
}

fn put_message(kind: u8, parts: &[&[u8]], dst: &mut BytesMut) -> Result<(), CodecError> {
    let len = KIND_BYTES + parts.iter().map(|p| p.len()).sum::<usize>();
    if len > MAX_LINK_MESSAGE_BYTES {
        return Err(CodecError::MessageTooLarge {
            len,
            max: MAX_LINK_MESSAGE_BYTES,
        });
    }
    let prefix = u32::try_from(len).map_err(|_| CodecError::MessageTooLarge {
        len,
        max: MAX_LINK_MESSAGE_BYTES,
    })?;
    dst.reserve(LEN_PREFIX_BYTES + len);
    dst.put_u32(prefix);
    dst.put_u8(kind);
    for part in parts {
        dst.put_slice(part);
    }
    Ok(())
}

/// What a fake sidecar sends to the gateway.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub enum SidecarMessage {
    Json(LinkUp),
    Frame { header: FrameHeader, jpeg: Bytes },
}

/// Sidecar side of the link, for driving a fake sidecar in tests: decodes
/// [`LinkDown`], encodes [`SidecarMessage`].
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct SidecarLinkCodec;

#[cfg(any(test, feature = "test-support"))]
impl Decoder for SidecarLinkCodec {
    type Item = LinkDown;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        match decode_raw(src)? {
            None => Ok(None),
            Some(RawMessage::Json(body)) => {
                serde_json::from_slice(&body)
                    .map(Some)
                    .map_err(|e| CodecError::Decode {
                        reason: e.to_string(),
                    })
            }
            Some(RawMessage::Frame { .. }) => Err(CodecError::UnexpectedFrame),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Encoder<SidecarMessage> for SidecarLinkCodec {
    type Error = CodecError;

    fn encode(&mut self, item: SidecarMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            SidecarMessage::Json(up) => encode_json(&up, dst),
            SidecarMessage::Frame { header, jpeg } => encode_frame(&header, &jpeg, dst),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
fn encode_frame(header: &FrameHeader, jpeg: &[u8], dst: &mut BytesMut) -> Result<(), CodecError> {
    let header_json = serde_json::to_vec(header).map_err(|e| CodecError::Encode {
        reason: e.to_string(),
    })?;
    if header_json.len() > MAX_FRAME_HEADER_BYTES {
        return Err(CodecError::FrameHeaderTooLarge {
            len: header_json.len(),
            max: MAX_FRAME_HEADER_BYTES,
        });
    }
    if jpeg.is_empty() {
        return Err(CodecError::EmptyJpeg);
    }
    if jpeg.len() > MAX_SCREENCAST_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge {
            len: jpeg.len(),
            max: MAX_SCREENCAST_FRAME_BYTES,
        });
    }
    let header_len =
        u32::try_from(header_json.len()).map_err(|_| CodecError::FrameHeaderTooLarge {
            len: header_json.len(),
            max: MAX_FRAME_HEADER_BYTES,
        })?;
    put_message(
        KIND_FRAME,
        &[&header_len.to_be_bytes(), &header_json, jpeg],
        dst,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{
        BootId, BrowserMode, BrowserPhase, BrowserStatus, Capability, HelloRejectReason,
        LinkSecret, TargetId,
    };

    fn header_of(frame: &FrameMsg) -> FrameHeader {
        let bytes = frame.header_and_jpeg.as_ref();
        let len = u32::from_be_bytes(bytes[..LEN_PREFIX_BYTES].try_into().unwrap()) as usize;
        serde_json::from_slice(&bytes[LEN_PREFIX_BYTES..LEN_PREFIX_BYTES + len]).unwrap()
    }

    fn header() -> FrameHeader {
        FrameHeader {
            target_id: TargetId::new("T1"),
            browser_gen: 1,
            seq: 9,
            captured_at_ms: 12.5,
            device_width: 1280.0,
            device_height: 800.0,
            offset_top: 0.0,
            page_scale_factor: 1.0,
            scroll_offset_x: 0.0,
            scroll_offset_y: 0.0,
        }
    }

    fn hello() -> LinkUp {
        LinkUp::Hello {
            protocol: 1,
            secret: LinkSecret::new("s"),
            boot_id: BootId::new("b"),
            pid: 7,
            capabilities: vec![Capability::Screencast],
        }
    }

    fn encode_sidecar(msgs: Vec<SidecarMessage>) -> BytesMut {
        let mut buf = BytesMut::new();
        for msg in msgs {
            SidecarLinkCodec.encode(msg, &mut buf).unwrap();
        }
        buf
    }

    fn decode_all(buf: &mut BytesMut) -> Vec<LinkMessage> {
        let mut out = Vec::new();
        while let Some(msg) = GatewayLinkCodec.decode(buf).unwrap() {
            out.push(msg);
        }
        out
    }

    fn raw(len: u32, kind: u8, body: &[u8]) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u32(len);
        buf.put_u8(kind);
        buf.put_slice(body);
        buf
    }

    fn frame_body(header_len: u32, header_json: &[u8], jpeg_len: usize) -> Vec<u8> {
        let mut body = header_len.to_be_bytes().to_vec();
        body.extend_from_slice(header_json);
        body.resize(body.len() + jpeg_len, 0xAB);
        body
    }

    #[test]
    fn json_and_frame_round_trip() {
        let status = LinkUp::Status(BrowserStatus {
            mode: BrowserMode::Host,
            phase: BrowserPhase::Ready,
            browser_gen: 2,
        });
        let jpeg = Bytes::from_static(b"\xFF\xD8jpeg-bytes\xFF\xD9");
        let mut buf = encode_sidecar(vec![
            SidecarMessage::Json(hello()),
            SidecarMessage::Frame {
                header: header(),
                jpeg: jpeg.clone(),
            },
            SidecarMessage::Json(status.clone()),
        ]);
        let decoded = decode_all(&mut buf);
        assert!(buf.is_empty());
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0], LinkMessage::Json(hello()));
        assert_eq!(decoded[2], LinkMessage::Json(status));
        let LinkMessage::Frame(frame) = &decoded[1] else {
            panic!("expected frame, got {:?}", decoded[1]);
        };
        assert_eq!(header_of(frame), header());
        let header_json = serde_json::to_vec(&header()).unwrap();
        let mut expected = (header_json.len() as u32).to_be_bytes().to_vec();
        expected.extend_from_slice(&header_json);
        expected.extend_from_slice(&jpeg);
        assert_eq!(frame.header_and_jpeg.as_ref(), expected.as_slice());
    }

    #[test]
    fn link_down_round_trip() {
        let msgs = [
            LinkDown::hello_ack(),
            LinkDown::HelloReject {
                reason: HelloRejectReason::AlreadyConnected,
            },
            LinkDown::StartScreencast,
            LinkDown::StopScreencast,
        ];
        let mut buf = BytesMut::new();
        for msg in msgs.clone() {
            GatewayLinkCodec.encode(msg, &mut buf).unwrap();
        }
        let mut decoded = Vec::new();
        while let Some(msg) = SidecarLinkCodec.decode(&mut buf).unwrap() {
            decoded.push(msg);
        }
        assert_eq!(decoded, msgs);
    }

    #[test]
    fn wire_bytes_are_length_kind_json() {
        let mut buf = BytesMut::new();
        GatewayLinkCodec
            .encode(LinkDown::StartScreencast, &mut buf)
            .unwrap();
        let json = br#"{"type":"start_screencast"}"#;
        assert_eq!(&buf[..4], &((json.len() + 1) as u32).to_be_bytes());
        assert_eq!(buf[4], KIND_JSON);
        assert_eq!(&buf[5..], json);
    }

    #[test]
    fn byte_at_a_time_reads() {
        let full = encode_sidecar(vec![
            SidecarMessage::Json(hello()),
            SidecarMessage::Frame {
                header: header(),
                jpeg: Bytes::from_static(b"jpeg"),
            },
        ]);
        let mut buf = BytesMut::new();
        let mut decoded = Vec::new();
        for byte in full.iter() {
            buf.put_u8(*byte);
            if let Some(msg) = GatewayLinkCodec.decode(&mut buf).unwrap() {
                decoded.push(msg);
            }
        }
        assert!(buf.is_empty());
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], LinkMessage::Json(hello()));
        assert!(matches!(&decoded[1], LinkMessage::Frame(f) if header_of(f) == header()));
    }

    #[test]
    fn max_size_jpeg_is_accepted() {
        let mut buf = encode_sidecar(vec![SidecarMessage::Frame {
            header: header(),
            jpeg: Bytes::from(vec![0u8; MAX_SCREENCAST_FRAME_BYTES]),
        }]);
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf).unwrap(),
            Some(LinkMessage::Frame(_))
        ));
    }

    #[test]
    fn oversized_message_rejected_before_allocating() {
        let mut buf = BytesMut::with_capacity(LEN_PREFIX_BYTES);
        buf.put_u32((MAX_LINK_MESSAGE_BYTES + 1) as u32);
        let capacity = buf.capacity();
        let err = GatewayLinkCodec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::MessageTooLarge { .. }), "{err}");
        assert_eq!(buf.capacity(), capacity);
    }

    #[test]
    fn empty_message_rejected() {
        let mut buf = BytesMut::new();
        buf.put_u32(0);
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::EmptyMessage)
        ));
    }

    #[test]
    fn unknown_kind_rejected() {
        let mut buf = raw(3, 7, b"xy");
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::UnknownKind { kind: 7 })
        ));
    }

    #[test]
    fn oversized_json_rejected_from_prefix_alone() {
        let mut buf = raw((MAX_LINK_JSON_BYTES + 2) as u32, KIND_JSON, b"");
        let capacity = buf.capacity();
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::JsonTooLarge { .. })
        ));
        assert_eq!(buf.capacity(), capacity);
    }

    #[test]
    fn encoder_rejects_oversized_json() {
        let huge = "x".repeat(MAX_LINK_JSON_BYTES);
        let mut buf = BytesMut::new();
        assert!(matches!(
            encode_json(&huge, &mut buf),
            Err(CodecError::JsonTooLarge { .. })
        ));
        assert!(buf.is_empty());
    }

    #[test]
    fn frame_shorter_than_header_prefix_rejected() {
        let mut buf = raw(3, KIND_FRAME, b"\0\0");
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::FrameTruncated)
        ));
    }

    #[test]
    fn oversized_frame_header_rejected() {
        let header_len = (MAX_FRAME_HEADER_BYTES + 1) as u32;
        let body = header_len.to_be_bytes();
        let mut buf = raw(
            (KIND_BYTES + LEN_PREFIX_BYTES + MAX_FRAME_HEADER_BYTES + 1) as u32,
            KIND_FRAME,
            &body,
        );
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::FrameHeaderTooLarge { .. })
        ));
    }

    #[test]
    fn frame_header_overrunning_body_rejected() {
        let body = frame_body(100, b"{}", 0);
        let mut buf = raw((KIND_BYTES + body.len()) as u32, KIND_FRAME, &body);
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::FrameHeaderOverrun { .. })
        ));
    }

    #[test]
    fn oversized_jpeg_rejected_from_prefixes_alone() {
        let header_json = serde_json::to_vec(&header()).unwrap();
        let len =
            KIND_BYTES + LEN_PREFIX_BYTES + header_json.len() + MAX_SCREENCAST_FRAME_BYTES + 1;
        let mut buf = raw(
            len as u32,
            KIND_FRAME,
            &(header_json.len() as u32).to_be_bytes(),
        );
        let capacity = buf.capacity();
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::FrameTooLarge { .. })
        ));
        assert_eq!(buf.capacity(), capacity);
    }

    #[test]
    fn encoder_rejects_oversized_jpeg() {
        let mut buf = BytesMut::new();
        let err = SidecarLinkCodec
            .encode(
                SidecarMessage::Frame {
                    header: header(),
                    jpeg: Bytes::from(vec![0u8; MAX_SCREENCAST_FRAME_BYTES + 1]),
                },
                &mut buf,
            )
            .unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge { .. }), "{err}");
    }

    #[test]
    fn malformed_json_is_skipped_not_fatal() {
        let mut buf = raw(4, KIND_JSON, b"{x}");
        let unknown = br#"{"type":"control_granted"}"#;
        buf.extend_from_slice(&raw(
            (KIND_BYTES + unknown.len()) as u32,
            KIND_JSON,
            unknown,
        ));
        buf.extend_from_slice(&encode_sidecar(vec![SidecarMessage::Json(hello())]));
        let decoded = decode_all(&mut buf);
        assert!(buf.is_empty());
        assert_eq!(decoded.len(), 3);
        for bad in &decoded[..2] {
            assert!(
                matches!(
                    bad,
                    LinkMessage::Malformed {
                        kind: MalformedKind::Json,
                        ..
                    }
                ),
                "{bad:?}"
            );
        }
        assert_eq!(decoded[2], LinkMessage::Json(hello()));
    }

    #[test]
    fn malformed_frame_header_is_skipped_not_fatal() {
        let mut bad_headers = vec![b"{x}".to_vec()];
        let mut with_null = serde_json::to_value(header()).unwrap();
        with_null["captured_at_ms"] = serde_json::Value::Null;
        bad_headers.push(serde_json::to_vec(&with_null).unwrap());
        let mut with_extra = serde_json::to_value(header()).unwrap();
        with_extra["extra"] = serde_json::json!(true);
        bad_headers.push(serde_json::to_vec(&with_extra).unwrap());

        let mut buf = BytesMut::new();
        for bad in &bad_headers {
            let body = frame_body(bad.len() as u32, bad, 4);
            buf.extend_from_slice(&raw((KIND_BYTES + body.len()) as u32, KIND_FRAME, &body));
        }
        buf.extend_from_slice(&encode_sidecar(vec![SidecarMessage::Frame {
            header: header(),
            jpeg: Bytes::from_static(b"jpeg"),
        }]));
        let decoded = decode_all(&mut buf);
        assert!(buf.is_empty());
        assert_eq!(decoded.len(), bad_headers.len() + 1);
        for bad in &decoded[..bad_headers.len()] {
            assert!(
                matches!(
                    bad,
                    LinkMessage::Malformed {
                        kind: MalformedKind::FrameHeader,
                        ..
                    }
                ),
                "{bad:?}"
            );
        }
        assert!(matches!(decoded.last(), Some(LinkMessage::Frame(f)) if header_of(f) == header()));
    }

    #[test]
    fn json_of_exactly_max_size_is_accepted() {
        let mut body = br#"{"type":"availability","unavailable":null}"#.to_vec();
        body.resize(MAX_LINK_JSON_BYTES, b' ');
        let mut buf = raw((KIND_BYTES + body.len()) as u32, KIND_JSON, &body);
        assert_eq!(
            GatewayLinkCodec.decode(&mut buf).unwrap(),
            Some(LinkMessage::Json(LinkUp::Availability {
                unavailable: None
            }))
        );
    }

    #[test]
    fn empty_jpeg_rejected() {
        let header_json = serde_json::to_vec(&header()).unwrap();
        let body = frame_body(header_json.len() as u32, &header_json, 0);
        let mut buf = raw((KIND_BYTES + body.len()) as u32, KIND_FRAME, &body);
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::EmptyJpeg)
        ));
        let err = SidecarLinkCodec
            .encode(
                SidecarMessage::Frame {
                    header: header(),
                    jpeg: Bytes::new(),
                },
                &mut BytesMut::new(),
            )
            .unwrap_err();
        assert!(matches!(err, CodecError::EmptyJpeg), "{err}");
    }

    #[test]
    fn oversized_header_len_split_across_reads_rejected_before_body() {
        let header_len = ((MAX_FRAME_HEADER_BYTES + 1) as u32).to_be_bytes();
        let len = (KIND_BYTES + LEN_PREFIX_BYTES + MAX_FRAME_HEADER_BYTES + 1 + 4) as u32;
        let mut buf = raw(len, KIND_FRAME, &header_len[..2]);
        let capacity = buf.capacity();
        assert!(matches!(GatewayLinkCodec.decode(&mut buf), Ok(None)));
        assert_eq!(buf.capacity(), capacity);
        buf.put_slice(&header_len[2..]);
        assert!(matches!(
            GatewayLinkCodec.decode(&mut buf),
            Err(CodecError::FrameHeaderTooLarge { .. })
        ));
        assert!(buf.capacity() < MAX_FRAME_HEADER_BYTES);
    }

    #[test]
    fn sidecar_codec_rejects_frames() {
        let body = frame_body(2, b"{}", 1);
        let mut buf = raw((KIND_BYTES + body.len()) as u32, KIND_FRAME, &body);
        assert!(matches!(
            SidecarLinkCodec.decode(&mut buf),
            Err(CodecError::UnexpectedFrame)
        ));
    }
}
