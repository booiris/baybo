//! WS-backed glue the TUI uses to talk to the gateway.
//!
//! The TUI reaches the gateway's channel WebSocket endpoint exclusively
//! — messages, deltas, notices, and tool approvals all travel over a
//! single [`WsClient`]. Admin-shaped resources (skills, jobs, memory,
//! sessions CRUD) live on the admin TCP surface and are only reachable
//! through the `baybo` CLI.
//!
//! This module holds the TUI-facing adapters:
//!
//! * [`WsClient`] — private WS + MessagePack client against
//!   `/v1/channel-ws` (PSK-authenticated).
//! * [`WsTransport`] — [`TuiTransport`](crate::transport::TuiTransport)
//!   driven by the channel WS.
//! * [`TuiSlashHandler`] — slash commands that the WS surface can
//!   satisfy (`/clear`, `/quit`, dashboard shortcuts); other slashes
//!   pass through to the agent. Tool approvals are handled by the
//!   modal keybindings, not slash commands.
//! * [`TuiDashboardProvider`] — dashboard views rendered as admin-only
//!   placeholders, so keybindings keep working without misleading data.

pub mod dashboard;
pub mod slash;
pub mod transport;
pub mod ws;

pub use dashboard::TuiDashboardProvider;
pub use slash::TuiSlashHandler;
pub use transport::WsTransport;
pub use ws::WsClient;
