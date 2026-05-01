//! Gateway-side `SidecarMcpProvider` over [`McpTunnelRouter`].
//!
//! Connection lifetime is **driven by the sidecar's lifetime, not by
//! agent loop turns.** When a sidecar registers and claims the
//! `mcp_tunnel` capability, `route.rs` calls [`Self::attach`] which
//! spawns the rmcp handshake in the background. Subsequent agent
//! loop turns just read the cached tool list — no per-turn
//! connection cost. On disconnect, [`Self::detach`] tears down the
//! cached rmcp session before the tunnel registry drains.
//!
//! Tool **exposure** is still per-session: the LLM only sees a
//! channel's MCP tools when the agent loop is processing a message
//! from that channel. A telegram session never sees lark's tools
//! even if both sidecars are connected.
//!
//! # Cache key
//!
//! `ChannelType` alone, not `(ChannelType, bot_id)`. The sidecar
//! advertises the same tool set for every bot it serves; bot
//! scoping happens *at call time* via `_meta.auraSessionId` (slice
//! 2E). One rmcp session per channel covers every agent turn that
//! targets that channel.
//!
//! # Concurrency
//!
//! `DashMap<ChannelType, Arc<Mutex<Option<CachedSession>>>>`. The
//! outer DashMap shards by channel; the inner mutex is held only by
//! the background handshake task while populating the slot, so
//! reads from `tool_definitions_for_session` never block on
//! handshake latency.

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{ChannelType, Session};
use aura_tools::ToolDefinition;
use aura_tools::mcp::{McpServerSession, SidecarMcpProvider, connect_sidecar};
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing::{info, warn};

use super::mcp_tunnel::{McpTunnelGuard, McpTunnelRouter};

pub struct SidecarMcpManager {
    router: Arc<McpTunnelRouter>,
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
    pub fn new(router: Arc<McpTunnelRouter>) -> Self {
        Self {
            router,
            sessions: DashMap::new(),
        }
    }

    /// Eagerly attach an rmcp session for `channel_type`. Called
    /// from `route.rs` after capability negotiation when the
    /// sidecar claimed `mcp_tunnel`. Spawns the handshake in the
    /// background — agent loops can keep accepting messages while
    /// the rmcp `initialize` + `tools/list` round-trips run.
    ///
    /// Idempotent: a re-attach for an already-attached channel is a
    /// no-op (the existing slot stays). Disconnect → reattach must
    /// go through [`Self::detach`] first.
    pub fn attach(self: &Arc<Self>, channel_type: ChannelType) {
        let slot = Arc::new(Mutex::new(None));
        match self.sessions.entry(channel_type.clone()) {
            dashmap::Entry::Occupied(_) => return,
            dashmap::Entry::Vacant(v) => {
                v.insert(Arc::clone(&slot));
            }
        }

        let manager = Arc::clone(self);
        let ct_for_task = channel_type.clone();
        tokio::spawn(async move {
            let tunnel = manager.router.open(ct_for_task.clone());
            let (sender, rx, tunnel_guard) = tunnel.into_transport_parts();
            match connect_sidecar(sender, rx).await {
                Ok(mcp_session) => {
                    let tools: Vec<ToolDefinition> = mcp_session
                        .tools()
                        .iter()
                        .map(|t| ToolDefinition {
                            name: format!("{ct_for_task}/{}", t.name),
                            description: t
                                .description
                                .as_ref()
                                .map(|c| c.to_string())
                                .unwrap_or_default(),
                            parameters_schema: serde_json::Value::Object(
                                (*t.input_schema).clone(),
                            ),
                        })
                        .collect();
                    info!(
                        channel_type = %ct_for_task,
                        count = tools.len(),
                        "sidecar mcp: attached",
                    );
                    *slot.lock().await = Some(CachedSession {
                        session: mcp_session,
                        tools,
                        _guard: tunnel_guard,
                    });
                }
                Err(e) => {
                    warn!(
                        channel_type = %ct_for_task,
                        error = %e,
                        "sidecar mcp attach failed; removing slot so a future re-attach can retry",
                    );
                    drop(tunnel_guard);
                    manager.sessions.remove(&ct_for_task);
                }
            }
        });
    }

    /// Drop the cached rmcp session for `channel_type`, if any.
    /// Called from the WS route's disconnect path so a reconnect
    /// re-runs the handshake instead of reusing a dead tunnel.
    /// Safe to call before the background `attach` finishes — the
    /// slot is removed and the in-flight handshake task discovers
    /// the empty cache when it tries to populate it.
    pub async fn detach(&self, channel_type: &ChannelType) {
        let Some((_, slot)) = self.sessions.remove(channel_type) else {
            return;
        };
        let cached = { slot.lock().await.take() };
        if let Some(CachedSession {
            session, _guard, ..
        }) = cached
        {
            drop(_guard);
            session.shutdown().await;
        }
    }
}

#[async_trait]
impl SidecarMcpProvider for SidecarMcpManager {
    async fn tool_definitions_for_session(&self, session: &Session) -> Vec<ToolDefinition> {
        let Some(slot) = self.sessions.get(&session.user.channel) else {
            return Vec::new();
        };
        let slot = slot.value().clone();
        let guard = slot.lock().await;
        guard
            .as_ref()
            .map(|c| c.tools.clone())
            .unwrap_or_default()
    }

    async fn claims_tool(&self, session: &Session, name: &str) -> bool {
        let channel_type = &session.user.channel;
        let prefix = format!("{channel_type}/");
        if !name.starts_with(&prefix) {
            return false;
        }
        let Some(slot) = self.sessions.get(channel_type) else {
            return false;
        };
        let slot = slot.value().clone();
        let guard = slot.lock().await;
        guard.is_some()
    }

    async fn execute_for_session(
        &self,
        session: &Session,
        name: &str,
        params: serde_json::Value,
    ) -> Option<Result<aura_tools::ToolOutput, String>> {
        let channel_type = session.user.channel.clone();
        // Only claim names that match the session's channel prefix.
        // Mismatch falls back to local dispatch.
        let prefix = format!("{channel_type}/");
        let inner_name = name.strip_prefix(&prefix)?;

        // Cache lookup. If the channel never attached or the
        // background handshake hasn't finished yet, fall back to
        // local dispatch (LLM gets a NotFound — same as if the
        // sidecar weren't there at all).
        let slot = self.sessions.get(&channel_type)?.value().clone();
        // Drop the slot guard before awaiting the rmcp round-trip
        // so a concurrent `detach` (e.g. sidecar disconnects mid-
        // call) can run instead of blocking behind us. Clone the
        // peer-bearing handle out under the guard, then release.
        let peer_session = {
            let guard = slot.lock().await;
            guard.as_ref().map(|c| c.session.peer())
        };
        let peer = peer_session?;

        // The auraSessionId meta lets the sidecar map the call
        // back to a specific bot in multi-bot deployments. Until
        // the sidecar grows that lookup, slice 2A's
        // `LarkChannelResolution::ambiguous` still fails closed.
        Some(
            aura_tools::mcp::call_tool_via_peer(&peer, inner_name, params, Some(&session.id))
                .await
                .map_err(|e| format!("sidecar mcp {name}: {e}")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aura_model::{ChannelType, User};
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
    async fn returns_empty_when_channel_was_never_attached() {
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = SidecarMcpManager::new(router);
        let session = dummy_session(ChannelType::from("lark"));
        assert!(
            manager
                .tool_definitions_for_session(&session)
                .await
                .is_empty(),
            "no attach => no tools"
        );
    }

    #[tokio::test]
    async fn attach_without_sidecar_registers_then_clears_slot_on_failure() {
        // Eager attach spawns the handshake; with no sidecar
        // registered on the control reg, the first send fails so
        // the task removes the slot. Reads see an empty list both
        // during the handshake and after it fails.
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(Arc::clone(&control)));
        let manager = Arc::new(SidecarMcpManager::new(router));
        let ct = ChannelType::from("lark");
        manager.attach(ct.clone());

        // Wait briefly for the spawned task to fail and remove the
        // slot. Bound the wait so a hang is loud.
        for _ in 0..50 {
            if !manager.sessions.contains_key(&ct) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !manager.sessions.contains_key(&ct),
            "failed handshake must clear the slot",
        );

        let session = dummy_session(ct.clone());
        assert!(
            manager
                .tool_definitions_for_session(&session)
                .await
                .is_empty()
        );

        // detach is safe even when the slot is already gone.
        manager.detach(&ct).await;
    }

    #[tokio::test]
    async fn detach_is_idempotent_for_unknown_channel() {
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = SidecarMcpManager::new(router);
        manager.detach(&ChannelType::from("never-attached")).await;
        // No panic, no error — the empty-cache path is the steady
        // state right after gateway boot.
    }

    #[tokio::test]
    async fn re_attach_for_same_channel_is_no_op() {
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = Arc::new(SidecarMcpManager::new(router));
        let ct = ChannelType::from("lark");
        manager.attach(ct.clone());
        // Second call returns immediately without spawning a
        // second background task. Verifiable indirectly by the
        // sessions map carrying exactly one slot.
        manager.attach(ct.clone());
        assert_eq!(manager.sessions.len(), 1);
    }

    #[tokio::test]
    async fn claims_tool_matches_provider_preflight_contract() {
        // The agent loop relies on this: if claims_tool returns true,
        // execute_for_session must claim the call (and vice versa)
        // so the security pipeline wraps every claimed dispatch.
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = Arc::new(SidecarMcpManager::new(router));

        let session = dummy_session(ChannelType::from("lark"));
        // Not attached: claims_tool false, execute None.
        assert!(!manager.claims_tool(&session, "lark/feishu_get_chat_info").await);
        assert!(
            manager
                .execute_for_session(&session, "lark/feishu_get_chat_info", serde_json::json!({}))
                .await
                .is_none()
        );

        // Wrong-channel prefix: false even when its slot would be
        // populated. We can't populate without a real handshake, but
        // the prefix check runs first, so the result is the same.
        manager.attach(ChannelType::from("lark"));
        assert!(!manager.claims_tool(&session, "weixin/anything").await);
        assert!(!manager.claims_tool(&session, "no_prefix_at_all").await);
    }

    #[tokio::test]
    async fn execute_returns_none_for_unrelated_channel_prefix() {
        // Even when the session's channel is attached and ready,
        // a tool name with a different channel prefix doesn't
        // belong to this provider — the agent loop must fall
        // through to local dispatch.
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = Arc::new(SidecarMcpManager::new(router));
        let lark = ChannelType::from("lark");
        manager.attach(lark.clone());

        let session = dummy_session(lark);
        // `read_file` has no channel prefix — local builtin name.
        assert!(
            manager
                .execute_for_session(&session, "read_file", serde_json::json!({}))
                .await
                .is_none(),
            "names without a channel prefix must not be claimed",
        );
        // `weixin/whatever` is for a different channel.
        assert!(
            manager
                .execute_for_session(&session, "weixin/whatever", serde_json::json!({}))
                .await
                .is_none(),
            "wrong-channel prefix must not be claimed",
        );
    }

    #[tokio::test]
    async fn execute_returns_none_when_no_attach() {
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = SidecarMcpManager::new(router);
        let session = dummy_session(ChannelType::from("lark"));
        assert!(
            manager
                .execute_for_session(&session, "lark/feishu_get_chat_info", serde_json::json!({}))
                .await
                .is_none(),
            "no attach => no claim, agent loop falls through to local dispatch"
        );
    }

    #[tokio::test]
    async fn lookup_keys_off_session_channel_type() {
        // `tool_definitions_for_session` keys off `session.user.channel`,
        // so a telegram session never sees lark's slot even if lark is
        // (or were) attached. We assert this without standing up a
        // real rmcp session: a session targeting a never-attached
        // channel returns empty even when other channels have slots
        // queued (the failed-handshake test above exercises that
        // exact configuration before this test runs in isolation).
        let control = Arc::new(ChannelControlRegistry::new());
        let router = Arc::new(McpTunnelRouter::new(control));
        let manager = Arc::new(SidecarMcpManager::new(router));
        manager.attach(ChannelType::from("lark"));

        let tg_session = dummy_session(ChannelType::from("telegram"));
        assert!(
            manager
                .tool_definitions_for_session(&tg_session)
                .await
                .is_empty(),
            "session for an unrelated channel must not see other channels' tools",
        );
    }
}
