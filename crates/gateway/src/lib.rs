//! Aura HTTP gateway.
//!
//! Runs an axum server that both
//!
//! 1. implements [`aura_channels::ChannelAdapter`] for `ChannelType::Http`
//!    so chat traffic flows through the normal Router path, and
//! 2. exposes an admin REST + SSE API mirroring the CLI surface.
//!
//! The gateway is driven by the CLI command tree `aura gateway ...`:
//! `start` runs the server in the foreground, `install` writes a platform
//! service unit, `enable` mints the auth token (if absent) and marks the
//! service to autostart at boot.

pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod http_adapter;
pub mod installer;
pub mod server;

pub use crate::auth::GatewayToken;
pub use crate::config::RuntimeGatewayConfig;
pub use crate::error::{GatewayError, Result};
pub use crate::http_adapter::{ApprovalEvent, HttpAdapter, SseEvent};
pub use crate::installer::{
    InstallContext, InstallerError, ServiceInstaller, ServiceStatus, for_current_platform,
};
pub use crate::server::{GatewayDeps, GatewayServer};
