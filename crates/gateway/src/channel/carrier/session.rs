//! A carrier session from its first frame on: the `DirectOpen` token gate,
//! then the same Noise IK responder a relay leg runs: [`read_open`],
//! [`handle_authenticated_transport`] and [`FramedSource`].

use std::time::Duration;

use carrier::error::FrameError;
use carrier::framing::{FrameReader, read_direct_open, write_frame};
use carrier::kind::CarrierKind;
use remote_host_protocol::relay::{DirectOpen, DirectToken, LegClass};
use tokio::io::AsyncRead;
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::channel::api_tunnel::run_tunnel_session;
use crate::channel::device_content::{
    AuthenticatedDevice, BinarySink, BinarySource, RelaySessionError, run_content_session,
};
use crate::channel::state::{LegDedup, WsChannelState};

/// How long a new stream may take to deliver its `DirectOpen` preface.
pub(crate) const DIRECT_OPEN_DEADLINE: Duration = Duration::from_secs(1);

/// Reads a session's `DirectOpen` preface within `deadline` and checks its
/// token in constant time. Nothing else runs until it passes: a malformed
/// preface or a wrong token is an [`RelaySessionError::AuthRejected`], a
/// silent or closed stream a routine end.
pub(crate) async fn read_open<R>(
    frames: &mut FrameReader<R>,
    token: &DirectToken,
    deadline: Duration,
) -> Result<DirectOpen, RelaySessionError>
where
    R: AsyncRead + Unpin,
{
    let open = match tokio::time::timeout(deadline, read_direct_open(frames)).await {
        Err(_) => {
            return Err(RelaySessionError::Ended(
                "DirectOpen preface timed out".to_owned(),
            ));
        }
        Ok(Err(FrameError::MissingPreface)) => {
            return Err(RelaySessionError::Ended(
                "peer closed before the DirectOpen preface".to_owned(),
            ));
        }
        Ok(Err(FrameError::Io(error))) => {
            return Err(RelaySessionError::Ended(format!(
                "DirectOpen preface read failed: {error}"
            )));
        }
        Ok(Err(error)) => return Err(RelaySessionError::AuthRejected(error.to_string())),
        Ok(Ok(open)) => open,
    };
    if open.token != *token {
        return Err(RelaySessionError::AuthRejected(
            "wrong DirectOpen token".to_owned(),
        ));
    }
    Ok(open)
}

/// Runs an authenticated carrier session exactly as a relay leg of `class`
/// runs: Chat through the content responder, deduped by device with the
/// session task's own `AbortHandle` from `abort`, so whichever chat leg
/// installs last, on any carrier, wins; Api and Blob through the API tunnel.
/// Once Noise authenticates the device, the session is listed in the link
/// table as a leg riding `kind`.
pub(crate) async fn handle_authenticated_transport<Si, So>(
    sink: Si,
    source: So,
    class: LegClass,
    kind: Option<CarrierKind>,
    state: &WsChannelState,
    abort: oneshot::Receiver<AbortHandle>,
) -> Result<(), RelaySessionError>
where
    Si: BinarySink,
    So: BinarySource,
{
    let sink = state.device_links.tracked(sink, class, kind);
    match class {
        LegClass::Chat => {
            let dedup = abort.await.ok().map(|abort| LegDedup {
                registry: state.device_leg_registry.clone(),
                abort,
            });
            run_content_session(sink, source, state, dedup).await
        }
        LegClass::Api | LegClass::Blob => {
            drop(abort);
            run_tunnel_session(sink, source, class, state).await
        }
    }
}

/// Logs an ended carrier session. A rejected one logs at debug: an admitted
/// source is not an authenticated one, and a stale token is expected after a
/// restart. A gateway-side failure warns.
pub(crate) fn log_session_end(error: &RelaySessionError) {
    match error {
        RelaySessionError::AuthRejected(reason) => {
            tracing::debug!(%reason, "carrier session rejected")
        }
        RelaySessionError::Infra(reason) => {
            tracing::warn!(%reason, "carrier session: gateway-side handshake failure")
        }
        RelaySessionError::Ended(reason) => tracing::debug!(%reason, "carrier session ended"),
    }
}

/// Tells a carrier connection how one of its sessions' authentication ended:
/// `true` once Noise IK has authenticated the device, `false` if the session
/// ends first, however it ends.
pub(crate) struct AuthReport {
    outcome: mpsc::UnboundedSender<bool>,
    reported: bool,
}

impl AuthReport {
    pub(crate) fn new(outcome: mpsc::UnboundedSender<bool>) -> Self {
        Self {
            outcome,
            reported: false,
        }
    }

    fn authenticated(&mut self) {
        if !self.reported {
            self.reported = true;
            let _ = self.outcome.send(true);
        }
    }
}

impl Drop for AuthReport {
    fn drop(&mut self) {
        if !self.reported {
            let _ = self.outcome.send(false);
        }
    }
}

/// The send half of one QUIC stream, framed, and the report of its session's
/// authentication.
pub(crate) struct QuicBinarySink {
    pub(crate) stream: quinn::SendStream,
    pub(crate) report: AuthReport,
}

/// The receive half of one carrier session's QUIC stream, past its
/// `DirectOpen` preface.
pub(crate) struct FramedSource<R>(pub(crate) FrameReader<R>);

#[async_trait::async_trait]
impl BinarySink for QuicBinarySink {
    const CONFIRMS_HANDSHAKE: bool = true;

    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        write_frame(&mut self.stream, &bytes)
            .await
            .map_err(|error| tracing::debug!(%error, "carrier stream write failed"))
    }

    async fn close(&mut self) {
        let _ = self.stream.finish();
    }

    fn authenticated(&mut self, _device: &AuthenticatedDevice) -> Result<(), RelaySessionError> {
        self.report.authenticated();
        Ok(())
    }
}

#[async_trait::async_trait]
impl<R> BinarySource for FramedSource<R>
where
    R: AsyncRead + Unpin + Send,
{
    async fn next_bytes(&mut self) -> Option<Vec<u8>> {
        match self.0.next_frame().await {
            Ok(frame) => frame,
            Err(error) => {
                tracing::debug!(%error, "carrier session read ended");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use carrier::framing::{write_direct_open, write_frame};
    use tokio::io::{DuplexStream, duplex};

    use super::*;

    const PIPE_CAPACITY: usize = 4096;
    const SHORT: Duration = Duration::from_millis(20);

    fn pipe() -> (DuplexStream, FrameReader<DuplexStream>) {
        let (writer, reader) = duplex(PIPE_CAPACITY);
        (writer, FrameReader::new(reader))
    }

    #[tokio::test]
    async fn the_right_token_opens_the_session_and_leaves_the_next_frame() {
        let token = DirectToken::generate();
        let (mut writer, mut frames) = pipe();
        let open = DirectOpen {
            token: token.clone(),
            class: LegClass::Blob,
        };
        write_direct_open(&mut writer, &open).await.unwrap();
        write_frame(&mut writer, b"noise").await.unwrap();
        assert_eq!(read_open(&mut frames, &token, SHORT).await.unwrap(), open);
        assert_eq!(frames.next_frame().await.unwrap().unwrap(), b"noise");
    }

    #[tokio::test]
    async fn a_wrong_token_or_a_malformed_preface_is_rejected_before_noise() {
        let token = DirectToken::generate();
        let (mut writer, mut frames) = pipe();
        let wrong = DirectOpen {
            token: DirectToken::generate(),
            class: LegClass::Chat,
        };
        write_direct_open(&mut writer, &wrong).await.unwrap();
        assert!(matches!(
            read_open(&mut frames, &token, SHORT).await,
            Err(RelaySessionError::AuthRejected(_))
        ));

        let (mut writer, mut frames) = pipe();
        write_frame(&mut writer, b"{not json").await.unwrap();
        assert!(matches!(
            read_open(&mut frames, &token, SHORT).await,
            Err(RelaySessionError::AuthRejected(_))
        ));
    }

    #[tokio::test]
    async fn a_silent_or_closed_stream_ends_without_authenticating() {
        let token = DirectToken::generate();
        let (_writer, mut frames) = pipe();
        assert!(matches!(
            read_open(&mut frames, &token, SHORT).await,
            Err(RelaySessionError::Ended(_))
        ));

        let (writer, mut frames) = pipe();
        drop(writer);
        assert!(matches!(
            read_open(&mut frames, &token, SHORT).await,
            Err(RelaySessionError::Ended(_))
        ));
    }
}
