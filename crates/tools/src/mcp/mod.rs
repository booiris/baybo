pub mod config;
pub mod content_adapter;
pub mod credentials;
pub mod embedded;
pub mod error;
pub mod oauth;
pub mod profile;
pub mod reconciler;
pub mod sidecar;
pub mod tool;
pub mod transport;
pub mod vault_keys;

pub use config::{McpFile, McpServerEntry, McpTransportConfig, OAuthConfig, TrustLevelConfig};
pub use embedded::EmbeddedMcpServer;
pub use error::{McpError, McpResult};
pub use profile::{EmbeddedMcpProfile, browser_mcp_profile, embedded_servers};
pub use reconciler::McpReconciler;
pub use sidecar::{
    NoSidecarMcp, SidecarMcpProvider, SidecarSender, SidecarTransport, SidecarTransportError,
};
pub use tool::McpTool;
pub use transport::{CallToolMeta, McpServerSession, call_tool_via_peer, connect, connect_sidecar};
