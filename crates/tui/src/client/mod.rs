//! Gateway client used by the TUI.
//!
//! The TUI talks exclusively to the gateway's channel UDS listener —
//! sessions, messages, approvals, and the SSE streams that carry model
//! output. Admin routes (status, config, jobs, …) are deliberately out
//! of reach; `aura cli` is the tool for those.
//!
//! The HTTP+SSE transport and its DTOs live in [`aura_channels::client`]
//! so third-party channel plugins share the same client. This module
//! only holds the TUI-facing glue ([`GatewayTransport`],
//! [`GatewaySlashHandler`], [`GatewayDashboardProvider`]) that adapts
//! that client onto the TUI's transport / slash / dashboard traits.

pub mod dashboard;
pub mod slash;
pub mod transport;

pub use aura_channels::client::{ClientError, ClientResult, GatewayClient, SseStream};
pub use aura_channels::{ApprovalEvent, SseEvent};
pub use dashboard::GatewayDashboardProvider;
pub use slash::GatewaySlashHandler;
pub use transport::GatewayTransport;
