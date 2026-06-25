//! The remote-host ("C") wire contract: route paths, the admission header, and
//! the serde types crossing the relay (WS) + push (HTTP) boundaries. Pure serde,
//! no transport and no baybo deps, so the server, the gateway, and the app all
//! depend on it instead of hand-mirroring the protocol.

pub mod push;
pub mod relay;

/// Join a base URL to a route path, trimming a trailing slash on the base.
fn join(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}
