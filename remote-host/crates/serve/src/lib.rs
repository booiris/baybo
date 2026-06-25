//! Serve an axum [`Router`] over plain HTTP or rustls TLS — shared by the relay
//! and push binaries so each can terminate TLS in-process (wss/https) without a
//! separate reverse proxy. TLS is opt-in: pass `None` to serve plaintext (the
//! behind-a-terminator deployment), or a cert/key pair to serve TLS directly.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use thiserror::Error;

/// PEM certificate + private-key paths for in-process TLS.
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl TlsPaths {
    /// Read a cert+key path pair from the named env vars: `None` if neither is
    /// set (serve plaintext `ws`), `Some` if both are (serve `wss`), an error if
    /// only one is (a half-configured TLS setup). An unset *or empty* value
    /// counts as "not set", so a compose file can leave the vars blank.
    pub fn from_env(cert_var: &str, key_var: &str) -> Result<Option<Self>, String> {
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

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("invalid bind address {addr}: {reason}")]
    BindAddr { addr: String, reason: String },
    #[error("load TLS cert/key: {0}")]
    Tls(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Bind `bind_addr` and serve `router` — over rustls when `tls` is `Some`, else
/// plaintext. Runs until the process is signalled.
pub async fn serve(
    bind_addr: &str,
    tls: Option<TlsPaths>,
    router: Router,
) -> Result<(), ServeError> {
    let addr: SocketAddr =
        bind_addr
            .parse()
            .map_err(|e: std::net::AddrParseError| ServeError::BindAddr {
                addr: bind_addr.to_owned(),
                reason: e.to_string(),
            })?;
    match tls {
        Some(paths) => {
            // rustls 0.23 needs a process-wide crypto provider. Install the
            // aws-lc-rs one (already in the dependency graph via reqwest);
            // ignore the error if another part of the process installed it.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert, &paths.key)
                    .await
                    .map_err(|e| ServeError::Tls(e.to_string()))?;
            axum_server::bind_rustls(addr, config)
                .serve(router.into_make_service())
                .await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, router).await?;
        }
    }
    Ok(())
}
