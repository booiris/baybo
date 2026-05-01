//! Gateway-side `SidecarMcpProvider` over [`McpTunnelRouter`].
//!
//! The agent loop asks the provider "what MCP tools are available
//! for *this* session?" every turn (see
//! [`aura_tools::mcp::SidecarMcpProvider`]). This module materialises
//! that view: open a tunnel to the sidecar serving
//! `session.user.channel`, run the rmcp handshake, snapshot the
//! advertised tools, and cache the result for future turns.
//!
//! # Caching
//!
//! Cache key is `ChannelType` alone, not `(ChannelType, bot_id)`.
//! Reason: a sidecar advertises the same tool set regardless of
//! which bot a call is for — the bot scoping happens *at call time*
//! via `_meta.auraSessionId` injection (slice 2E). One rmcp session
//! per channel covers every agent turn that targets that channel.
//!
//! # Lazy connect
//!
//! The first agent turn whose session targets a channel with the
//! `mcp_tunnel` capability triggers `connect_sidecar`. Subsequent
//! turns reuse the cached snapshot — no per-turn handshake. If the
//! sidecar disconnects, [`Self::detach`] (called from `route.rs`)
//! drops the cached entry so a reconnect re-runs the handshake on
//! demand.
//!
//! # Concurrency
//!
//! `DashMap<ChannelType, Arc<Mutex<Option<CachedSession>>>>`. The
//! outer DashMap shards by channel; the inner mutex serialises
//! concurrent first-attach attempts so two agent turns hitting an
//! uncached channel together don't open two tunnels.

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{ChannelType, Session};
use aura_tools::ToolDefinition;
use aura_tools::mcp::{McpServerSession, SidecarMcpProvider, connect_sidecar};
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::diagnose::ChannelCapabilities;
use super::handshake::CAP_MCP_TUNNEL;
use super::mcp_tunnel::{McpTunnelGuard, McpTunnelRouter};

pub struct SidecarMcpManager {
    router: Arc<McpTunnelRouter>,
    capabilities: Arc<ChannelCapabilities>,
    sessions: DashMap<ChannelType, Arc<Mutex<Option<CachedSession>>>>,
}

struct CachedSession {
    session: McpServerSession,
    tools: Vec<ToolDefinition>,
    /// Holds the tunnel registry slot for the rmcp session's
    /// lifetime. Dropped before `session.shutdown()` on detach so
    /// the inbound forwarding stops first.
    _guard: McpTunnelGuard,
}

impl SidecarMcpManager {
    pub fn new(router: Arc<McpTunnelRouter>, capabilities: Arc<ChannelCapabilities>) -> Self {
        Self {
            router,
            capabilities,
            sessions: DashMap::new(),
        }
    }

    /// Drop the cached rmcp session for `channel_type`, if any.
    /// Called from the WS route's disconnect path so a reconnect
    /// re-runs the handshake instead of reusing a dead tunnel.
    pub async fn detach(&self, channel_type: &ChannelType) {
        let Some((_, slot)) = self.sessions.remove(channel_type) else {
            return;
        };
        let cached = { slot.lock().await.take() };
        if let Some(CachedSession { session, _guard, .. }) = cached {
            drop(_guard);
            session.shutdown().await;
        }
    }
}

#[async_trait]
impl SidecarMcpProvider for SidecarMcpManager {
    async fn tool_definitions_for_session(&self, session: &Session) -> Vec<ToolDefinition> {
        let channel_type = session.user.channel.clone();
        if !self.capabilities.supports(&channel_type, CAP_MCP_TUNNEL) {
            return Vec::new();
        }

        let slot = self
            .sessions
            .entry(channel_type.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone();
        let mut guard = slot.lock().await;
        if let Some(cached) = guard.as_ref() {
            return cached.tools.clone();
        }

        let tunnel = self.router.open(channel_type.clone());
        let (sender, rx, tunnel_guard) = tunnel.into_transport_parts();
        let mcp_session = match connect_sidecar(sender, rx).await {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    %channel_type,
                    error = %e,
                    "sidecar mcp handshake failed; dropping tunnel and returning no tools",
                );
                drop(tunnel_guard);
                return Vec::new();
            }
        };

        let tools: Vec<ToolDefinition> = mcp_session
            .tools()
            .iter()
            .map(|t| ToolDefinition {
                name: format!("{channel_type}/{}", t.name),
                description: t
                    .description
                    .as_ref()
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                parameters_schema: serde_json::Value::Object((*t.input_schema).clone()),
            })
            .collect();
        info!(
            %channel_type,
            count = tools.len(),
            "sidecar mcp: discovered tools",
        );

        let snapshot = tools.clone();
        *guard = Some(CachedSession {
            session: mcp_session,
            tools,
            _guard: tunnel_guard,
        });
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aura_model::{User, ChannelType};
    use chrono::Utc;

    use crate::channel::ChannelControlRegistry;

    fn dummy_session(channel: ChannelType) -> Session {
        let user = User {
            id: "u".into(),
            name: None,
            channel: channel.clone(),
        };
        Session {
            id: "s".into(),
            user: user.clone(),
            channel,
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: Default::default(),
        }
    }

    #[tokio::test]
    async fn returns_empty_when_channel_does_not_claim_mcp_tunnel() {
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let caps = Arc::new(ChannelCapabilities::new());
        // No `record(channel, [..., "mcp_tunnel"])` call — so the
        // channel doesn't claim the cap.
        let manager = SidecarMcpManager::new(router, caps);
        let session = dummy_session(ChannelType::from("lark"));
        assert!(
            manager
                .tool_definitions_for_session(&session)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn returns_empty_when_handshake_fails_and_does_not_cache() {
        // No sidecar registered on the control reg, so the rmcp
        // handshake will fail at the first send. Manager must
        // surface an empty list and NOT cache the failure (so a
        // later reconnect can retry).
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(Arc::clone(&control)));
        let caps = Arc::new(ChannelCapabilities::new());
        let ct = ChannelType::from("lark");
        caps.record(ct.clone(), vec!["mcp_tunnel".into()]);
        let manager = SidecarMcpManager::new(router, caps);

        let session = dummy_session(ct.clone());
        let tools = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            manager.tool_definitions_for_session(&session),
        )
        .await
        .expect("handshake fails fast")
        .clone();
        assert!(tools.is_empty());

        // The cache slot may have been created but the session
        // wasn't stored. detach is a no-op either way and must not
        // panic.
        manager.detach(&ct).await;
    }
}
