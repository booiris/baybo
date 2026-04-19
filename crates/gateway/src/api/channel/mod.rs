//! Channel surface (UDS, peer-credential + PSK/token authenticated).
//! Hosts session CRUD, message submit/stream, and approvals — the
//! routes the TUI and future sidecar channel plugins talk to.

pub mod approvals;
pub mod messages;
pub mod sessions;

use axum::Router;

use crate::server::ChannelState;

/// Compose the channel v1 router. The caller wraps this in the channel
/// auth middleware and mounts it under `/v1`.
pub fn v1_router() -> Router<ChannelState> {
    Router::new()
        .merge(sessions::routes())
        .merge(messages::routes())
        .merge(approvals::routes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_v1_router_assembles_without_panic() {
        let _ = v1_router();
    }
}
