//! Minimal Server-Sent Events reader over a transport byte stream.
//!
//! The gateway emits frames of the form:
//!
//! ```text
//! event: delta
//! data: {"kind":"delta","text":"hello"}
//!
//! event: response
//! data: {"kind":"response","text":"final"}
//! ```
//!
//! plus occasional keep-alive comments (`: ping`) and an optional
//! trailing `event: end` frame. We only care about `(event, data)`
//! pairs; comments and unknown fields are skipped per the WHATWG SSE
//! grammar.
//!
//! The parser is transport-agnostic — it consumes any
//! `Stream<Item = ClientResult<Bytes>>`, so the UDS-backed hyper body
//! (today's only transport) and any future byte stream reuse the same
//! frame reassembly.

use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures::Stream;
use serde::de::DeserializeOwned;

use super::dto::{ClientError, ClientResult};

/// Decoded SSE frame. `event` is the value of the `event:` line
/// (defaults to `"message"` per the spec when absent); `data` is the
/// concatenated `data:` lines joined by `\n`.
struct SseFrame {
    event: String,
    data: String,
}

/// Byte stream feeding an [`SseStream`]. Abstracted over the transport
/// (reqwest response body, hyper UDS body) so the frame parser is
/// reused.
pub type SseByteStream = Pin<Box<dyn Stream<Item = ClientResult<Bytes>> + Send>>;

/// Stream of deserialized events from an SSE endpoint.
///
/// Parameterized over `T` so both the session stream (`SseEvent`) and
/// the approval stream (`ApprovalEvent`) reuse the same transport.
pub struct SseStream<T> {
    inner: SseByteStream,
    buffer: BytesMut,
    /// In-flight frame accumulator, reset on each blank line.
    current_event: String,
    current_data: String,
    /// Whether any `data:` / `event:` line has been seen since the last
    /// blank-line flush. Distinguishes "frame with empty fields" from
    /// "no frame yet".
    has_fields: bool,
    /// `fn(T) -> T` so the phantom carries `T` without forcing
    /// `T: Unpin` — the struct holds no actual `T` values.
    _marker: PhantomData<fn(T) -> T>,
}

// SAFETY: all runtime fields are `Unpin`; `T` exists only in the
// zero-sized phantom, which `fn(T) -> T` already makes `Unpin`.
impl<T> Unpin for SseStream<T> {}

impl<T> SseStream<T>
where
    T: DeserializeOwned + 'static,
{
    /// Wrap any transport-level `Bytes` stream (hyper, …). Errors are
    /// already in [`ClientError`] shape so the transport glue does the
    /// mapping, not this parser.
    pub fn from_bytes_stream(inner: SseByteStream) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
            current_event: String::new(),
            current_data: String::new(),
            has_fields: false,
            _marker: PhantomData,
        }
    }

    /// Try to extract the next complete frame from the internal buffer.
    /// Returns `None` when more bytes are needed.
    fn next_frame(&mut self) -> Option<SseFrame> {
        // Walk the buffer, consuming one line per iteration until we hit
        // a blank line that delimits a frame. Lines may end with `\n` or
        // `\r\n`; both are handled here.
        loop {
            let nl_idx = self.buffer.iter().position(|b| *b == b'\n')?;
            let line_bytes = self.buffer.split_to(nl_idx + 1);
            // Trim trailing \n and optional \r.
            let line_len = line_bytes.len().saturating_sub(1);
            let trimmed = &line_bytes[..line_len];
            let trimmed = if trimmed.last() == Some(&b'\r') {
                &trimmed[..trimmed.len() - 1]
            } else {
                trimmed
            };

            if trimmed.is_empty() {
                if self.has_fields {
                    let event = std::mem::take(&mut self.current_event);
                    let data = std::mem::take(&mut self.current_data);
                    self.has_fields = false;
                    let event = if event.is_empty() {
                        "message".to_string()
                    } else {
                        event
                    };
                    return Some(SseFrame { event, data });
                }
                continue;
            }

            // Comment line — skip.
            if trimmed.first() == Some(&b':') {
                continue;
            }

            // Field lines are `name: value` or `name:value` (the leading
            // space after the colon is optional per spec). Unknown
            // fields are ignored silently.
            let line = match std::str::from_utf8(trimmed) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let (name, value) = match line.split_once(':') {
                Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match name {
                "event" => {
                    self.current_event = value.to_string();
                    self.has_fields = true;
                }
                "data" => {
                    if !self.current_data.is_empty() {
                        self.current_data.push('\n');
                    }
                    self.current_data.push_str(value);
                    self.has_fields = true;
                }
                // `id` / `retry` are ignored by this client.
                _ => {}
            }
        }
    }
}

impl<T> Stream for SseStream<T>
where
    T: DeserializeOwned + 'static,
{
    type Item = ClientResult<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `SseStream` holds only `Unpin` fields (the inner stream is
        // `Pin<Box<..>>`, itself `Unpin`), so projecting through
        // `get_mut` is sound.
        let this = self.get_mut();
        loop {
            if let Some(frame) = this.next_frame() {
                if frame.event == "end" {
                    return Poll::Ready(None);
                }
                if frame.event == "error" {
                    return Poll::Ready(Some(Err(ClientError::Server {
                        status: 0,
                        message: frame.data,
                    })));
                }
                if frame.data.is_empty() {
                    continue;
                }
                let decoded = serde_json::from_str::<T>(&frame.data)
                    .map_err(|e| ClientError::Decode(format!("{e} (payload: {})", frame.data)));
                return Poll::Ready(Some(decoded));
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => {
                    if this.has_fields {
                        let event = std::mem::take(&mut this.current_event);
                        let data = std::mem::take(&mut this.current_data);
                        this.has_fields = false;
                        let event = if event.is_empty() {
                            "message".to_string()
                        } else {
                            event
                        };
                        if event == "end" || data.is_empty() {
                            return Poll::Ready(None);
                        }
                        let decoded = serde_json::from_str::<T>(&data)
                            .map_err(|e| ClientError::Decode(format!("{e} (payload: {data})")));
                        return Poll::Ready(Some(decoded));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::SseEvent;

    use super::*;
    use futures::StreamExt;
    use futures::stream;

    fn stream_from_chunks(chunks: Vec<&'static [u8]>) -> SseStream<SseEvent> {
        // Build a stream of Bytes chunks that looks like the reqwest or
        // hyper body stream output. We can't call `from_response`
        // directly without a live HTTP server, so feed
        // `from_bytes_stream` a synthetic stream.
        let inner = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<Bytes, ClientError>(Bytes::from_static(c))),
        );
        SseStream::from_bytes_stream(Box::pin(inner))
    }

    #[tokio::test]
    async fn parses_single_frame() {
        let mut s = stream_from_chunks(vec![
            b"event: delta\ndata: {\"kind\":\"delta\",\"text\":\"hi\"}\n\n",
        ]);
        let first = s.next().await.unwrap().unwrap();
        match first {
            SseEvent::Delta { text } => assert_eq!(text, "hi"),
            other => panic!("expected delta, got {other:?}"),
        }
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn reassembles_split_frame() {
        // Frame is split mid-field across three chunks.
        let mut s = stream_from_chunks(vec![
            b"event: delta\nda",
            b"ta: {\"kind\":\"delta\",",
            b"\"text\":\"abc\"}\n\n",
        ]);
        let first = s.next().await.unwrap().unwrap();
        match first {
            SseEvent::Delta { text } => assert_eq!(text, "abc"),
            other => panic!("expected delta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn end_frame_closes_stream() {
        let mut s = stream_from_chunks(vec![
            b"event: delta\ndata: {\"kind\":\"delta\",\"text\":\"x\"}\n\n",
            b"event: end\ndata: \n\n",
            b"event: delta\ndata: {\"kind\":\"delta\",\"text\":\"after\"}\n\n",
        ]);
        let _ = s.next().await.unwrap().unwrap();
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn skips_comments_and_keepalives() {
        let mut s = stream_from_chunks(vec![
            b": ping\n\n",
            b"event: delta\ndata: {\"kind\":\"delta\",\"text\":\"y\"}\n\n",
        ]);
        let first = s.next().await.unwrap().unwrap();
        match first {
            SseEvent::Delta { text } => assert_eq!(text, "y"),
            other => panic!("expected delta, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multi_line_data_concatenates() {
        let mut s = stream_from_chunks(vec![
            b"event: notice\ndata: {\"kind\":\"notice\",\n",
            b"data: \"level\":\"warn\",\"text\":\"hi\"}\n\n",
        ]);
        let first = s.next().await.unwrap().unwrap();
        match first {
            SseEvent::Notice { level, text } => {
                assert_eq!(level, "warn");
                assert_eq!(text, "hi");
            }
            other => panic!("expected notice, got {other:?}"),
        }
    }
}
