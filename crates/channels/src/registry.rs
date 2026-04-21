use std::sync::Arc;

use aura_model::ChannelType;
use aura_tools::ApprovalGateMap;
use dashmap::DashMap;

use crate::{Channel, ChannelError, Result};

/// Central registry for live channels.
///
/// Tracks one [`Channel`] per [`ChannelType`]. Interior mutability
/// via `DashMap` so callers hold `Arc<ChannelRegistry>` directly and
/// can register/lookup concurrently without an outer lock.
pub struct ChannelRegistry {
    channels: DashMap<ChannelType, Arc<Channel>>,
    /// Per-channel approval gates, populated at registration time from
    /// [`Channel::approval_gate`]. Shared with `ToolExecutor` so it can
    /// resolve the right gate per-call without touching this registry.
    gate_map: Arc<ApprovalGateMap>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            channels: DashMap::new(),
            gate_map: Arc::new(ApprovalGateMap::new()),
        }
    }

    /// Shared handle to the per-channel gate map. Hand this to
    /// `ToolExecutor` at bootstrap — gates registered later are visible
    /// immediately since both sides share the same `Arc`.
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap> {
        Arc::clone(&self.gate_map)
    }

    /// Register a channel. Fails if the channel type is already live.
    pub fn register(&self, channel: Arc<Channel>) -> Result<()> {
        let channel_type = channel.channel_type().clone();
        if self.channels.contains_key(&channel_type) {
            return Err(ChannelError::DuplicateChannel(channel_type.to_string()));
        }
        if let Some(gate) = channel.approval_gate() {
            self.gate_map.insert(channel_type.clone(), gate);
        }
        let key = channel_type.clone();
        self.channels.insert(channel_type, channel);
        tracing::info!(channel_type = %key, "channel registered");
        Ok(())
    }

    /// Drop a live channel. Also evicts its approval gate so tool calls
    /// that arrive after disconnect fall back to the registry-wide
    /// fail-closed `AutoDenyGate`.
    pub fn unregister(&self, channel_type: ChannelType) -> Result<()> {
        self.channels
            .remove(&channel_type)
            .ok_or_else(|| ChannelError::NotFound(channel_type.to_string()))?;
        self.gate_map.remove(&channel_type);
        tracing::info!(%channel_type, "channel unregistered");
        Ok(())
    }

    /// Look up a live channel handle. Callers clone the `Arc` so the
    /// registry guard is released before awaiting.
    pub fn get(&self, channel_type: ChannelType) -> Option<Arc<Channel>> {
        self.channels.get(&channel_type).map(|e| Arc::clone(&e))
    }

    /// List all registered channel types.
    pub fn list(&self) -> Vec<ChannelType> {
        self.channels.iter().map(|e| e.key().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
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

    fn fake(ct: ChannelType) -> (Arc<Channel>, mpsc::Receiver<AgentOutput>) {
        let (tx, rx) = mpsc::channel(4);
        (Arc::new(Channel::new(ct, tx, None)), rx)
    }

    #[test]
    fn register_and_get() {
        let reg = ChannelRegistry::new();
        let (ch, _rx) = fake(ChannelType::tui());
        reg.register(ch).unwrap();
        assert!(reg.get(ChannelType::tui()).is_some());
        assert!(reg.get(ChannelType::http()).is_none());
    }

    #[test]
    fn duplicate_register_fails() {
        let reg = ChannelRegistry::new();
        let (ch1, _rx1) = fake(ChannelType::tui());
        let (ch2, _rx2) = fake(ChannelType::tui());
        reg.register(ch1).unwrap();
        let err = reg.register(ch2).unwrap_err();
        assert!(matches!(err, ChannelError::DuplicateChannel(_)));
    }

    #[test]
    fn unregister_removes_channel() {
        let reg = ChannelRegistry::new();
        let (ch, _rx) = fake(ChannelType::http());
        reg.register(ch).unwrap();
        reg.unregister(ChannelType::http()).unwrap();
        assert!(reg.get(ChannelType::http()).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn unregister_not_found() {
        let reg = ChannelRegistry::new();
        let err = reg.unregister(ChannelType::tui()).unwrap_err();
        assert!(matches!(err, ChannelError::NotFound(_)));
    }

    #[test]
    fn list_returns_all_registered() {
        let reg = ChannelRegistry::new();
        let (ch1, _rx1) = fake(ChannelType::tui());
        let (ch2, _rx2) = fake(ChannelType::http());
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
}
