//! Channel kind: how a [`Channel`](crate::Channel) routes agent output
//! across its attached [`Connection`](crate::Connection)s.
//!
//! Names describe the connection's relationship to sessions, not the
//! direction of message flow: `Multiplexed` means one connection
//! carries every session of its channel_type; `Subscribed` means
//! connections receive only the sessions they explicitly subscribe to.
//!
//! Compile-time constant per channel-type implementation, not a config
//! field: the operator cannot meaningfully flip telegram-the-bot
//! between the two.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// One connection carries every session of this channel_type. Used
    /// by sidecar bots (telegram, weixin, discord) where a single
    /// subprocess multiplexes traffic for many platform users.
    /// `Subscribe` / `Unsubscribe` frames from a `Multiplexed`-channel
    /// connection are protocol errors.
    Multiplexed,
    /// Connections receive only the sessions they explicitly subscribe
    /// to via `Subscribe` / `Unsubscribe` frames. Used by the TUI (one
    /// subscription per process, the session the TUI owns) and the web
    /// chat page (the active view, switched on navigation).
    Subscribed,
}

impl ChannelKind {
    pub fn is_multiplexed(self) -> bool {
        matches!(self, ChannelKind::Multiplexed)
    }

    pub fn is_subscribed(self) -> bool {
        matches!(self, ChannelKind::Subscribed)
    }
}
