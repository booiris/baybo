use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Stable identity of one exact MCP transport and governance configuration.
///
/// The tools crate computes the digest from canonical, non-secret config.
/// Model owns the value because cron grants persist it without depending on
/// the MCP runtime.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpTransportIdentity(String);

pub const MCP_TRANSPORT_IDENTITY_V1_PREFIX: &str = "mcp-transport:v1:sha256:";
const SHA256_HEX_LEN: usize = 64;

impl McpTransportIdentity {
    pub fn from_sha256(digest: [u8; 32]) -> Self {
        Self(format!(
            "{MCP_TRANSPORT_IDENTITY_V1_PREFIX}{}",
            encode_hex(&digest)
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for McpTransportIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid MCP transport identity '{value}'")]
pub struct InvalidMcpTransportIdentity {
    value: String,
}

impl FromStr for McpTransportIdentity {
    type Err = InvalidMcpTransportIdentity;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let digest = value
            .strip_prefix(MCP_TRANSPORT_IDENTITY_V1_PREFIX)
            .filter(|digest| {
                digest.len() == SHA256_HEX_LEN
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            });
        if digest.is_none() {
            return Err(InvalidMcpTransportIdentity {
                value: value.to_string(),
            });
        }
        Ok(Self(value.to_string()))
    }
}

impl Serialize for McpTransportIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for McpTransportIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

/// Authority for exactly one namespaced MCP operation on one transport config.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct McpToolGrant {
    /// Full LLM-facing registry name, such as `lighthouse/get_audits`.
    pub tool_name: String,
    pub transport_identity: McpTransportIdentity,
}

impl McpToolGrant {
    pub fn new(tool_name: impl Into<String>, transport_identity: McpTransportIdentity) -> Self {
        Self {
            tool_name: tool_name.into(),
            transport_identity,
        }
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Canonicalize exact MCP tool grants at every persistence boundary.
pub fn normalize_mcp_tool_grants(grants: &mut Vec<McpToolGrant>) {
    grants.sort_unstable();
    grants.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_identity_round_trips_only_the_versioned_sha256_shape() {
        let identity = McpTransportIdentity::from_sha256([0xab; 32]);
        assert_eq!(
            identity.as_str(),
            "mcp-transport:v1:sha256:abababababababababababababababababababababababababababababababab"
        );
        assert_eq!(
            serde_json::from_str::<McpTransportIdentity>(
                &serde_json::to_string(&identity).expect("serialize")
            )
            .expect("deserialize"),
            identity
        );
        assert!("sha256:ab".parse::<McpTransportIdentity>().is_err());
        assert!(
            format!("{MCP_TRANSPORT_IDENTITY_V1_PREFIX}{}", "A".repeat(64))
                .parse::<McpTransportIdentity>()
                .is_err()
        );
    }

    #[test]
    fn exact_tool_grants_sort_and_deduplicate_by_the_full_tuple() {
        let first_transport = McpTransportIdentity::from_sha256([0x01; 32]);
        let second_transport = McpTransportIdentity::from_sha256([0x02; 32]);
        let first = McpToolGrant::new("server/a", first_transport.clone());
        let second = McpToolGrant::new("server/a", second_transport);
        let sibling = McpToolGrant::new("server/b", first_transport);
        let mut grants = vec![
            sibling.clone(),
            second.clone(),
            first.clone(),
            second.clone(),
        ];
        normalize_mcp_tool_grants(&mut grants);
        assert_eq!(grants, vec![first, second, sibling]);
    }
}
