//! Serve the assembled [`Router`] over plain HTTP/ws or rustls TLS (wss/https) —
//! in-process TLS termination so the service can be exposed directly, with no
//! reverse proxy. TLS is opt-in: pass `None` to serve plaintext, or a cert/key
//! pair to serve TLS.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use thiserror::Error;

/// PEM certificate + private-key paths for in-process TLS.
#[derive(Debug, Clone)]
pub(crate) struct TlsPaths {
    cert: PathBuf,
    key: PathBuf,
}

impl TlsPaths {
    /// Read a cert+key path pair from the named env vars: `None` if neither is
    /// set (serve plaintext `ws`), `Some` if both are (serve `wss`), an error if
    /// only one is (a half-configured TLS setup). An unset *or empty* value
    /// counts as "not set", so a compose file can leave the vars blank.
    pub(crate) fn from_env(cert_var: &str, key_var: &str) -> Result<Option<Self>, String> {
        let cert = std::env::var(cert_var).ok().filter(|s| !s.is_empty());
        let key = std::env::var(key_var).ok().filter(|s| !s.is_empty());
        match (cert, key) {
            (Some(cert), Some(key)) => Ok(Some(Self {
                cert: cert.into(),
                key: key.into(),
            })),
            (None, None) => Ok(None),
            _ => Err(format!("set both {cert_var} and {key_var}, or neither")),
        }
    }
}

/// Each variant names the listener (`addr`) it failed on — with the relay and
/// dashboard listeners running under one `try_join!`, either one's failure kills
/// both, and the fatal log line must say which address died.
#[derive(Debug, Error)]
pub(crate) enum ServeError {
    #[error("invalid bind address {addr}: {reason}")]
    BindAddr { addr: String, reason: String },
    #[error("listener {addr}: load TLS cert/key: {reason}")]
    Tls { addr: String, reason: String },
    #[error("listener {addr}: {source}")]
    Io {
        addr: String,
        source: std::io::Error,
    },
}

/// Bind `bind_addr` and serve `router` — over rustls when `tls` is `Some`, else
/// plaintext. Runs until the process is signalled.
pub(crate) async fn serve(
    bind_addr: &str,
    tls: Option<TlsPaths>,
    router: Router,
) -> Result<(), ServeError> {
    let io_err = |e: std::io::Error| ServeError::Io {
        addr: bind_addr.to_owned(),
        source: e,
    };
    let addr: SocketAddr =
        bind_addr
            .parse()
            .map_err(|e: std::net::AddrParseError| ServeError::BindAddr {
                addr: bind_addr.to_owned(),
                reason: e.to_string(),
            })?;
    // Serve with the client socket address attached (`ConnectInfo<SocketAddr>`)
    // so the relay's per-IP flood limiter can read the peer address. Behind a TLS
    // terminator this is the proxy's address — see the limiter's deployment note.
    match tls {
        Some(paths) => {
            // rustls 0.23 needs a process-wide crypto provider. Install the
            // aws-lc-rs one (already in the dependency graph via reqwest);
            // ignore the error if another part of the process installed it.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert, &paths.key)
                    .await
                    .map_err(|e| ServeError::Tls {
                        addr: bind_addr.to_owned(),
                        reason: e.to_string(),
                    })?;
            axum_server::bind_rustls(addr, config)
                .serve(router.into_make_service_with_connect_info::<SocketAddr>())
                .await
                .map_err(io_err)?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(addr).await.map_err(io_err)?;
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .map_err(io_err)?;
        }
    }
    Ok(())
}
