//! Gateway-side [`baybo_deck::DeckEvents`] implementation: deck pushes
//! ride the owner channel like every other live chat-list signal.

use std::sync::Arc;

use baybo_channels::ChannelRegistry;
use baybo_model::ChannelType;

/// Broadcasts deck frames to every owner-channel connection. The owner
/// channel is shared (web + device); clients without deck UI ignore the
/// frames by contract (`crates/wire`).
pub struct GatewayDeckEvents {
    registry: Arc<ChannelRegistry>,
}

impl GatewayDeckEvents {
    pub fn new(registry: Arc<ChannelRegistry>) -> Self {
        Self { registry }
    }

    fn with_owner(&self, f: impl FnOnce(baybo_channels::SubscribedView<'_>)) {
        if let Some(channel) = self.registry.get(&ChannelType::owner())
            && let Some(sub) = channel.as_subscribed()
        {
            f(sub);
        }
    }
}

impl baybo_deck::DeckEvents for GatewayDeckEvents {
    fn card_data(&self, card_id: &str, seq: i64, payload: &str) {
        // The wire counter is u32 (TS BigInt avoidance); a card would
        // need one accepted emit per second for ~136 years to overflow.
        let Ok(seq) = u32::try_from(seq) else {
            tracing::warn!(card = %card_id, seq, "deck: seq exceeds wire range; dropping push");
            return;
        };
        self.with_owner(|sub| {
            sub.broadcast_deck_card_data(card_id.to_string(), seq, payload.to_string());
        });
    }

    fn deck_changed(&self) {
        self.with_owner(|sub| sub.broadcast_deck_changed());
    }
}
