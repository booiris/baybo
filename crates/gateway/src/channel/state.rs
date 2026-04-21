//! State shared with the WS channel server.
//!
//! Threaded through the axum router used by [`crate::channel::route`] so
//! per-connection tasks can register a [`crate::channel::adapter::SidecarAdapter`]
//! on the workspace [`ChannelRegistry`], validate the caller's capability
//! token against the live [`ChannelTokenTable`], and forward decoded
//! frames onto the router's incoming mpsc.

use std::sync::Arc;

use aura_agent::SessionManager;
use aura_channels::{ChannelRegistry, IncomingMessage};
use aura_gateway_auth::ChannelTokenTable;
use tokio::sync::mpsc;

/// State passed to the `/v1/channel-ws` handler. Cheap to clone — every
/// field is an `Arc` or a clone-cheap handle.
#[derive(Clone)]
pub struct WsChannelState {
    pub registry: Arc<ChannelRegistry>,
    pub incoming_tx: mpsc::Sender<IncomingMessage>,
    pub tokens: ChannelTokenTable,
    pub session_manager: Arc<SessionManager>,
}
