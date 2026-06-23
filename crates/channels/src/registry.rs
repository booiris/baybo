//! Central registry for live channels.
//!
//! After the Channel / Connection / Subscription refactor the registry
//! is a thin map. Channels are eagerly created at gateway boot from
//! `ChannelsConfig` and installed once; connections live inside each
//! [`Channel`] (`channel.attach` / `channel.detach`) and are not the
//! registry's concern.
//!
//! See [`docs/modules/channels.md`](../../../docs/modules/channels.md)
//! for the surrounding model.

use std::sync::Arc;

use baybo_model::ChannelType;
use baybo_tools::ApprovalGateMap;
use dashmap::DashMap;

use crate::{Channel, ChannelError, Result};

pub struct ChannelRegistry {
    channels: DashMap<ChannelType, Arc<Channel>>,
    /// Per-channel approval gates collected at install time. Shared
    /// with `ToolExecutor` so the tool path can look up the right gate
    /// per call without re-traversing the registry.
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
    /// `ToolExecutor` at bootstrap.
    pub fn approval_gates(&self) -> Arc<ApprovalGateMap> {
        Arc::clone(&self.gate_map)
    }

    /// Install a channel at boot. Errors if the channel type is
    /// already installed (config bug — the operator declared the same
    /// channel twice).
    pub fn install(&self, channel: Arc<Channel>) -> Result<()> {
        let channel_type = channel.channel_type().clone();
        if self.channels.contains_key(&channel_type) {
            return Err(ChannelError::DuplicateChannel(channel_type.to_string()));
        }
        if let Some(gate) = channel.approval_gate() {
            self.gate_map.insert(channel_type.clone(), gate);
        }
        let key = channel_type.clone();
        self.channels.insert(channel_type, channel);
        tracing::info!(channel_type = %key, "channel installed");
        Ok(())
    }

    /// Look up a channel by type.
    pub fn get(&self, channel_type: &ChannelType) -> Option<Arc<Channel>> {
        self.channels.get(channel_type).map(|e| Arc::clone(&e))
    }

    /// All installed channel types.
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
    use crate::connection::{Connection, ConnectionSink, SendOutcome};
    use crate::kind::ChannelKind;
    use crate::types::SessionEvent;
    use crate::wire::Frame;
    use baybo_model::SessionId;

    struct DiscardSink;
    impl ConnectionSink for DiscardSink {
        fn try_send_event(&self, _event: SessionEvent) -> SendOutcome {
            SendOutcome::Sent
        }
        fn try_send_frame(&self, _frame: Frame) -> SendOutcome {
            SendOutcome::Sent
        }
    }

    fn channel(ct: ChannelType, kind: ChannelKind) -> Arc<Channel> {
        Arc::new(Channel::new(ct, kind, None))
    }

    #[test]
    fn install_and_get() {
        let reg = ChannelRegistry::new();
        let ch = channel(ChannelType::http(), ChannelKind::Subscribed);
        reg.install(Arc::clone(&ch)).unwrap();
        assert!(Arc::ptr_eq(&reg.get(&ChannelType::http()).unwrap(), &ch));
    }

    #[test]
    fn duplicate_install_fails() {
        let reg = ChannelRegistry::new();
        let a = channel(ChannelType::tui(), ChannelKind::Subscribed);
        let b = channel(ChannelType::tui(), ChannelKind::Subscribed);
        reg.install(a).unwrap();
        let err = reg.install(b).unwrap_err();
        assert!(matches!(err, ChannelError::DuplicateChannel(_)));
    }

    #[test]
    fn list_returns_installed() {
        let reg = ChannelRegistry::new();
        reg.install(channel(ChannelType::http(), ChannelKind::Subscribed))
            .unwrap();
        reg.install(channel(ChannelType::tui(), ChannelKind::Subscribed))
            .unwrap();
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn channel_attach_and_dispatch_selective() {
        let ch = channel(ChannelType::http(), ChannelKind::Subscribed);
        let conn = Arc::new(Connection::new(Arc::new(DiscardSink)));
        let id = conn.id();
        ch.attach(Arc::clone(&conn));
        ch.as_subscribed()
            .unwrap()
            .subscribe(id, "sess-a".into())
            .unwrap();
        assert!(ch.has_subscribers(&SessionId::from("sess-a")));
        assert!(!ch.has_subscribers(&SessionId::from("sess-b")));
    }

    #[test]
    fn detach_clears_subscriptions() {
        let ch = channel(ChannelType::http(), ChannelKind::Subscribed);
        let conn = Arc::new(Connection::new(Arc::new(DiscardSink)));
        let id = conn.id();
        ch.attach(Arc::clone(&conn));
        ch.as_subscribed()
            .unwrap()
            .subscribe(id, "sess-a".into())
            .unwrap();
        ch.detach(id);
        assert!(!ch.has_subscribers(&SessionId::from("sess-a")));
        assert_eq!(ch.connection_count(), 0);
    }

    #[test]
    fn subscribe_unreachable_on_broadcast_channel() {
        // Subscribe is now an operation on `SubscribedView`, which is
        // unobtainable from a Multiplexed channel. The runtime
        // `WrongKind` error variant the previous version returned is
        // structurally unreachable. We lock in the new invariant by
        // asserting `as_subscribed()` returns `None`.
        let ch = channel(ChannelType::telegram(), ChannelKind::Multiplexed);
        assert!(
            ch.as_subscribed().is_none(),
            "Multiplexed channels expose no Subscribed view, so subscribe is uncallable",
        );
    }
}
