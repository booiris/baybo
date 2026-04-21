use std::sync::Arc;

use aura_model::ChannelType;
use aura_tools::ApprovalGateMap;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::{ChannelAdapter, ChannelError, ChannelStatus, IncomingMessage, Result};

struct ChannelEntry {
    adapter: Arc<dyn ChannelAdapter>,
    status: Mutex<ChannelStatus>,
}

/// Central registry for channel adapters.
///
/// Manages registration, lookup, and lifecycle (start/stop) of all channel
/// adapters. Uses interior mutability so callers hold `Arc<ChannelRegistry>`
/// directly — no outer `RwLock` is needed, which avoids holding a lock
/// guard across the `.await` inside `start_all`/`stop_all`.
pub struct ChannelRegistry {
    channels: DashMap<ChannelType, ChannelEntry>,
    /// Per-channel approval gates, populated at registration time from
    /// [`ChannelAdapter::approval_gate`]. Shared with `ToolExecutor` so
    /// it can resolve the right gate per-call without touching this
    /// registry.
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

    /// Register a channel adapter. Fails if the channel type is already registered.
    ///
    /// Takes `Arc<dyn ChannelAdapter>` rather than `Box<dyn ...>` so
    /// callers can keep their own handle to the adapter (e.g. route
    /// handlers calling methods directly) without needing a newtype
    /// wrapper just to satisfy the registry.
    pub fn register(&self, adapter: Arc<dyn ChannelAdapter>) -> Result<()> {
        let channel_type = adapter.channel_type();
        if self.channels.contains_key(&channel_type) {
            return Err(ChannelError::DuplicateChannel(channel_type.to_string()));
        }
        if let Some(gate) = adapter.approval_gate() {
            self.gate_map.insert(channel_type.clone(), gate);
        }
        let key = channel_type.clone();
        self.channels.insert(
            channel_type,
            ChannelEntry {
                adapter,
                status: Mutex::new(ChannelStatus::Registered),
            },
        );
        tracing::info!(channel_type = %key, "channel registered");
        Ok(())
    }

    /// Remove a channel adapter. Stops it first if it is running.
    pub async fn unregister(&self, channel_type: ChannelType) -> Result<()> {
        let (_, entry) = self
            .channels
            .remove(&channel_type)
            .ok_or_else(|| ChannelError::NotFound(channel_type.to_string()))?;

        let was_running = *entry.status.lock() == ChannelStatus::Running;
        if was_running && let Err(e) = entry.adapter.stop().await {
            tracing::warn!(%channel_type, error = %e, "error stopping channel during unregister");
        }
        tracing::info!(%channel_type, "channel unregistered");
        Ok(())
    }

    /// Look up an owned `Arc` handle for the adapter, so callers can
    /// release the registry guard before awaiting on the adapter.
    pub fn get_adapter(&self, channel_type: ChannelType) -> Option<Arc<dyn ChannelAdapter>> {
        self.channels
            .get(&channel_type)
            .map(|e| Arc::clone(&e.adapter))
    }

    /// Start all registered channels that are not already running.
    pub async fn start_all(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let to_start: Vec<(ChannelType, Arc<dyn ChannelAdapter>)> = self
            .channels
            .iter()
            .filter(|e| *e.value().status.lock() != ChannelStatus::Running)
            .map(|e| (e.key().clone(), Arc::clone(&e.value().adapter)))
            .collect();

        for (channel_type, adapter) in to_start {
            match adapter.start(sender.clone()).await {
                Ok(()) => {
                    if let Some(entry) = self.channels.get(&channel_type) {
                        *entry.status.lock() = ChannelStatus::Running;
                    }
                    tracing::info!(%channel_type, "channel started");
                }
                Err(e) => {
                    let reason = e.to_string();
                    if let Some(entry) = self.channels.get(&channel_type) {
                        *entry.status.lock() = ChannelStatus::Error(reason);
                    }
                    tracing::error!(%channel_type, error = %e, "failed to start channel");
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Stop all running channels. Continues on individual failures.
    pub async fn stop_all(&self) {
        let to_stop: Vec<(ChannelType, Arc<dyn ChannelAdapter>)> = self
            .channels
            .iter()
            .filter(|e| *e.value().status.lock() == ChannelStatus::Running)
            .map(|e| (e.key().clone(), Arc::clone(&e.value().adapter)))
            .collect();

        for (channel_type, adapter) in to_stop {
            match adapter.stop().await {
                Ok(()) => {
                    if let Some(entry) = self.channels.get(&channel_type) {
                        *entry.status.lock() = ChannelStatus::Stopped;
                    }
                    tracing::info!(%channel_type, "channel stopped");
                }
                Err(e) => {
                    let reason = e.to_string();
                    if let Some(entry) = self.channels.get(&channel_type) {
                        *entry.status.lock() = ChannelStatus::Error(reason);
                    }
                    tracing::error!(%channel_type, error = %e, "failed to stop channel");
                }
            }
        }
    }

    /// Return the status of all registered channels.
    pub fn list(&self) -> Vec<(ChannelType, ChannelStatus)> {
        self.channels
            .iter()
            .map(|e| (e.key().clone(), e.value().status.lock().clone()))
            .collect()
    }

    /// Return the number of registered channels.
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// Returns `true` if no channels are registered.
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
    use crate::{AgentOutput, Result as ChannelResult};
    use async_trait::async_trait;

    struct FakeAdapter {
        channel_type: ChannelType,
    }

    #[async_trait]
    impl ChannelAdapter for FakeAdapter {
        fn channel_type(&self) -> ChannelType {
            self.channel_type.clone()
        }

        async fn start(&self, _sender: mpsc::Sender<IncomingMessage>) -> ChannelResult<()> {
            Ok(())
        }

        async fn send(&self, _output: AgentOutput) -> ChannelResult<()> {
            Ok(())
        }

        fn approval_gate(&self) -> Option<std::sync::Arc<dyn aura_tools::ApprovalGate>> {
            None
        }

        async fn stop(&self) -> ChannelResult<()> {
            Ok(())
        }
    }

    fn fake(ct: ChannelType) -> Arc<dyn ChannelAdapter> {
        Arc::new(FakeAdapter { channel_type: ct })
    }

    #[test]
    fn register_and_get() {
        let reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::tui())).unwrap();
        assert!(reg.get_adapter(ChannelType::tui()).is_some());
        assert!(reg.get_adapter(ChannelType::http()).is_none());
    }

    #[test]
    fn duplicate_register_fails() {
        let reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::tui())).unwrap();
        let err = reg.register(fake(ChannelType::tui())).unwrap_err();
        assert!(matches!(err, ChannelError::DuplicateChannel(_)));
    }

    #[tokio::test]
    async fn unregister_removes_adapter() {
        let reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::http())).unwrap();
        reg.unregister(ChannelType::http()).await.unwrap();
        assert!(reg.get_adapter(ChannelType::http()).is_none());
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn unregister_not_found() {
        let reg = ChannelRegistry::new();
        let err = reg.unregister(ChannelType::tui()).await.unwrap_err();
        assert!(matches!(err, ChannelError::NotFound(_)));
    }

    #[tokio::test]
    async fn start_all_and_stop_all() {
        let reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::tui())).unwrap();
        reg.register(fake(ChannelType::http())).unwrap();

        let (tx, _rx) = mpsc::channel(16);
        reg.start_all(tx).await.unwrap();

        let statuses = reg.list();
        assert!(statuses.iter().all(|(_, s)| *s == ChannelStatus::Running));

        reg.stop_all().await;

        let statuses = reg.list();
        assert!(statuses.iter().all(|(_, s)| *s == ChannelStatus::Stopped));
    }

    #[test]
    fn list_returns_all_registered() {
        let reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::tui())).unwrap();
        reg.register(fake(ChannelType::http())).unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn default_is_empty() {
        let reg = ChannelRegistry::default();
        assert!(reg.is_empty());
    }
}
