pub mod config;
pub mod credentials;
pub mod error;
pub mod oauth;
pub mod reconciler;
pub mod sidecar;
pub mod tool;
pub mod transport;
pub mod vault_keys;

pub use config::{McpFile, McpServerEntry, McpTransportConfig, OAuthConfig, TrustLevelConfig};
pub use error::{McpError, McpResult};
pub use reconciler::McpReconciler;
pub use sidecar::{
    NoSidecarMcp, SidecarMcpProvider, SidecarSender, SidecarTransport, SidecarTransportError,
};
pub use tool::McpTool;
pub use transport::{McpServerSession, call_tool_via_peer, connect, connect_sidecar};
