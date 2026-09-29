//! Carrier-session framing. Every record on a direct QUIC stream or TCP
//! connection is `u32be(len) ‖ bytes` with `len ≤ MAX_DIRECT_FRAME_BYTES`. The
//! first record is the JSON [`DirectOpen`] preface; the Noise frames that
//! follow use the same framing.

use std::fmt;

use remote_host_protocol::relay::DirectOpen;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::error::FrameError;

/// One frame carries one Noise message, whose maximum this is.
pub const MAX_DIRECT_FRAME_BYTES: usize = 65_535;

const LEN_PREFIX_BYTES: usize = size_of::<u32>();
/// Room for a serialized `DirectOpen` (about 90 bytes), so encoding it never
/// reallocates and leaves no unwiped copy of the token behind.
const DIRECT_OPEN_JSON_CAPACITY: usize = 256;

/// Writes one frame in a single write. Not cancel safe: a cancelled write may
/// leave part of the frame on the stream.
pub async fn write_frame<W>(writer: &mut W, bytes: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let len = frame_len(bytes.len())?;
    let mut framed = Zeroizing::new(Vec::with_capacity(LEN_PREFIX_BYTES + bytes.len()));
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(bytes);
    writer.write_all(&framed).await?;
    Ok(())
}

/// Writes the `DirectOpen` preface, the first frame of every carrier session.
pub async fn write_direct_open<W>(writer: &mut W, open: &DirectOpen) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let mut json = Zeroizing::new(Vec::with_capacity(DIRECT_OPEN_JSON_CAPACITY));
    serde_json::to_writer(&mut *json, open).map_err(|error| FrameError::Preface {
        reason: json_error_reason(&error),
    })?;
    write_frame(writer, &json).await
}

/// Reads the `DirectOpen` preface. A stream that ends before it is
/// [`FrameError::MissingPreface`].
pub async fn read_direct_open<R>(reader: &mut FrameReader<R>) -> Result<DirectOpen, FrameError>
where
    R: AsyncRead + Unpin,
{
    let frame = Zeroizing::new(
        reader
            .next_frame()
            .await?
            .ok_or(FrameError::MissingPreface)?,
    );
    serde_json::from_slice(&frame).map_err(|error| FrameError::Preface {
        reason: json_error_reason(&error),
    })
}

/// Reads frames from one stream. It never reads past the frame it is
/// assembling, and [`FrameReader::next_frame`] is cancel safe: bytes already
/// read stay buffered for the next call, so it can sit in a `select!`. The
/// frame being assembled may be the `DirectOpen` preface, so it is wiped when
/// the reader drops and never printed.
pub struct FrameReader<R> {
    reader: R,
    prefix: [u8; LEN_PREFIX_BYTES],
    prefix_filled: usize,
    body: Zeroizing<Vec<u8>>,
    body_filled: usize,
    poisoned: bool,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            prefix: [0; LEN_PREFIX_BYTES],
            prefix_filled: 0,
            body: Zeroizing::new(Vec::new()),
            body_filled: 0,
            poisoned: false,
        }
    }

    /// The next frame, or `None` when the stream ends cleanly between frames.
    /// An end inside a frame is [`FrameError::Truncated`]; a declared length
    /// over the cap is refused before anything is allocated for it. An error
    /// ends the stream: every later call fails with [`FrameError::Poisoned`].
    pub async fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.poisoned {
            return Err(FrameError::Poisoned);
        }
        let frame = self.read_frame().await;
        self.poisoned = frame.is_err();
        frame
    }

    pub fn into_inner(self) -> R {
        self.reader
    }

    async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        while self.prefix_filled < LEN_PREFIX_BYTES {
            let read = self
                .reader
                .read(&mut self.prefix[self.prefix_filled..])
                .await?;
            if read == 0 {
                return if self.prefix_filled == 0 {
                    Ok(None)
                } else {
                    Err(FrameError::Truncated)
                };
            }
            self.prefix_filled += read;
            if self.prefix_filled == LEN_PREFIX_BYTES {
                let len = declared_len(self.prefix)?;
                self.body = Zeroizing::new(vec![0; len]);
                self.body_filled = 0;
            }
        }
        while self.body_filled < self.body.len() {
            let read = self.reader.read(&mut self.body[self.body_filled..]).await?;
            if read == 0 {
                return Err(FrameError::Truncated);
            }
            self.body_filled += read;
        }
        self.prefix_filled = 0;
        self.body_filled = 0;
        Ok(Some(std::mem::take(&mut *self.body)))
    }
}

impl<R> fmt::Debug for FrameReader<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameReader")
            .field("prefix_filled", &self.prefix_filled)
            .field("body_len", &self.body.len())
            .field("body_filled", &self.body_filled)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

fn frame_len(len: usize) -> Result<u32, FrameError> {
    let too_large = FrameError::TooLarge {
        len,
        max: MAX_DIRECT_FRAME_BYTES,
    };
    if len > MAX_DIRECT_FRAME_BYTES {
        return Err(too_large);
    }
    u32::try_from(len).map_err(|_| too_large)
}

fn declared_len(prefix: [u8; LEN_PREFIX_BYTES]) -> Result<usize, FrameError> {
    let len = usize::try_from(u32::from_be_bytes(prefix)).unwrap_or(usize::MAX);
    if len > MAX_DIRECT_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            len,
            max: MAX_DIRECT_FRAME_BYTES,
        });
    }
    Ok(len)
}

/// The category and position of a JSON error, never its text, which can quote
/// the input.
fn json_error_reason(error: &serde_json::Error) -> String {
    format!(
        "{:?} error at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use remote_host_protocol::relay::{DirectToken, LegClass};
    use tokio::io::{AsyncWriteExt, DuplexStream, duplex};

    use super::*;

    const PIPE_CAPACITY: usize = 4 * MAX_DIRECT_FRAME_BYTES;

    fn pipe() -> (DuplexStream, FrameReader<DuplexStream>) {
        let (writer, reader) = duplex(PIPE_CAPACITY);
        (writer, FrameReader::new(reader))
    }

    #[tokio::test]
    async fn frames_round_trip_and_a_clean_end_reads_as_none() {
        let (mut writer, mut reader) = pipe();
        write_frame(&mut writer, b"first").await.unwrap();
        write_frame(&mut writer, b"").await.unwrap();
        write_frame(&mut writer, &[7; MAX_DIRECT_FRAME_BYTES])
            .await
            .unwrap();
        drop(writer);

        assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"first");
        assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"");
        assert_eq!(
            reader.next_frame().await.unwrap().unwrap(),
            vec![7; MAX_DIRECT_FRAME_BYTES]
        );
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_on_write_and_on_read() {
        let (mut writer, mut reader) = pipe();
        let oversized = vec![0; MAX_DIRECT_FRAME_BYTES + 1];
        assert!(matches!(
            write_frame(&mut writer, &oversized).await,
            Err(FrameError::TooLarge { len, .. }) if len == MAX_DIRECT_FRAME_BYTES + 1
        ));

        let declared = u32::try_from(MAX_DIRECT_FRAME_BYTES + 1).unwrap();
        writer.write_all(&declared.to_be_bytes()).await.unwrap();
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::TooLarge { len, .. }) if len == MAX_DIRECT_FRAME_BYTES + 1
        ));
        write_frame(&mut writer, b"after").await.unwrap();
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Poisoned)
        ));

        let (mut writer, mut reader) = pipe();
        writer.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn an_end_inside_a_frame_is_truncation() {
        let (mut writer, mut reader) = pipe();
        writer.write_all(&[0, 0]).await.unwrap();
        drop(writer);
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Truncated)
        ));

        let (mut writer, mut reader) = pipe();
        writer.write_all(&5u32.to_be_bytes()).await.unwrap();
        writer.write_all(b"abc").await.unwrap();
        drop(writer);
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Truncated)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_read_keeps_what_it_already_read() {
        let (mut writer, mut reader) = pipe();
        writer.write_all(&6u32.to_be_bytes()[..3]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.next_frame())
                .await
                .is_err()
        );
        writer.write_all(&6u32.to_be_bytes()[3..]).await.unwrap();
        writer.write_all(b"hal").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.next_frame())
                .await
                .is_err()
        );
        writer.write_all(b"ves").await.unwrap();
        assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"halves");
    }

    #[tokio::test]
    async fn the_preface_round_trips_and_leaves_the_next_frame_in_place() {
        let (mut writer, mut reader) = pipe();
        let open = DirectOpen {
            token: DirectToken::generate(),
            class: LegClass::Blob,
        };
        write_direct_open(&mut writer, &open).await.unwrap();
        write_frame(&mut writer, b"noise").await.unwrap();

        assert_eq!(read_direct_open(&mut reader).await.unwrap(), open);
        assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"noise");
    }

    fn token_text() -> String {
        let token = serde_json::to_value(DirectToken::generate()).unwrap();
        token.as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn a_malformed_preface_is_refused_without_echoing_it() {
        let token = token_text();
        let secret = token_text();
        let (mut writer, mut reader) = pipe();
        let body = format!(r#"{{"token":"{token}","class":"{secret}"}}"#);
        assert!(
            serde_json::from_str::<DirectOpen>(&body)
                .unwrap_err()
                .to_string()
                .contains(&secret)
        );
        write_frame(&mut writer, body.as_bytes()).await.unwrap();

        let error = read_direct_open(&mut reader).await.unwrap_err();
        assert!(matches!(error, FrameError::Preface { .. }));
        assert!(!error.to_string().contains(&secret));
        assert!(!error.to_string().contains(&token));

        let (writer, mut reader) = pipe();
        drop(writer);
        assert!(matches!(
            read_direct_open(&mut reader).await,
            Err(FrameError::MissingPreface)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_never_prints_the_frame_it_is_assembling() {
        let open = DirectOpen {
            token: DirectToken::generate(),
            class: LegClass::Chat,
        };
        let preface = serde_json::to_vec(&open).unwrap();
        let (mut writer, mut reader) = pipe();
        let declared = u32::try_from(preface.len()).unwrap();
        writer.write_all(&declared.to_be_bytes()).await.unwrap();
        writer
            .write_all(&preface[..preface.len() - 1])
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.next_frame())
                .await
                .is_err()
        );

        let printed = format!("{reader:?}");
        let assembled = format!("{:?}", &preface[..preface.len() - 1]);
        assert!(printed.contains(&format!("body_filled: {}", preface.len() - 1)));
        assert!(!printed.contains(assembled.trim_matches(['[', ']'])));
    }
}
