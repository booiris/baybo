//! Channel UDS listener.
//!
//! Binds a Unix domain socket under the per-workspace identity directory
//! (`<workspace>/channel.sock`), enforces `0o600` perms, and serves the
//! channel router built by [`crate::server::build_channel_router`]. The
//! accept loop extracts `SO_PEERCRED` from each connection and injects a
//! [`crate::auth_channel::PeerCred`] extension so the channel auth
//! middleware can verify the caller before any handler runs.
//!
//! Lifecycle:
//! * Stale socket files from prior runs are unlinked before `bind`.
//! * A `Drop` guard on the owning [`ChannelServer`] removes the socket
//!   file on graceful and abnormal exits.
//! * Graceful shutdown is driven by the shared [`ShutdownSignal`] — once
//!   the signal fires we stop accepting new connections and then wait up
//!   to the configured grace for in-flight requests to finish (the
//!   parent gateway arms a force-exit watchdog alongside this).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aura_agent::service::ShutdownSignal;
use aura_gateway_auth::ChannelTokenTable;
use axum::Router;
use axum::extract::Extension;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;
use tower::Service;

use crate::auth_channel::{ChannelAuthState, PeerCred};
use crate::server::{GatewayDeps, build_channel_router};
use crate::{GatewayError, Result};

/// Long-lived channel UDS server. Owns the bound [`UnixListener`] and
/// removes the socket file on drop.
pub struct ChannelServer {
    socket_path: PathBuf,
    listener: UnixListener,
    router: Router,
    shutdown_grace: Duration,
}

impl ChannelServer {
    /// Build a server against `socket_path`. Unlinks any stale file at
    /// that path, binds fresh, and chmods `0o600` so only the current
    /// UID can connect.
    pub fn bind(
        deps: &GatewayDeps,
        socket_path: PathBuf,
        psk: [u8; 32],
        tokens: ChannelTokenTable,
    ) -> Result<Self> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| GatewayError::Bind {
                addr: socket_path.display().to_string(),
                reason: format!("create parent dir: {e}"),
            })?;
        }
        // Best-effort unlink; ignore "not found", surface everything else.
        match std::fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GatewayError::Bind {
                    addr: socket_path.display().to_string(),
                    reason: format!("unlink stale socket: {e}"),
                });
            }
        }

        let listener = UnixListener::bind(&socket_path).map_err(|e| GatewayError::Bind {
            addr: socket_path.display().to_string(),
            reason: e.to_string(),
        })?;
        restrict_socket_perms(&socket_path)?;

        let expected_uid = current_uid();
        let auth_state = ChannelAuthState::new(psk, tokens, expected_uid);
        let router = build_channel_router(deps, auth_state);
        let shutdown_grace = deps.runtime_config.shutdown_grace;
        Ok(Self {
            socket_path,
            listener,
            router,
            shutdown_grace,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run until `shutdown` fires and in-flight connections drain (up to
    /// the configured grace period).
    pub async fn run(self, shutdown: ShutdownSignal) -> Result<()> {
        tracing::info!(
            socket = %self.socket_path.display(),
            listener = "channel",
            "gateway listening",
        );
        let ChannelServer {
            socket_path,
            listener,
            router,
            shutdown_grace,
        } = self;
        let _unlink_guard = UnlinkOnDrop(socket_path.clone());

        let mut conns = JoinSet::<()>::new();
        let shutdown_for_loop = shutdown.clone();

        loop {
            tokio::select! {
                biased;
                _ = shutdown_for_loop.wait() => {
                    tracing::info!(listener = "channel", "shutdown signal received");
                    break;
                }
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _addr)) => {
                            let peer = extract_peer_cred(&stream);
                            let svc = router.clone().layer(Extension(peer));
                            let io = TokioIo::new(stream);
                            conns.spawn(serve_connection(io, svc));
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, listener = "channel", "accept error");
                        }
                    }
                }
            }
        }

        // Drain with a grace period; force-exit watchdog at the CLI layer
        // enforces the hard ceiling.
        let drain = async { while conns.join_next().await.is_some() {} };
        if tokio::time::timeout(shutdown_grace, drain).await.is_err() {
            tracing::warn!(
                listener = "channel",
                "shutdown grace elapsed with connections still open",
            );
            conns.shutdown().await;
        }
        Ok(())
    }
}

async fn serve_connection<R>(io: TokioIo<UnixStream>, router: R)
where
    R: Service<
            hyper::Request<Incoming>,
            Response = axum::response::Response,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + 'static,
    R::Future: Send + 'static,
{
    let router = Arc::new(tokio::sync::Mutex::new(router));
    let service = service_fn(move |req: hyper::Request<Incoming>| {
        let router = Arc::clone(&router);
        async move {
            let mut guard = router.lock().await;
            guard.call(req).await
        }
    });
    let builder = AutoBuilder::new(TokioExecutor::new());
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!(error = %e, listener = "channel", "connection closed with error");
    }
}

fn extract_peer_cred(stream: &UnixStream) -> PeerCred {
    match stream.peer_cred() {
        Ok(ucred) => PeerCred {
            uid: ucred.uid(),
            pid: ucred.pid().map(|p| p as u32),
        },
        Err(e) => {
            tracing::warn!(error = %e, "peer_cred() failed; connection will fail auth");
            // Use a sentinel UID that cannot match any real uid check:
            // setting it to u32::MAX guarantees `peer.uid != expected_uid`
            // unless the gateway also runs as u32::MAX (impossible on
            // Linux — valid UIDs are < 2^32 - 1 but 0xFFFFFFFF is the
            // `(uid_t)-1` sentinel).
            PeerCred {
                uid: u32::MAX,
                pid: None,
            }
        }
    }
}

fn restrict_socket_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|e| GatewayError::Bind {
        addr: path.display().to_string(),
        reason: format!("chmod 0600: {e}"),
    })
}

fn current_uid() -> u32 {
    // Safe: getuid() is defined for every process and cannot fail.
    unsafe { libc::getuid() }
}

/// Unlinks the socket file on drop. Cheap; log but don't fail.
struct UnlinkOnDrop(PathBuf);

impl Drop for UnlinkOnDrop {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %self.0.display(), "socket unlink failed");
            }
        }
    }
}
