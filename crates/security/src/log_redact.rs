//! `MakeWriter` adapter that scrubs secrets out of formatted log events
//! before they hit the file appender.
//!
//! The tracing fmt layer calls `write_all` with one formatted record at a
//! time, so we can safely scan and substitute per-call. The output marker
//! `[{REDACTED_LOG_<rule>}]` is intentionally distinct from the vault's
//! reversible `[{REDACTED_SECRET_<hex>}]` placeholder — log redaction is
//! one-way and carries no vault entry.

use std::borrow::Cow;
use std::io;
use std::sync::Arc;

use crate::{LeakAction, LeakDetector};
use tracing_subscriber::fmt::MakeWriter;

pub struct RedactingMakeWriter<W> {
    detector: Arc<LeakDetector>,
    inner: W,
}

impl<W> RedactingMakeWriter<W> {
    pub fn new(detector: Arc<LeakDetector>, inner: W) -> Self {
        Self { detector, inner }
    }
}

impl<'a, W> MakeWriter<'a> for RedactingMakeWriter<W>
where
    W: MakeWriter<'a>,
{
    type Writer = RedactingWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            detector: Arc::clone(&self.detector),
            inner: self.inner.make_writer(),
        }
    }
}

pub struct RedactingWriter<W> {
    detector: Arc<LeakDetector>,
    inner: W,
}

impl<W: io::Write> io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Ok(text) = std::str::from_utf8(buf) else {
            return self.inner.write(buf);
        };
        match apply_redaction(&self.detector, text) {
            Cow::Borrowed(_) => self.inner.write_all(buf)?,
            Cow::Owned(s) => self.inner.write_all(s.as_bytes())?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn apply_redaction<'a>(detector: &LeakDetector, text: &'a str) -> Cow<'a, str> {
    let mut out: Cow<'a, str> = Cow::Borrowed(text);
    for rule in detector.rules() {
        if !matches!(rule.action, LeakAction::Replace) {
            continue;
        }
        let replacement = format!("[{{REDACTED_LOG_{}}}]", rule.name);
        if let Cow::Owned(s) = rule.pattern.replace_all(&out, replacement.as_str()) {
            out = Cow::Owned(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct CapturingWriter(Vec<u8>);

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn redactor() -> RedactingWriter<CapturingWriter> {
        RedactingWriter {
            detector: Arc::new(LeakDetector::with_default_rules()),
            inner: CapturingWriter(Vec::new()),
        }
    }

    #[test]
    fn redacts_aws_access_key_in_log_line() {
        let mut w = redactor();
        let line = b"2026-04-18T12:00:00Z INFO baybo: key=AKIAIOSFODNN7EXAMPLE done\n";
        w.write_all(line).unwrap();
        let out = String::from_utf8(w.inner.0).unwrap();
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(out.contains("[{REDACTED_LOG_aws_access_key}]"));
    }

    #[test]
    fn redacts_anthropic_api_key() {
        let mut w = redactor();
        let line = b"auth sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-ABC tail\n";
        w.write_all(line).unwrap();
        let out = String::from_utf8(w.inner.0).unwrap();
        assert!(!out.contains("sk-ant-api03-"));
        assert!(out.contains("[{REDACTED_LOG_anthropic_api_key}]"));
    }

    #[test]
    fn clean_line_avoids_allocation() {
        let detector = LeakDetector::with_default_rules();
        let text = "Hello, how can I help you today?";
        let out = apply_redaction(&detector, text);
        assert!(matches!(out, Cow::Borrowed(_)));
    }

    #[test]
    fn passes_through_non_utf8_bytes() {
        let mut w = redactor();
        let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
        assert_eq!(w.write(bytes).unwrap(), bytes.len());
        assert_eq!(w.inner.0, bytes);
    }

    #[test]
    fn empty_detector_is_pass_through() {
        let mut w = RedactingWriter {
            detector: Arc::new(LeakDetector::new()),
            inner: CapturingWriter(Vec::new()),
        };
        let line = b"secret sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-ABC\n";
        w.write_all(line).unwrap();
        assert_eq!(w.inner.0, line);
    }
}
