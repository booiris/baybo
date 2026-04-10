use std::collections::HashMap;

use aura_session::ChannelType;
use tokio::sync::mpsc;

use crate::{ChannelAdapter, ChannelError, ChannelStatus, IncomingMessage, Result};

struct ChannelEntry {
    adapter: Box<dyn ChannelAdapter>,
    status: ChannelStatus,
}

/// Central registry for channel adapters.
///
/// Manages registration, lookup, and lifecycle (start/stop) of all channel
/// adapters. Replaces the raw `Vec<Box<dyn ChannelAdapter>>` that was
/// previously passed directly to the router.
pub struct ChannelRegistry {
    channels: HashMap<ChannelType, ChannelEntry>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
        }
    }

    /// Register a channel adapter. Fails if the channel type is already registered.
    pub fn register(&mut self, adapter: Box<dyn ChannelAdapter>) -> Result<()> {
        let channel_type = adapter.channel_type();
        if self.channels.contains_key(&channel_type) {
            return Err(ChannelError::DuplicateChannel(channel_type.to_string()));
        }
        self.channels.insert(
            channel_type,
            ChannelEntry {
                adapter,
                status: ChannelStatus::Registered,
            },
        );
        tracing::info!(%channel_type, "channel registered");
        Ok(())
    }

    /// Remove a channel adapter. Stops it first if it is running.
    pub async fn unregister(&mut self, channel_type: ChannelType) -> Result<()> {
        let entry = self
            .channels
            .remove(&channel_type)
            .ok_or_else(|| ChannelError::NotFound(channel_type.to_string()))?;

        if entry.status == ChannelStatus::Running
            && let Err(e) = entry.adapter.stop().await
        {
            tracing::warn!(%channel_type, error = %e, "error stopping channel during unregister");
        }
        tracing::info!(%channel_type, "channel unregistered");
        Ok(())
    }

    /// Look up a running adapter by channel type.
    pub fn get(&self, channel_type: ChannelType) -> Option<&dyn ChannelAdapter> {
        self.channels.get(&channel_type).map(|e| e.adapter.as_ref())
    }

    /// Start all registered channels that are not already running.
    pub async fn start_all(&mut self, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        for (channel_type, entry) in &mut self.channels {
            if entry.status == ChannelStatus::Running {
                continue;
            }
            match entry.adapter.start(sender.clone()).await {
                Ok(()) => {
                    entry.status = ChannelStatus::Running;
                    tracing::info!(%channel_type, "channel started");
                }
                Err(e) => {
                    let reason = e.to_string();
                    entry.status = ChannelStatus::Error(reason);
                    tracing::error!(%channel_type, error = %e, "failed to start channel");
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Stop all running channels. Continues on individual failures.
    pub async fn stop_all(&mut self) {
        for (channel_type, entry) in &mut self.channels {
            if entry.status != ChannelStatus::Running {
                continue;
            }
            match entry.adapter.stop().await {
                Ok(()) => {
                    entry.status = ChannelStatus::Stopped;
                    tracing::info!(%channel_type, "channel stopped");
                }
                Err(e) => {
                    entry.status = ChannelStatus::Error(e.to_string());
                    tracing::error!(%channel_type, error = %e, "failed to stop channel");
                }
            }
        }
    }

    /// Return the status of all registered channels.
    pub fn list(&self) -> Vec<(ChannelType, &ChannelStatus)> {
        self.channels
            .iter()
            .map(|(ct, entry)| (*ct, &entry.status))
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
    use crate::{OutgoingMessage, Result as ChannelResult};
    use async_trait::async_trait;

    struct FakeAdapter {
        channel_type: ChannelType,
    }

    #[async_trait]
    impl ChannelAdapter for FakeAdapter {
        fn channel_type(&self) -> ChannelType {
            self.channel_type
        }

        async fn start(&self, _sender: mpsc::Sender<IncomingMessage>) -> ChannelResult<()> {
            Ok(())
        }

        async fn send_response(&self, _response: OutgoingMessage) -> ChannelResult<()> {
            Ok(())
        }

        async fn stop(&self) -> ChannelResult<()> {
            Ok(())
        }
    }

    fn fake(ct: ChannelType) -> Box<dyn ChannelAdapter> {
        Box::new(FakeAdapter { channel_type: ct })
    }

    #[test]
    fn register_and_get() {
        let mut reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::Cli)).unwrap();
        assert!(reg.get(ChannelType::Cli).is_some());
        assert!(reg.get(ChannelType::Http).is_none());
    }

    #[test]
    fn duplicate_register_fails() {
        let mut reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::Cli)).unwrap();
        let err = reg.register(fake(ChannelType::Cli)).unwrap_err();
        assert!(matches!(err, ChannelError::DuplicateChannel(_)));
    }

    #[tokio::test]
    async fn unregister_removes_adapter() {
        let mut reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::Http)).unwrap();
        reg.unregister(ChannelType::Http).await.unwrap();
        assert!(reg.get(ChannelType::Http).is_none());
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn unregister_not_found() {
        let mut reg = ChannelRegistry::new();
        let err = reg.unregister(ChannelType::Cli).await.unwrap_err();
        assert!(matches!(err, ChannelError::NotFound(_)));
    }

    #[tokio::test]
    async fn start_all_and_stop_all() {
        let mut reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::Cli)).unwrap();
        reg.register(fake(ChannelType::Http)).unwrap();

        let (tx, _rx) = mpsc::channel(16);
        reg.start_all(tx).await.unwrap();

        let statuses = reg.list();
        assert!(statuses.iter().all(|(_, s)| **s == ChannelStatus::Running));

        reg.stop_all().await;

        let statuses = reg.list();
        assert!(statuses.iter().all(|(_, s)| **s == ChannelStatus::Stopped));
    }

    #[test]
    fn list_returns_all_registered() {
        let mut reg = ChannelRegistry::new();
        reg.register(fake(ChannelType::Cli)).unwrap();
        reg.register(fake(ChannelType::Http)).unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn default_is_empty() {
        let reg = ChannelRegistry::default();
        assert!(reg.is_empty());
    }
}
