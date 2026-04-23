//! Aura HTTP gateway.
//!
//! Exposes two listeners:
//!
//! * **Admin** — TCP, bearer-token authenticated. Config / status /
//!   jobs / cron / memory / traces / skills / tools / llm and a
//!   read-only channel list. Surfaces the same data the CLI does; no
//!   chat content flows through.
//! * **Channel** — Unix domain socket, peer-credential +
//!   PSK/token authenticated. Hosts a single WebSocket endpoint
//!   (`/v1/channel-ws`) over which the bundled TUI and sidecar channel
//!   plugins exchange [`aura_channels::wire::Frame`]s (MessagePack).
//!
//! The gateway is driven by the CLI command tree `aura gateway ...`:
//! `start` runs both listeners in the foreground; `install` writes a
//! platform service unit; `enable` mints the admin token (if absent) and
//! marks the service to autostart at boot.

pub mod api;
pub mod auth_admin;
pub mod auth_channel;
pub mod channel;
pub mod channel_listener;
pub mod config;
pub mod error;
pub mod installer;
pub mod log_buffer;
pub mod server;
pub mod sidecar;
pub mod spawn;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use crate::auth_admin::AdminToken;
pub use crate::channel::{ChannelControlError, ChannelControlRegistry};
pub use crate::channel_listener::ChannelServer;
pub use crate::config::RuntimeGatewayConfig;
pub use crate::error::{GatewayError, Result};
pub use crate::installer::{
    InstallContext, InstallerError, ServiceInstaller, ServiceStatus, for_current_platform,
};
pub use crate::log_buffer::{LogBuffer, LogBufferLayer, LogLevel, LogPage, LogQuery, LogRecord};
pub use crate::server::{GatewayDeps, GatewayServer};
pub use crate::sidecar::{SidecarError, SidecarRuntime, SidecarSupervisor};
pub use crate::spawn::{ChannelSpawner, ChildHandle};
