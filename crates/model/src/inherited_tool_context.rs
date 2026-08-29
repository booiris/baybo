use std::sync::Arc;

use crate::{McpToolGrant, McpTransportIdentity, normalize_mcp_tool_grants};

/// Transient tool authority inherited by in-process delegated work.
///
/// The context is deliberately separate from persistent session state: it
/// follows the active execution lineage, but a later independent turn does not
/// reconstruct it from the session's trigger. `Some(default())` is therefore a
/// meaningful fail-closed context, distinct from no inherited context.
#[derive(Debug, Clone, Default)]
pub struct InheritedToolContext {
    mcp_tool_grants: Arc<[McpToolGrant]>,
}

impl InheritedToolContext {
    pub fn new(mut mcp_tool_grants: Vec<McpToolGrant>) -> Self {
        normalize_mcp_tool_grants(&mut mcp_tool_grants);
        Self {
            mcp_tool_grants: mcp_tool_grants.into(),
        }
    }

    pub fn mcp_tool_grants(&self) -> &[McpToolGrant] {
        &self.mcp_tool_grants
    }

    pub fn grants_mcp_tool(
        &self,
        tool_name: &str,
        transport_identity: &McpTransportIdentity,
    ) -> bool {
        self.mcp_tool_grants
            .binary_search_by(|grant| {
                grant
                    .tool_name
                    .as_str()
                    .cmp(tool_name)
                    .then_with(|| grant.transport_identity.cmp(transport_identity))
            })
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_are_normalized_and_matched_by_exact_tool_and_transport() {
        let first_identity = McpTransportIdentity::from_sha256([1; 32]);
        let second_identity = McpTransportIdentity::from_sha256([2; 32]);
        let context = InheritedToolContext::new(vec![
            McpToolGrant::new("server/tool", second_identity.clone()),
            McpToolGrant::new("server/tool", first_identity.clone()),
            McpToolGrant::new("server/tool", first_identity.clone()),
        ]);

        assert_eq!(context.mcp_tool_grants().len(), 2);
        assert!(context.grants_mcp_tool("server/tool", &first_identity));
        assert!(context.grants_mcp_tool("server/tool", &second_identity));
        assert!(!context.grants_mcp_tool("server/sibling", &first_identity));
        assert!(
            !context.grants_mcp_tool("server/tool", &McpTransportIdentity::from_sha256([3; 32]),)
        );
    }
}
