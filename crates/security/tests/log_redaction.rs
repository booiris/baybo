//! End-to-end test of `RedactingMakeWriter` wired into a real
//! `tracing-subscriber` fmt layer. This pins the contract that no
//! production log line ever carries an unredacted secret.

use std::io::Write;
use std::sync::{Arc, Mutex};

use aura_security::{LeakDetector, RedactingMakeWriter};
use tracing::{info, warn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> Self {
        Self::default()
    }

    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn install_redacting_subscriber(buf: SharedBuf) -> tracing::subscriber::DefaultGuard {
    let detector = Arc::new(LeakDetector::with_default_rules());
    let writer = RedactingMakeWriter::new(detector, buf);
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true);
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::set_default(subscriber)
}

#[test]
fn aws_key_in_info_event_is_redacted() {
    let buf = SharedBuf::new();
    let _guard = install_redacting_subscriber(buf.clone());

    info!(target: "aura.test", key = "AKIAIOSFODNN7EXAMPLE", "auth attempt");
    drop(_guard);

    let out = buf.contents();
    assert!(!out.is_empty(), "subscriber wrote nothing");
    assert!(
        !out.contains("AKIAIOSFODNN7EXAMPLE"),
        "raw AWS key leaked into log line: {out}"
    );
    assert!(
        out.contains("[{REDACTED_LOG_aws_access_key}]"),
        "expected redaction marker missing: {out}"
    );
}

#[test]
fn anthropic_key_inside_message_string_is_redacted() {
    let buf = SharedBuf::new();
    let _guard = install_redacting_subscriber(buf.clone());

    let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-ABC";
    warn!(target: "aura.test", "got secret {secret}");
    drop(_guard);

    let out = buf.contents();
    assert!(!out.contains(secret), "anthropic key leaked: {out}");
    assert!(
        out.contains("[{REDACTED_LOG_anthropic_api_key}]"),
        "missing redaction marker: {out}"
    );
}

#[test]
fn benign_log_passes_through_unchanged() {
    let buf = SharedBuf::new();
    let _guard = install_redacting_subscriber(buf.clone());

    info!(target: "aura.test", action = "boot", "starting up");
    drop(_guard);

    let out = buf.contents();
    assert!(out.contains("starting up"));
    assert!(out.contains("action=\"boot\""));
    assert!(!out.contains("REDACTED"));
}

#[test]
fn multiple_secrets_in_one_event_all_redacted() {
    let buf = SharedBuf::new();
    let _guard = install_redacting_subscriber(buf.clone());

    info!(
        target: "aura.test",
        aws = "AKIAIOSFODNN7EXAMPLE",
        gh = "ghp_1234567890abcdefghijklmnopqrstuvwxyz",
        "two leaks"
    );
    drop(_guard);

    let out = buf.contents();
    assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!out.contains("ghp_1234567890abcdefghijklmnopqrstuvwxyz"));
    assert!(out.contains("[{REDACTED_LOG_aws_access_key}]"));
    assert!(out.contains("[{REDACTED_LOG_github_token}]"));
}
