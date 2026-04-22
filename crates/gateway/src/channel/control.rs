//! Per-connection control-plane handles for registered sidecars.
//!
//! The agent's output path already has a home: `AgentOutput`s flow
//! through the [`aura_channels::Channel`] + its `mpsc::Sender<AgentOutput>`.
//! But the admin surface needs to push **raw wire frames** (specifically
//! `Frame::StartBot` / `Frame::StopBot`) to the currently-connected
//! sidecar, bypassing `AgentOutput` entirely. This registry hands out
//! the adapter's `mpsc::Sender<Frame>` so the admin thread can reach
//! into the WS pump without going through agent output.
//!
//! One entry per sidecar `ChannelType`. Session-scoped clients (the
//! TUI) don't participate — only sidecars that multiplex tenants on
//! their end care about `StartBot`/`StopBot`.

use aura_channels::wire::Frame;
use aura_model::ChannelType;
use dashmap::DashMap;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum ChannelControlError {
    #[error("no sidecar currently connected for channel_type '{0}'")]
    NotConnected(String),

    #[error("sidecar for channel_type '{0}' has disconnected")]
    Closed(String),
}

#[derive(Default)]
pub struct ChannelControlRegistry {
    senders: DashMap<ChannelType, mpsc::Sender<Frame>>,
}

impl ChannelControlRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the sidecar's outbound frame mpsc so the admin thread
    /// can later push control frames to it. Replacing a previously
    /// registered entry is intentional — [`aura_channels::ChannelRegistry`]
    /// already rejects duplicate sidecars at the same layer, and if
    /// somehow two connections get past that guard the newer pump is
    /// the one still live.
    pub fn register(&self, channel_type: ChannelType, tx: mpsc::Sender<Frame>) {
        self.senders.insert(channel_type, tx);
    }

    pub fn unregister(&self, channel_type: &ChannelType) {
        self.senders.remove(channel_type);
    }

    /// Push `frame` into the sidecar's outbound pump. Returns
    /// `NotConnected` when no sidecar is registered for `channel_type`
    /// and `Closed` when the pump has already torn down.
    pub async fn send(
        &self,
        channel_type: &ChannelType,
        frame: Frame,
    ) -> Result<(), ChannelControlError> {
        let sender = {
            let Some(entry) = self.senders.get(channel_type) else {
                return Err(ChannelControlError::NotConnected(channel_type.to_string()));
            };
            entry.value().clone()
        };
        sender
            .send(frame)
            .await
            .map_err(|_| ChannelControlError::Closed(channel_type.to_string()))
    }

    pub fn is_connected(&self, channel_type: &ChannelType) -> bool {
        self.senders.contains_key(channel_type)
    }

    /// Snapshot every currently-connected sidecar's `ChannelType`.
    /// Used by the reconciler to iterate live sidecars without
    /// holding a DashMap guard across awaits.
    pub fn connected_channel_types(&self) -> Vec<ChannelType> {
        self.senders
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }
}
