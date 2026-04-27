//! Vault key naming conventions for MCP credentials.
//!
//! All MCP-related secrets share the `mcp.<name>.…` prefix so a single
//! enumeration scrubs every credential when a server is removed.
//! Both `aura-cli` (writer) and the transport layer (reader) go through
//! these helpers — never hand-format the strings.

pub fn env_bag(name: &str) -> String {
    format!("mcp.{name}.env")
}

pub fn header_bag(name: &str) -> String {
    format!("mcp.{name}.headers")
}

pub fn oauth_client_secret(name: &str) -> String {
    format!("mcp.{name}.oauth.client_secret")
}

/// Single key carrying the JSON-serialized rmcp [`StoredCredentials`]
/// (client_id + access_token + refresh_token + granted_scopes +
/// token_received_at). Replaces the earlier split between
/// `oauth.access_token` and `oauth.refresh_token` so refresh-on-demand
/// can rotate both atomically through the credential store.
pub fn oauth_credentials(name: &str) -> String {
    format!("mcp.{name}.oauth.credentials")
}

/// All vault keys ever associated with a server, in the deletion order.
/// `aura mcp remove` calls `delete_secret` on each in turn so a single
/// pass scrubs every secret the server ever stored.
pub fn all_keys_for(name: &str) -> [String; 4] {
    [
        env_bag(name),
        header_bag(name),
        oauth_client_secret(name),
        oauth_credentials(name),
    ]
}
