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

    /// Drop the registration **only if** the currently-stored sender
    /// matches `tx`. A blind `remove(channel_type)` would evict the
    /// live entry when an old WS task fires its cleanup *after* a
    /// reconnect already overwrote the slot — the live sidecar would
    /// then go silent on `StartBot` / `StopBot` until the next bounce.
    /// `mpsc::Sender::same_channel` compares the underlying queue, so
    /// a clone of the live sender will still match and a stale one
    /// won't. Returns `true` iff the entry was removed; callers chain
    /// reconciler `forget` on the same gate so a stale cleanup can't
    /// clobber the live seed.
    pub fn unregister_if_owned(
        &self,
        channel_type: &ChannelType,
        tx: &mpsc::Sender<Frame>,
    ) -> bool {
        self.senders
            .remove_if(channel_type, |_, stored| stored.same_channel(tx))
            .is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unregister_if_owned_removes_only_matching_sender() {
        let reg = ChannelControlRegistry::new();
        let ct = ChannelType::telegram();
        let (tx_a, _rx_a) = mpsc::channel(4);
        let (tx_b, _rx_b) = mpsc::channel(4);

        // First WS comes up.
        reg.register(ct.clone(), tx_a.clone());
        assert!(reg.is_connected(&ct));

        // Second WS reconnects fast and overwrites the slot.
        reg.register(ct.clone(), tx_b.clone());

        // The old WS's cleanup now fires with its stale sender — it
        // must *not* evict the live entry. This is the exact reconnect
        // race the connection-scoped unregister was added to fix.
        let removed = reg.unregister_if_owned(&ct, &tx_a);
        assert!(!removed, "stale unregister must not match");
        assert!(reg.is_connected(&ct), "live entry survives stale cleanup");

        // The live WS's own cleanup (with the matching sender) evicts.
        let removed = reg.unregister_if_owned(&ct, &tx_b);
        assert!(removed, "owning unregister returns true");
        assert!(!reg.is_connected(&ct));
    }

    #[test]
    fn unregister_if_owned_matches_clone_of_same_sender() {
        let reg = ChannelControlRegistry::new();
        let ct = ChannelType::telegram();
        let (tx, _rx) = mpsc::channel(4);
        // The route hands `frame_tx_clone()` (a clone) to register and
        // a separate clone on exit; both must compare equal via
        // `same_channel`.
        reg.register(ct.clone(), tx.clone());
        assert!(reg.unregister_if_owned(&ct, &tx.clone()));
    }
}
