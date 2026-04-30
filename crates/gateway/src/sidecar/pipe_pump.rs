//! Safe primitives for pumping a child process's stdout/stderr into
//! `tracing` without unbounded memory growth.
//!
//! `BufReader::lines()` has no per-line cap and would happily buffer
//! a multi-GB unbroken stream into one allocation before yielding —
//! a misbehaving sidecar dumping a no-newline blast OOMs the
//! supervisor before any application-level truncation runs. The
//! `read_capped_line` + `drain_until_newline` pair below uses
//! `fill_buf` / `consume` so the read buffer never exceeds the cap.
//!
//! Used today by `tool_ws::supervisor`. `sidecar::supervisor`
//! (channel side) still uses the unsafe `BufReader::lines()` shape;
//! it should migrate here when next touched.

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Hard cap on bytes accumulated into a single log line before we
/// force a flush.
pub(crate) const PIPE_LINE_READ_BUF_MAX: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineOutcome {
    /// Reached `\n` within the cap.
    Newline,
    /// Hit `PIPE_LINE_READ_BUF_MAX` first; caller flushes + drains.
    CapHit,
    /// EOF before any data on this iteration.
    Eof,
}

/// Read into `out` until either `\n` or `PIPE_LINE_READ_BUF_MAX`,
/// whichever comes first. Allocates strictly bounded by the cap.
pub(crate) async fn read_capped_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> std::io::Result<LineOutcome> {
    while out.len() < PIPE_LINE_READ_BUF_MAX {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Ok(if out.is_empty() {
                LineOutcome::Eof
            } else {
                LineOutcome::Newline
            });
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            let take = (pos + 1).min(PIPE_LINE_READ_BUF_MAX - out.len());
            out.extend_from_slice(&avail[..take]);
            reader.consume(take);
            return Ok(LineOutcome::Newline);
        }
        let take = avail.len().min(PIPE_LINE_READ_BUF_MAX - out.len());
        out.extend_from_slice(&avail[..take]);
        reader.consume(take);
    }
    Ok(LineOutcome::CapHit)
}

/// Discard input up to and including the next `\n`, so a cap-hit
/// flush can resume on a clean line boundary.
pub(crate) async fn drain_until_newline<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<()> {
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Ok(());
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let len = avail.len();
        reader.consume(len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn read_capped_line_returns_newline_when_within_cap() {
        let data = b"hello\nworld\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(data));
        let mut out = Vec::new();
        let outcome = read_capped_line(&mut reader, &mut out).await.unwrap();
        assert_eq!(outcome, LineOutcome::Newline);
        assert_eq!(out, b"hello\n");
    }

    #[tokio::test]
    async fn read_capped_line_caps_at_max_without_newline() {
        let data = vec![b'a'; PIPE_LINE_READ_BUF_MAX + 100];
        let mut reader = BufReader::new(Cursor::new(data));
        let mut out = Vec::new();
        let outcome = read_capped_line(&mut reader, &mut out).await.unwrap();
        assert_eq!(outcome, LineOutcome::CapHit);
        assert_eq!(out.len(), PIPE_LINE_READ_BUF_MAX);
    }

    #[tokio::test]
    async fn read_capped_line_returns_eof_on_empty() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let mut out = Vec::new();
        let outcome = read_capped_line(&mut reader, &mut out).await.unwrap();
        assert_eq!(outcome, LineOutcome::Eof);
    }

    #[tokio::test]
    async fn drain_until_newline_advances_past_first_nl() {
        let data = b"junk-without-cap\nrest\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(data));
        drain_until_newline(&mut reader).await.unwrap();
        let mut out = Vec::new();
        let outcome = read_capped_line(&mut reader, &mut out).await.unwrap();
        assert_eq!(outcome, LineOutcome::Newline);
        assert_eq!(out, b"rest\n");
    }
}
