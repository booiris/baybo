//! Channel TCP listener.
//!
//! Binds a loopback TCP socket (`127.0.0.1:<ephemeral>`), publishes
//! the chosen port to `<workspace>/state/channel.port` so sidecars can
//! discover it, and serves the channel router built by
//! [`crate::server::build_channel_router`]. 127.0.0.1 is hardcoded —
//! config cannot loosen it to `0.0.0.0`, so a fat-fingered bind
//! address can't leak the channel listener to the network.
//!
//! Auth lives in the axum middleware [`crate::auth::channel`]: every
//! caller presents `x-baybo-channel-token` and the middleware looks the
//! token up in [`ChannelTokenTable`]. This listener serves subprocess
//! and tool sidecars, whose tokens are minted at spawn time and handed
//! to the child via env var. The bundled TUI holds a channel token too,
//! but dials the admin listener's co-hosted `/v1/channel-ws` instead —
//! it has a config-known address and needs no port-file discovery.
//! Same-UID containment comes from the fact that both delivery channels
//! are owned by the gateway UID (child env vars and the sqlite database
//! holding the vault, kept owner-only by `baybo-storage`'s
//! `DB_FILE_MODE`), so port-level exposure on loopback is not itself a
//! threat — an attacker with the local UID has already won.
//!
//! Lifecycle:
//! * `<workspace>/state/channel.port` is written `0o600` after bind
//!   succeeds and removed on graceful + abnormal exit via a Drop
//!   guard on [`PortFileGuard`].
//! * Graceful shutdown is driven by the shared [`ShutdownSignal`] —
//!   once the signal fires the accept loop exits and in-flight
//!   requests drain up to the configured grace period.

use std::fs;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use baybo_agent::service::ShutdownSignal;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tower::Service;

use crate::auth::ChannelTokenTable;
use crate::auth::channel::ChannelAuthState;
use crate::server::{GatewayDeps, build_channel_router};
use crate::{GatewayError, Result};

/// Long-lived channel TCP server. Owns the bound [`TcpListener`] and
/// the `channel.port` discovery file (removed on drop).
pub struct ChannelServer {
    port_file: PortFileGuard,
    listener: TcpListener,
    router: Router,
    shutdown_grace: Duration,
}

impl ChannelServer {
    /// Bind on `127.0.0.1:0`, publish the chosen port to `port_file`,
    /// and assemble the channel router with the given token table.
    /// Always loopback — the bind address is not configurable.
    pub fn bind(deps: &GatewayDeps, port_file: PathBuf, tokens: ChannelTokenTable) -> Result<Self> {
        Self::bind_with_device_store(deps, port_file, tokens, None)
    }

    /// Like [`Self::bind`] but also wires the device registry so a presented
    /// device `auth_token` resolves to [`crate::auth::AuthedClient::Device`].
    /// (Production devices reach the admin listener, not this loopback one; this
    /// path exists so crate-level tests can exercise device channel auth on the
    /// proven WS-serving stack.)
    pub fn bind_with_device_store(
        deps: &GatewayDeps,
        port_file: PathBuf,
        tokens: ChannelTokenTable,
        device_store: Option<std::sync::Arc<dyn baybo_store::DeviceStore>>,
    ) -> Result<Self> {
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        // Use std bind first so we can read `local_addr` before
        // handing the fd to tokio. `TcpListener::from_std` wants a
        // non-blocking fd.
        let std_listener =
            std::net::TcpListener::bind(bind_addr).map_err(|e| GatewayError::Bind {
                addr: bind_addr.to_string(),
                reason: e.to_string(),
            })?;
        std_listener
            .set_nonblocking(true)
            .map_err(|e| GatewayError::Bind {
                addr: bind_addr.to_string(),
                reason: format!("set_nonblocking: {e}"),
            })?;
        let local_addr = std_listener.local_addr().map_err(|e| GatewayError::Bind {
            addr: bind_addr.to_string(),
            reason: format!("local_addr: {e}"),
        })?;
        let listener = TcpListener::from_std(std_listener).map_err(|e| GatewayError::Bind {
            addr: bind_addr.to_string(),
            reason: format!("register with tokio: {e}"),
        })?;

        let port_file = PortFileGuard::write(&port_file, local_addr.port())?;
        let mut auth_state = ChannelAuthState::new(tokens);
        if let Some(device_store) = device_store {
            auth_state = auth_state.with_device_store(device_store);
        }
        let router = build_channel_router(deps, auth_state);
        let shutdown_grace = deps.runtime_config.shutdown_grace;
        Ok(Self {
            port_file,
            listener,
            router,
            shutdown_grace,
        })
    }

    pub fn port(&self) -> u16 {
        self.port_file.port
    }

    pub fn port_file(&self) -> &Path {
        &self.port_file.path
    }

    /// Run until `shutdown` fires and in-flight connections drain.
    pub async fn run(self, shutdown: ShutdownSignal) -> Result<()> {
        tracing::info!(
            port = self.port_file.port,
            port_file = %self.port_file.path.display(),
            listener = "channel",
            "gateway listening",
        );
        let ChannelServer {
            port_file,
            listener,
            router,
            shutdown_grace,
        } = self;
        // Hold the guard until the accept loop + drain both finish,
        // so the port file survives for the full server lifetime.
        let _port_guard = port_file;

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
                            let svc = router.clone();
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

async fn serve_connection<R>(io: TokioIo<TcpStream>, router: R)
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
    let service = service_fn(move |req: hyper::Request<Incoming>| {
        let mut router = router.clone();
        async move { router.call(req).await }
    });
    let builder = AutoBuilder::new(TokioExecutor::new());
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        tracing::debug!(error = %e, listener = "channel", "connection closed with error");
    }
}

/// Owns the `channel.port` discovery file: writes it on bind, unlinks
/// it on drop. `0o600` so other UIDs on the machine can't even read
/// the port number — defense in depth; the port itself isn't secret,
/// but keeping the file same-UID-only matches the threat model.
struct PortFileGuard {
    path: PathBuf,
    port: u16,
}

impl PortFileGuard {
    fn write(path: &Path, port: u16) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| GatewayError::Bind {
                addr: path.display().to_string(),
                reason: format!("create parent dir: {e}"),
            })?;
        }
        // Best-effort unlink of any stale file from a prior run; a
        // living gateway on the same workspace is already prevented by
        // the singleton lock.
        let _ = fs::remove_file(path);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| GatewayError::Bind {
                addr: path.display().to_string(),
                reason: format!("create port file: {e}"),
            })?;
        writeln!(f, "{port}").map_err(|e| GatewayError::Bind {
            addr: path.display().to_string(),
            reason: format!("write port file: {e}"),
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            port,
        })
    }
}

impl Drop for PortFileGuard {
    fn drop(&mut self) {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "port file unlink failed",
                );
            }
        }
    }
}
