//! Conversation-title broadcaster.
//!
//! The agent's title pass (`baybo-agent`) generates a short conversation
//! title from a session's first user question and persists it, then hands
//! `(session_id, title)` to a [`baybo_agent::SessionTitleSink`] so a display
//! surface can push the change to its live clients. This is that sink for the
//! channel layer: a channel-wide `SessionUpdated { patch.title }` broadcast —
//! the same mechanism the pin / hide / folder mutations use (see
//! [`crate::api::admin::chat::broadcast_session_patch`]) and a sibling of the
//! [`super::session_pulse::SessionPulse`] activity broadcaster.
//!
//! Lives here (not in the agent layer) so the agent stays channel-agnostic:
//! it only knows the narrow trait, and the gateway decides how to fan out.

use std::sync::Arc;

use baybo_channels::ChannelRegistry;
use baybo_channels::wire::SessionPatch;
use baybo_model::SessionId;

/// Broadcasts a freshly-generated conversation title to **every installed
/// Subscribed channel** (the web `http` chat, the iOS app, …) as a
/// `SessionUpdated` patch, so whichever surface owns the session updates its
/// header and sidebar live without a list refetch. Titles are generic session
/// metadata generated for any channel's user session, so the broadcast is not
/// hardcoded to `http`: it fans out to each Subscribed channel. Non-Subscribed
/// channels (e.g. Telegram) carry no session-patch surface and are skipped,
/// and a patch for a session a given channel doesn't track is harmlessly
/// dropped by that channel's clients.
pub struct SessionTitleBroadcaster {
    channels: Arc<ChannelRegistry>,
}

impl SessionTitleBroadcaster {
    pub fn new(channels: Arc<ChannelRegistry>) -> Self {
        Self { channels }
    }
}

impl baybo_agent::SessionTitleSink for SessionTitleBroadcaster {
    fn title_updated(&self, session_id: &SessionId, title: &str) {
        let patch = SessionPatch {
            title: Some(title.to_string()),
            ..Default::default()
        };
        // `list()` snapshots the channel keys before `get()`, so this never
        // holds the registry's DashMap iterator across a lookup.
        for channel_type in self.channels.list() {
            let Some(channel) = self.channels.get(&channel_type) else {
                continue;
            };
            if let Some(sub) = channel.as_subscribed() {
                sub.broadcast_session_patch(session_id.clone(), patch.clone());
            }
        }
    }
}
