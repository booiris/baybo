use std::sync::Arc;

use aura_model::ChannelType;
use aura_tools::ApprovalGateMap;
use dashmap::DashMap;

use crate::{Channel, ChannelError, Result};

/// Central registry for live channels.
///
/// Two disjoint views sit behind one registry:
///
/// * **Sidecars** — one [`Channel`] per [`ChannelType`]. A Telegram
///   sidecar (for example) serves every Telegram user from a single
///   process, so the 1:1 `ChannelType → Channel` mapping is correct.
/// * **Session-scoped clients** — many per [`ChannelType`], keyed by
///   `session_id`. Used by the built-in TUI so multiple TUI processes
///   can each pin their own session without fighting over the
///   channel-type slot.
///
/// [`get_for`](Self::get_for) hides the split from callers: it prefers
/// a session-scoped match when the output's `session_id` has an
/// attached client and falls back to the type-level sidecar otherwise.
/// This keeps the router (and every sidecar in the tree) oblivious to
/// the session-scoped flavor.
pub struct ChannelRegistry {
    /// Type-level sidecars. One entry per `ChannelType`.
    sidecars: DashMap<ChannelType, Arc<Channel>>,
    /// Session-scoped clients. Keyed by `session_id` so a single
    /// session can only have one attached client at a time regardless
    /// of channel type.
    session_clients: DashMap<String, Arc<Channel>>,
    /// Per-channel approval gates, populated at registration time from
    /// [`Channel::approval_gate`]. Shared with `ToolExecutor` so it can
    /// resolve the right gate per-call without touching this registry.
    gate_map: Arc<ApprovalGateMap>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            sidecars: DashMap::new(),
            session_clients: DashMap::new(),
            gate_map: Arc::new(ApprovalGateMap::new()),
        }
    }

    /// Shared handle to the per-channel gate map. Hand this to
    /// `ToolExecutor` at bootstrap — gates registered later are visible
    /// immediately since both sides share the same `Arc`.
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap> {
        Arc::clone(&self.gate_map)
    }

    /// Register a channel. Routing depends on
    /// [`Channel::owned_session`]:
    ///
    /// * `None` → sidecar slot. Fails with
    ///   [`ChannelError::DuplicateChannel`] if another sidecar already
    ///   owns the channel type.
    /// * `Some(sid)` → session-scoped slot. Fails with
    ///   [`ChannelError::DuplicateSessionClient`] if another client is
    ///   already attached to that session id.
    pub fn register(&self, channel: Arc<Channel>) -> Result<()> {
        let channel_type = channel.channel_type().clone();
        match channel.owned_session() {
            Some(session_id) => {
                let session_id = session_id.to_owned();
                if self.session_clients.contains_key(&session_id) {
                    return Err(ChannelError::DuplicateSessionClient(session_id));
                }
                if let Some(gate) = channel.approval_gate() {
                    self.gate_map
                        .insert_session(channel_type.clone(), session_id.clone(), gate);
                }
                self.session_clients.insert(session_id.clone(), channel);
                tracing::info!(%channel_type, session_id, "session-scoped channel registered");
            }
            None => {
                if self.sidecars.contains_key(&channel_type) {
                    return Err(ChannelError::DuplicateChannel(channel_type.to_string()));
                }
                if let Some(gate) = channel.approval_gate() {
                    self.gate_map.insert(channel_type.clone(), gate);
                }
                let key = channel_type.clone();
                self.sidecars.insert(channel_type, channel);
                tracing::info!(channel_type = %key, "sidecar channel registered");
            }
        }
        Ok(())
    }

    /// Drop a sidecar. Also evicts its type-level approval gate so
    /// tool calls that arrive after disconnect fall back to the
    /// fail-closed `AutoDenyGate`.
    pub fn unregister_sidecar(&self, channel_type: ChannelType) -> Result<()> {
        self.sidecars
            .remove(&channel_type)
            .ok_or_else(|| ChannelError::NotFound(channel_type.to_string()))?;
        self.gate_map.remove(&channel_type);
        tracing::info!(%channel_type, "sidecar channel unregistered");
        Ok(())
    }

    /// Drop a session-scoped client. Looks up by `session_id` only so
    /// the caller doesn't have to remember which channel type the
    /// client registered under. Evicts the per-session gate in the
    /// same step.
    pub fn unregister_session(&self, session_id: &str) -> Result<()> {
        let (_, channel) = self
            .session_clients
            .remove(session_id)
            .ok_or_else(|| ChannelError::SessionClientNotFound(session_id.to_string()))?;
        let channel_type = channel.channel_type().clone();
        self.gate_map.remove_session(&channel_type, session_id);
        tracing::info!(%channel_type, session_id, "session-scoped channel unregistered");
        Ok(())
    }

    /// Resolve the channel that should receive output for
    /// `(channel_type, session_id)`. Session-scoped clients win over
    /// sidecars so a TUI attached to `sid` receives its own stream
    /// even if a type-level sidecar also happens to be registered.
    pub fn get_for(&self, channel_type: &ChannelType, session_id: &str) -> Option<Arc<Channel>> {
        if let Some(entry) = self.session_clients.get(session_id)
            && entry.channel_type() == channel_type
        {
            return Some(Arc::clone(entry.value()));
        }
        self.sidecars
            .get(channel_type)
            .map(|e| Arc::clone(e.value()))
    }

    /// Look up a sidecar by channel type, ignoring any session-scoped
    /// clients. Kept for callers that are specifically asking about
    /// the sidecar slot (e.g. admin UIs / diagnostics).
    pub fn get_sidecar(&self, channel_type: ChannelType) -> Option<Arc<Channel>> {
        self.sidecars.get(&channel_type).map(|e| Arc::clone(&e))
    }

    /// List all registered sidecars' channel types.
    pub fn list(&self) -> Vec<ChannelType> {
        self.sidecars.iter().map(|e| e.key().clone()).collect()
    }

    /// Snapshot the session ids with an attached session-scoped client.
    pub fn list_session_clients(&self) -> Vec<String> {
        self.session_clients
            .iter()
            .map(|e| e.key().clone())
            .collect()
    }

    /// Total number of registered channels across both views.
    pub fn len(&self) -> usize {
        self.sidecars.len() + self.session_clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sidecars.is_empty() && self.session_clients.is_empty()
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentOutput;
    use tokio::sync::mpsc;

    fn sidecar(ct: ChannelType) -> (Arc<Channel>, mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = mpsc::channel(4);
        (Arc::new(Channel::new(ct, tx, None)), rx)
    }

    fn session_client(
        ct: ChannelType,
        session_id: &str,
    ) -> (Arc<Channel>, mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = mpsc::channel(4);
        (
            Arc::new(Channel::new_session_scoped(
                ct,
                session_id.to_owned(),
                tx,
                None,
            )),
            rx,
        )
    }

    #[test]
    fn register_and_get_sidecar() {
        let reg = ChannelRegistry::new();
        let (ch, _rx) = sidecar(ChannelType::tui());
        reg.register(ch).unwrap();
        assert!(reg.get_sidecar(ChannelType::tui()).is_some());
        assert!(reg.get_sidecar(ChannelType::http()).is_none());
    }

    #[test]
    fn duplicate_sidecar_register_fails() {
        let reg = ChannelRegistry::new();
        let (ch1, _rx1) = sidecar(ChannelType::tui());
        let (ch2, _rx2) = sidecar(ChannelType::tui());
        reg.register(ch1).unwrap();
        let err = reg.register(ch2).unwrap_err();
        assert!(matches!(err, ChannelError::DuplicateChannel(_)));
    }

    #[test]
    fn unregister_removes_sidecar() {
        let reg = ChannelRegistry::new();
        let (ch, _rx) = sidecar(ChannelType::http());
        reg.register(ch).unwrap();
        reg.unregister_sidecar(ChannelType::http()).unwrap();
        assert!(reg.get_sidecar(ChannelType::http()).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn unregister_not_found() {
        let reg = ChannelRegistry::new();
        let err = reg.unregister_sidecar(ChannelType::tui()).unwrap_err();
        assert!(matches!(err, ChannelError::NotFound(_)));
    }

    #[test]
    fn list_returns_all_sidecars() {
        let reg = ChannelRegistry::new();
        let (ch1, _rx1) = sidecar(ChannelType::tui());
        let (ch2, _rx2) = sidecar(ChannelType::http());
        reg.register(ch1).unwrap();
        reg.register(ch2).unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn default_is_empty() {
        let reg = ChannelRegistry::default();
        assert!(reg.is_empty());
    }

    #[test]
    fn two_session_clients_same_type_different_sessions() {
        let reg = ChannelRegistry::new();
        let (a, _rx_a) = session_client(ChannelType::tui(), "sess-a");
        let (b, _rx_b) = session_client(ChannelType::tui(), "sess-b");
        reg.register(a).unwrap();
        reg.register(b).unwrap();
        assert_eq!(reg.list_session_clients().len(), 2);
    }

    #[test]
    fn duplicate_session_id_rejected() {
        let reg = ChannelRegistry::new();
        let (a, _rx_a) = session_client(ChannelType::tui(), "sess-x");
        let (b, _rx_b) = session_client(ChannelType::tui(), "sess-x");
        reg.register(a).unwrap();
        let err = reg.register(b).unwrap_err();
        assert!(matches!(err, ChannelError::DuplicateSessionClient(_)));
    }

    #[test]
    fn get_for_prefers_session_client_over_sidecar() {
        let reg = ChannelRegistry::new();
        let (side, _rx_side) = sidecar(ChannelType::tui());
        let (client, _rx_client) = session_client(ChannelType::tui(), "sess-1");
        reg.register(side).unwrap();
        reg.register(client.clone()).unwrap();
        let hit = reg.get_for(&ChannelType::tui(), "sess-1").unwrap();
        assert!(Arc::ptr_eq(&hit, &client));
    }

    #[test]
    fn get_for_falls_back_to_sidecar_when_session_unclaimed() {
        let reg = ChannelRegistry::new();
        let (side, _rx_side) = sidecar(ChannelType::tui());
        reg.register(side.clone()).unwrap();
        let hit = reg.get_for(&ChannelType::tui(), "orphan").unwrap();
        assert!(Arc::ptr_eq(&hit, &side));
    }

    #[test]
    fn get_for_returns_none_when_nothing_matches() {
        let reg = ChannelRegistry::new();
        assert!(reg.get_for(&ChannelType::tui(), "sess-none").is_none());
    }

    #[test]
    fn session_client_cross_type_does_not_collide() {
        // A session-scoped client is looked up by session_id, but the
        // channel_type must also match or we'd misroute output
        // belonging to a different channel type that happens to share
        // a session id.
        let reg = ChannelRegistry::new();
        let (tui_client, _rx) = session_client(ChannelType::tui(), "sess-1");
        let (http_side, _rx2) = sidecar(ChannelType::http());
        reg.register(tui_client).unwrap();
        reg.register(http_side.clone()).unwrap();
        let hit = reg.get_for(&ChannelType::http(), "sess-1").unwrap();
        assert!(Arc::ptr_eq(&hit, &http_side));
    }
}
