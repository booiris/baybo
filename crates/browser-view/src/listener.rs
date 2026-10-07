//! The gateway's unix-socket listener for the browser sidecar's link.
//!
//! The socket's directory is `0700` and the socket `0600`, the peer's uid
//! must be ours, and the first message must be a `Hello` carrying the
//! one-time secret and our protocol version — all within
//! [`HELLO_TIMEOUT`]. First wins: while one link is live every other
//! `Hello` is refused with `already_connected`.
//!
//! There is no link-level liveness probe. A dead peer closes its socket and
//! frees the slot at once; a *wedged* one (event loop stuck, socket open) is
//! not detectable by a write — the kernel buffer accepts it — and needs a
//! protocol-level ping the contract does not have. What bounds that case is
//! the MCP reconciler: it respawns a stalled sidecar and kills the old
//! process, whose socket then closes. Until then the new sidecar sees
//! `already_connected` and retries with backoff.
//!
//! Every per-link task lives in the listener's `JoinSet`, so the listener
//! task (which the gateway tracks) ending — on shutdown or abort — ends them
//! too.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::codec::{GatewayLinkCodec, LinkMessage};
use crate::error::BrowserViewError;
use crate::hub::{HubShared, LinkLease};
use crate::limits::{BROWSER_LINK_PROTOCOL_VERSION, HELLO_TIMEOUT};
use crate::params::current_uid;
use crate::wire::{HelloRejectReason, LinkDown, LinkSecret, LinkUp};

const SOCKET_DIR_MODE: u32 = 0o700;
const SOCKET_FILE_MODE: u32 = 0o600;

/// Mode bits compared when checking a directory is private.
const PERMISSION_BITS: u32 = 0o777;

/// Pause after a failed `accept` (e.g. fd exhaustion) before retrying.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(250);

/// Longest a single gateway → sidecar write may take before the link is
/// considered stuck and dropped.
const LINK_SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A bound link socket. Dropping it unlinks the socket file.
pub(crate) struct LinkListener {
    listener: UnixListener,
    path: PathBuf,
}

impl LinkListener {
    /// Prepare the private directory, unlink a stale socket and bind.
    /// Must run inside a tokio runtime.
    pub(crate) fn bind(path: &Path) -> Result<Self, BrowserViewError> {
        let dir = path.parent().ok_or_else(|| BrowserViewError::SocketDir {
            path: path.display().to_string(),
            reason: "socket path has no parent directory".into(),
        })?;
        prepare_private_dir(dir)?;
        let bind_err = |reason: String| BrowserViewError::Bind {
            path: path.display().to_string(),
            reason,
        };
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(bind_err(format!("remove stale socket: {e}"))),
        }
        let listener = UnixListener::bind(path).map_err(|e| bind_err(e.to_string()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_FILE_MODE))
            .map_err(|e| bind_err(format!("chmod socket: {e}")))?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub(crate) async fn run(
        self,
        hub: Arc<HubShared>,
        secret: LinkSecret,
        shutdown: CancellationToken,
    ) {
        let mut links = JoinSet::new();
        loop {
            let accepted = tokio::select! {
                _ = shutdown.cancelled() => break,
                Some(_) = links.join_next() => continue,
                accepted = self.listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, _)) => {
                    links.spawn(serve_link(
                        stream,
                        Arc::clone(&hub),
                        secret.clone(),
                        shutdown.clone(),
                    ));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "browser link: accept failed");
                    tokio::select! {
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                    }
                }
            }
        }
        // Every link task watches `shutdown` too; waiting lets each drop its
        // lease (marking the link down) before the socket file goes.
        while links.join_next().await.is_some() {}
    }
}

impl Drop for LinkListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn prepare_private_dir(dir: &Path) -> Result<(), BrowserViewError> {
    let dir_err = |reason: String| BrowserViewError::SocketDir {
        path: dir.display().to_string(),
        reason,
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(SOCKET_DIR_MODE)
        .create(dir)
        .map_err(|e| dir_err(e.to_string()))?;
    let meta = fs::symlink_metadata(dir).map_err(|e| dir_err(e.to_string()))?;
    if !meta.is_dir() {
        return Err(dir_err("not a directory".into()));
    }
    if meta.uid() != current_uid() {
        return Err(dir_err(format!("owned by uid {}, not us", meta.uid())));
    }
    if meta.mode() & PERMISSION_BITS != SOCKET_DIR_MODE {
        fs::set_permissions(dir, fs::Permissions::from_mode(SOCKET_DIR_MODE))
            .map_err(|e| dir_err(format!("chmod: {e}")))?;
    }
    Ok(())
}

type LinkFramed = Framed<UnixStream, GatewayLinkCodec>;

async fn serve_link(
    stream: UnixStream,
    hub: Arc<HubShared>,
    secret: LinkSecret,
    shutdown: CancellationToken,
) {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == current_uid() => {}
        Ok(cred) => {
            tracing::warn!(
                peer_uid = cred.uid(),
                "browser link: refusing a foreign uid"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "browser link: could not read peer credentials");
            return;
        }
    }
    let mut framed = Framed::new(stream, GatewayLinkCodec);
    let first = tokio::select! {
        _ = shutdown.cancelled() => return,
        first = tokio::time::timeout(HELLO_TIMEOUT, framed.next()) => first,
    };
    let (protocol, presented, pid) = match first {
        Ok(Some(Ok(LinkMessage::Json(LinkUp::Hello {
            protocol,
            secret,
            pid,
            ..
        })))) => (protocol, secret, pid),
        Ok(Some(Ok(_))) => {
            tracing::warn!("browser link: first message was not a hello");
            reject(&mut framed, HelloRejectReason::Unauthorized).await;
            return;
        }
        Ok(Some(Err(e))) => {
            tracing::warn!(error = %e, "browser link: bad framing before hello");
            return;
        }
        Ok(None) => return,
        Err(_) => {
            tracing::warn!("browser link: no hello in time");
            return;
        }
    };
    if !presented.ct_eq(&secret) {
        tracing::warn!(pid, "browser link: wrong secret");
        reject(&mut framed, HelloRejectReason::Unauthorized).await;
        return;
    }
    if protocol != BROWSER_LINK_PROTOCOL_VERSION {
        tracing::warn!(
            pid,
            protocol,
            expected = BROWSER_LINK_PROTOCOL_VERSION,
            "browser link: protocol mismatch"
        );
        reject(&mut framed, HelloRejectReason::ProtocolMismatch).await;
        return;
    }
    let Some(lease) = hub.claim_link() else {
        tracing::warn!(
            pid,
            "browser link: refusing a second link while one is live"
        );
        reject(&mut framed, HelloRejectReason::AlreadyConnected).await;
        return;
    };
    if !send(&mut framed, LinkDown::hello_ack()).await {
        return;
    }
    tracing::info!(pid, "browser link: sidecar connected");
    run_link(framed, lease, shutdown).await;
    tracing::info!(pid, "browser link: sidecar disconnected");
}

async fn reject(framed: &mut LinkFramed, reason: HelloRejectReason) {
    send(framed, LinkDown::HelloReject { reason }).await;
}

async fn send(framed: &mut LinkFramed, msg: LinkDown) -> bool {
    match tokio::time::timeout(LINK_SEND_TIMEOUT, framed.send(msg)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "browser link: send failed");
            false
        }
        Err(_) => {
            tracing::warn!("browser link: send timed out");
            false
        }
    }
}

/// Drive one accepted link until either side ends it. Holding `lease`
/// keeps the link marked up; dropping it on return marks it down.
async fn run_link(mut framed: LinkFramed, mut lease: LinkLease, shutdown: CancellationToken) {
    let mut streaming = false;
    loop {
        let wanted = *lease.want_stream().borrow_and_update();
        if wanted != streaming {
            let command = if wanted {
                LinkDown::StartScreencast
            } else {
                LinkDown::StopScreencast
            };
            if !send(&mut framed, command).await {
                return;
            }
            tracing::debug!(streaming = wanted, "browser link: screencast toggled");
            streaming = wanted;
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            changed = lease.want_stream().changed() => {
                if changed.is_err() {
                    return;
                }
            }
            msg = framed.next() => match msg {
                Some(Ok(msg)) => lease.on_message(msg),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "browser link: framing error; dropping link");
                    return;
                }
                None => return,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::codec::{SidecarLinkCodec, SidecarMessage};
    use crate::hub::{
        BrowserLinkConfig, BrowserViewHub, BrowserViewHubConfig, BrowserViewer, ViewEvent,
    };
    use crate::limits::MAX_SCREENCAST_FRAME_BYTES;
    use crate::params::BrowserLinkParams;
    use crate::wire::{BootId, Capability, FrameHeader, TargetId, ViewerDown};

    const WAIT: Duration = Duration::from_secs(5);

    type Sidecar = Framed<UnixStream, SidecarLinkCodec>;

    struct Fixture {
        _dir: tempfile::TempDir,
        params: BrowserLinkParams,
        viewer: BrowserViewer,
        shutdown: CancellationToken,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let params = BrowserLinkParams::resolve(dir.path()).unwrap();
        let shutdown = CancellationToken::new();
        let hub = BrowserViewHub::from_config(BrowserViewHubConfig {
            link: BrowserLinkConfig::Listen(params.clone()),
            shutdown: shutdown.clone(),
        });
        assert!(hub.link.is_some(), "listener bound");
        Fixture {
            _dir: dir,
            params,
            viewer: hub.viewer,
            shutdown,
        }
    }

    fn hello(secret: &LinkSecret, protocol: u32) -> SidecarMessage {
        SidecarMessage::Json(LinkUp::Hello {
            protocol,
            secret: secret.clone(),
            boot_id: BootId::new("boot"),
            pid: 1,
            capabilities: vec![Capability::Screencast],
        })
    }

    async fn connect(params: &BrowserLinkParams) -> Sidecar {
        let stream = UnixStream::connect(params.socket()).await.unwrap();
        Framed::new(stream, SidecarLinkCodec)
    }

    async fn recv(sidecar: &mut Sidecar) -> Option<LinkDown> {
        tokio::time::timeout(WAIT, sidecar.next())
            .await
            .expect("link message in time")
            .map(|r| r.unwrap())
    }

    async fn handshake(
        params: &BrowserLinkParams,
        secret: &LinkSecret,
        protocol: u32,
    ) -> (Sidecar, Option<LinkDown>) {
        let mut sidecar = connect(params).await;
        sidecar.send(hello(secret, protocol)).await.unwrap();
        let reply = recv(&mut sidecar).await;
        (sidecar, reply)
    }

    async fn view(sub: &mut crate::hub::ViewSubscription) -> ViewEvent {
        tokio::time::timeout(WAIT, sub.recv())
            .await
            .expect("view event")
    }

    #[tokio::test]
    async fn socket_and_dir_are_private() {
        let f = fixture();
        let sock = fs::metadata(f.params.socket()).unwrap();
        assert_eq!(sock.mode() & PERMISSION_BITS, SOCKET_FILE_MODE);
        let dir = fs::metadata(f.params.socket().parent().unwrap()).unwrap();
        assert_eq!(dir.mode() & PERMISSION_BITS, SOCKET_DIR_MODE);
    }

    #[tokio::test]
    async fn loose_existing_dir_is_tightened_and_stale_socket_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let params = BrowserLinkParams::resolve(dir.path()).unwrap();
        let sock_dir = params.socket().parent().unwrap();
        fs::create_dir_all(sock_dir).unwrap();
        fs::set_permissions(sock_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(params.socket(), b"stale").unwrap();
        let listener = LinkListener::bind(params.socket()).unwrap();
        assert_eq!(
            fs::metadata(sock_dir).unwrap().mode() & PERMISSION_BITS,
            SOCKET_DIR_MODE
        );
        drop(listener);
        assert!(!params.socket().exists(), "socket unlinked on drop");
    }

    #[tokio::test]
    async fn bind_failure_leaves_no_link_and_no_reason() {
        let dir = tempfile::tempdir().unwrap();
        let params = BrowserLinkParams::resolve(dir.path()).unwrap();
        fs::write(params.socket().parent().unwrap(), b"not a dir").unwrap();
        let hub = BrowserViewHub::from_config(BrowserViewHubConfig {
            link: BrowserLinkConfig::Listen(params),
            shutdown: CancellationToken::new(),
        });
        assert!(hub.link.is_none(), "no link env for the sidecar");
        assert!(hub.listener_task.is_none());
        let mut sub = hub.viewer.subscribe().unwrap();
        assert_eq!(
            view(&mut sub).await,
            ViewEvent::State(ViewerDown::link(false, 0, None))
        );
    }

    #[tokio::test]
    async fn wrong_secret_is_unauthorized() {
        let f = fixture();
        let (_s, reply) = handshake(
            &f.params,
            &LinkSecret::new("nope"),
            BROWSER_LINK_PROTOCOL_VERSION,
        )
        .await;
        assert_eq!(
            reply,
            Some(LinkDown::HelloReject {
                reason: HelloRejectReason::Unauthorized
            })
        );
    }

    #[tokio::test]
    async fn wrong_protocol_is_rejected() {
        let f = fixture();
        let (_s, reply) = handshake(
            &f.params,
            f.params.secret(),
            BROWSER_LINK_PROTOCOL_VERSION + 1,
        )
        .await;
        assert_eq!(
            reply,
            Some(LinkDown::HelloReject {
                reason: HelloRejectReason::ProtocolMismatch
            })
        );
    }

    #[tokio::test]
    async fn non_hello_first_is_unauthorized() {
        let f = fixture();
        let mut sidecar = connect(&f.params).await;
        sidecar
            .send(SidecarMessage::Json(LinkUp::Availability {
                unavailable: None,
            }))
            .await
            .unwrap();
        assert_eq!(
            recv(&mut sidecar).await,
            Some(LinkDown::HelloReject {
                reason: HelloRejectReason::Unauthorized
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_peer_is_dropped_after_hello_timeout() {
        let f = fixture();
        let mut sidecar = connect(&f.params).await;
        let closed = tokio::time::timeout(HELLO_TIMEOUT * 2, sidecar.next()).await;
        assert!(matches!(closed, Ok(None)), "{closed:?}");
    }

    #[tokio::test]
    async fn first_link_wins_and_next_is_accepted_after_close_with_start_replay() {
        let f = fixture();
        let mut sub = f.viewer.subscribe().unwrap();
        assert_eq!(
            view(&mut sub).await,
            ViewEvent::State(ViewerDown::link(false, 0, None))
        );

        let (mut first, reply) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        assert!(
            matches!(reply, Some(LinkDown::HelloAck { .. })),
            "{reply:?}"
        );
        assert_eq!(recv(&mut first).await, Some(LinkDown::StartScreencast));
        assert_eq!(
            view(&mut sub).await,
            ViewEvent::State(ViewerDown::link(true, 1, None))
        );

        let (_second, reply) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        assert_eq!(
            reply,
            Some(LinkDown::HelloReject {
                reason: HelloRejectReason::AlreadyConnected
            })
        );

        drop(first);
        assert_eq!(
            view(&mut sub).await,
            ViewEvent::State(ViewerDown::link(false, 1, None))
        );

        let (mut third, reply) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        assert!(
            matches!(reply, Some(LinkDown::HelloAck { .. })),
            "{reply:?}"
        );
        assert_eq!(
            recv(&mut third).await,
            Some(LinkDown::StartScreencast),
            "Start replayed to the new link because a viewer is waiting"
        );
        assert_eq!(
            view(&mut sub).await,
            ViewEvent::State(ViewerDown::link(true, 2, None))
        );
    }

    #[tokio::test]
    async fn start_and_stop_sent_exactly_once_per_edge() {
        let f = fixture();
        let (mut sidecar, reply) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        assert!(matches!(reply, Some(LinkDown::HelloAck { .. })));
        let a = f.viewer.subscribe().unwrap();
        let b = f.viewer.subscribe().unwrap();
        assert_eq!(recv(&mut sidecar).await, Some(LinkDown::StartScreencast));
        drop(a);
        drop(b);
        assert_eq!(recv(&mut sidecar).await, Some(LinkDown::StopScreencast));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), sidecar.next())
                .await
                .is_err(),
            "no further commands"
        );
    }

    #[tokio::test]
    async fn frames_reach_viewers_verbatim() {
        let f = fixture();
        let mut sub = f.viewer.subscribe().unwrap();
        let (mut sidecar, _) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        assert_eq!(recv(&mut sidecar).await, Some(LinkDown::StartScreencast));
        let header = FrameHeader {
            target_id: TargetId::new("T1"),
            browser_gen: 0,
            seq: 1,
            captured_at_ms: 1.0,
            device_width: 800.0,
            device_height: 600.0,
            offset_top: 0.0,
            page_scale_factor: 1.0,
            scroll_offset_x: 0.0,
            scroll_offset_y: 0.0,
        };
        let jpeg = Bytes::from_static(b"\xFF\xD8jpeg\xFF\xD9");
        sidecar
            .send(SidecarMessage::Frame {
                header: header.clone(),
                jpeg: jpeg.clone(),
            })
            .await
            .unwrap();
        let mut expected = Vec::new();
        let header_json = serde_json::to_vec(&header).unwrap();
        expected.extend_from_slice(&(header_json.len() as u32).to_be_bytes());
        expected.extend_from_slice(&header_json);
        expected.extend_from_slice(&jpeg);
        loop {
            match view(&mut sub).await {
                ViewEvent::Frame(bytes) => {
                    assert_eq!(bytes.as_ref(), expected.as_slice());
                    break;
                }
                ViewEvent::State(_) => {}
                ViewEvent::Shutdown => panic!("unexpected shutdown"),
            }
        }
    }

    #[tokio::test]
    async fn oversized_frame_drops_the_link() {
        let f = fixture();
        let mut sub = f.viewer.subscribe().unwrap();
        let (mut sidecar, _) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        assert_eq!(recv(&mut sidecar).await, Some(LinkDown::StartScreencast));
        let mut raw = Vec::new();
        raw.extend_from_slice(&((MAX_SCREENCAST_FRAME_BYTES * 2) as u32).to_be_bytes());
        raw.push(1);
        tokio::io::AsyncWriteExt::write_all(sidecar.get_mut(), &raw)
            .await
            .unwrap();
        loop {
            if view(&mut sub).await == ViewEvent::State(ViewerDown::link(false, 1, None)) {
                break;
            }
        }
        assert!(
            recv(&mut sidecar).await.is_none(),
            "gateway closed the link"
        );
    }

    #[tokio::test]
    async fn shutdown_closes_the_link() {
        let f = fixture();
        let (mut sidecar, _) =
            handshake(&f.params, f.params.secret(), BROWSER_LINK_PROTOCOL_VERSION).await;
        f.shutdown.cancel();
        assert!(recv(&mut sidecar).await.is_none());
    }
}
