use serde::{Deserialize, Serialize};

/// Network access policy for sandboxed executions.
///
/// Follows deny-by-default: only domains explicitly listed in
/// `allowed_domains` may be accessed, and loopback must be
/// explicitly permitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicy {
    /// Domains that sandboxed code is allowed to contact.
    pub allowed_domains: Vec<String>,
    /// Whether loopback (127.0.0.1 / localhost) access is permitted.
    pub allow_loopback: bool,
}
