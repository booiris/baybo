//! Baybo HTTP gateway.
//!
//! Exposes two listeners:
//!
//! * **Admin** — TCP, bearer-token authenticated. Config / status /
//!   turns / cron / memory / traces / skills / tools / llm and a
//!   read-only channel list. Also co-hosts admin-token web chat routes
//!   (`/v1/channel-ws`, `/v1/blobs/*`) so browser clients can reach
//!   them over the public bind.
//! * **Channel** — loopback TCP (`127.0.0.1:<ephemeral>`),
//!   vault-issued channel-token authenticated. Hosts a single
//!   WebSocket endpoint (`/v1/channel-ws`) over which the bundled
//!   TUI and sidecar channel plugins exchange
//!   [`baybo_channels::wire::Frame`]s (MessagePack).
//!
//! The gateway is driven by the CLI command tree `baybo gateway ...`:
//! `start` runs both listeners in the foreground; `install` writes a
//! platform service unit; `enable` mints the admin token (if absent) and
//! enables and starts the service; `restart` and `disable` manage the
//! running service lifecycle.

pub mod api;
pub mod auth;
pub mod channel;
pub mod channel_listener;
pub mod config;
pub mod deck_events;
pub mod device;
pub mod error;
pub mod installer;
pub mod log_buffer;
pub mod project_events;
pub mod push;
pub mod relay;
pub mod reload;
pub mod server;
pub mod sidecar;
pub mod spawn;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use crate::auth::{
    AdminToken, CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity, TOOL_CLIENT_LABEL_PREFIX,
    TUI_CLIENT_LABEL, TUI_TOKEN_VAULT_KEY, TokenHandle, constant_time_eq, generate_token,
};
pub use crate::channel::{ChannelControlError, ChannelControlRegistry};
// Self-contained relay pairing for `baybo device pair`: host a `/pair/host`
// leg and run the XXpsk0 handshake without a running gateway daemon.
pub use crate::channel::device_pair::PairingHostDeps;
pub use crate::channel::relay_pair::host_pairing_leg;
pub use crate::channel_listener::ChannelServer;
pub use crate::config::RuntimeGatewayConfig;
pub use crate::error::{GatewayError, Result};
pub use crate::installer::{
    InstallContext, InstallerError, ServiceInstaller, ServiceStatus, for_current_platform,
};
pub use crate::log_buffer::{LogBuffer, LogBufferLayer, LogLevel, LogPage, LogQuery, LogRecord};
pub use crate::reload::{ConfigReloader, ReloadError, ReloadOutcome};
pub use crate::server::{GatewayDeps, GatewayServer, spawn_relay_content};
pub use crate::sidecar::{
    BUN_BINARY_ENV, NODE_BINARY_ENV, SidecarError, SidecarRuntime, SidecarSupervisor,
    collect_profiles, node_binary,
};
pub use crate::spawn::{ChannelSpawner, ChildHandle, SIDECAR_ENV_ALLOWLIST};

/// Longest remote-host response-body excerpt carried into a log/error string —
/// enough to state a reject reason without dumping an arbitrary payload.
const HTTP_BODY_SNIPPET_MAX: usize = 256;

/// Trimmed, length-capped excerpt of a remote-host response body for logs.
pub(crate) fn http_body_snippet(body: &str) -> String {
    body.trim().chars().take(HTTP_BODY_SNIPPET_MAX).collect()
}
