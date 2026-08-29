pub mod config;
pub mod content_adapter;
pub mod credentials;
pub mod embedded;
pub mod error;
pub mod identity;
pub mod log_line;
pub mod oauth;
pub mod profile;
pub mod reconciler;
mod runtime;
pub mod tool;
pub mod transport;
pub mod vault_keys;

pub use config::{McpFile, McpServerEntry, McpTransportConfig, OAuthConfig, TrustLevelConfig};
pub use embedded::EmbeddedMcpServer;
pub use error::{McpError, McpResult};
pub use identity::transport_identity;
pub use log_line::SidecarLogLine;
pub use profile::{
    BrowserProfileParams, EmbeddedMcpProfile, browser_mcp_profile, embedded_servers,
};
pub use reconciler::McpReconciler;
pub use runtime::McpRuntime;
pub use tool::{McpTool, McpToolMetadata};
pub use transport::{McpServerSession, connect};
