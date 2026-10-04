//! The freshness and replay rules an opened offer must pass before A answers
//! it. C can store and replay anything it forwards; the seal proves an offer
//! came from P, and these rules prove it is new.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use device_proto::candidates::{DeviceOffer, OfferId};
use parking_lot::Mutex;

/// How far an offer's `issued_at_ms` may lie from A's clock, either way.
pub(crate) const OFFER_MAX_AGE: Duration = Duration::from_secs(120);
/// Offer ids the gateway process remembers. When full, a new offer is
/// declined rather than an unexpired id evicted.
pub(crate) const MAX_REPLAY_ENTRIES: usize = 64;
/// How long an accepted offer's id is remembered, inclusive of its last
/// millisecond: an offer accepted at `a` is fresh no later than
/// `a + 2 × OFFER_MAX_AGE`, so it is always still in the cache while fresh.
const REPLAY_WINDOW: Duration = OFFER_MAX_AGE.saturating_mul(2);

/// The `direct_offer` log line's `outcome` for an offer A answered.
pub(crate) const ACCEPTED: &str = "accepted";

/// Why A declined an offer, as the `direct_offer` log line names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decline {
    /// The offer's seal did not open, or the answer could not be sealed.
    Auth,
    /// Issued too far from A's clock, or before the gateway process's
    /// carriers started.
    Stale,
    Replayed,
    /// The replay cache is full.
    OverCap,
    /// No socket or listener is bound, so there is nothing to answer with.
    /// An inactive runtime, whose binding has no candidate keys, binds none.
    Unbound,
}

impl Decline {
    pub(crate) fn outcome(self) -> &'static str {
        match self {
            Self::Auth => "declined:auth",
            Self::Stale => "declined:stale",
            Self::Replayed => "declined:replayed",
            Self::OverCap => "declined:over_cap",
            Self::Unbound => "declined:unbound",
        }
    }

    /// Whether the offer opened under the binding's key before it was
    /// declined, so it came from the paired device. Only such an outcome
    /// may replace the device's last offer: anyone holding the node id can
    /// make C forward garbage.
    pub(crate) fn authenticated(self) -> bool {
        match self {
            Self::Stale | Self::Replayed | Self::OverCap => true,
            Self::Auth | Self::Unbound => false,
        }
    }
}

/// The freshness rule and replay cache of a gateway process's carriers.
/// Every runtime of the process shares one (clones are the same gate), so a
/// Reconfigure, which starts a new runtime, never forgets an offer the last
/// one accepted. Every time is the same wall clock in unix milliseconds, so
/// an id always outlives the offer's own freshness, whatever that clock
/// does.
#[derive(Debug, Clone)]
pub(crate) struct OfferGate(Arc<Mutex<GateState>>);

#[derive(Debug)]
struct GateState {
    started_at_ms: u64,
    /// Accepted offer ids and the last time each must still be remembered.
    seen: HashMap<OfferId, u64>,
}

impl OfferGate {
    pub(crate) fn new(started_at_ms: u64) -> Self {
        Self(Arc::new(Mutex::new(GateState {
            started_at_ms,
            seen: HashMap::new(),
        })))
    }

    #[cfg(test)]
    pub(crate) fn started_at_ms(&self) -> u64 {
        self.0.lock().started_at_ms
    }

    /// Admits an opened offer at `now_ms` and remembers its id. The offer
    /// must be within [`OFFER_MAX_AGE`] of `now_ms`, issued no earlier than
    /// the gate was made, and not seen before.
    pub(crate) fn admit(&self, offer: &DeviceOffer, now_ms: u64) -> Result<(), Decline> {
        let mut gate = self.0.lock();
        if u128::from(now_ms.abs_diff(offer.issued_at_ms)) > OFFER_MAX_AGE.as_millis()
            || offer.issued_at_ms < gate.started_at_ms
        {
            return Err(Decline::Stale);
        }
        gate.seen
            .retain(|_, remember_until| *remember_until >= now_ms);
        if gate.seen.contains_key(&offer.offer_id) {
            return Err(Decline::Replayed);
        }
        if gate.seen.len() >= MAX_REPLAY_ENTRIES {
            return Err(Decline::OverCap);
        }
        let window = u64::try_from(REPLAY_WINDOW.as_millis()).unwrap_or(u64::MAX);
        gate.seen
            .insert(offer.offer_id, now_ms.saturating_add(window));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTED_AT_MS: u64 = 1_000_000;

    fn ms(duration: Duration) -> u64 {
        u64::try_from(duration.as_millis()).unwrap()
    }

    fn offer(issued_at_ms: u64) -> DeviceOffer {
        DeviceOffer::new(issued_at_ms, Vec::new())
    }

    #[test]
    fn an_offer_outside_the_clock_window_or_before_the_start_is_stale() {
        let gate = OfferGate::new(STARTED_AT_MS);
        let now = STARTED_AT_MS + ms(OFFER_MAX_AGE) * 3;
        let max_age = ms(OFFER_MAX_AGE);
        for stale in [now - max_age - 1, now + max_age + 1] {
            assert_eq!(gate.admit(&offer(stale), now), Err(Decline::Stale));
        }
        assert_eq!(
            gate.admit(&offer(STARTED_AT_MS - 1), STARTED_AT_MS),
            Err(Decline::Stale),
            "an offer issued before the gate existed"
        );
        assert_eq!(gate.admit(&offer(now - max_age), now), Ok(()));
        assert_eq!(gate.admit(&offer(now + max_age), now), Ok(()));
    }

    #[test]
    fn an_offer_id_is_accepted_once_for_the_replay_window() {
        let gate = OfferGate::new(STARTED_AT_MS);
        let first = offer(STARTED_AT_MS);
        assert_eq!(gate.admit(&first, STARTED_AT_MS), Ok(()));
        assert_eq!(
            gate.admit(&first, STARTED_AT_MS + ms(OFFER_MAX_AGE)),
            Err(Decline::Replayed)
        );
    }

    /// An offer issued `OFFER_MAX_AGE` ahead of A's clock is still fresh a
    /// whole replay window after A accepted it.
    #[test]
    fn an_offer_still_fresh_at_the_end_of_the_replay_window_is_still_a_replay() {
        let gate = OfferGate::new(STARTED_AT_MS);
        let ahead = offer(STARTED_AT_MS + ms(OFFER_MAX_AGE));
        assert_eq!(gate.admit(&ahead, STARTED_AT_MS), Ok(()));
        assert_eq!(
            gate.admit(&ahead, STARTED_AT_MS + ms(REPLAY_WINDOW)),
            Err(Decline::Replayed)
        );
        assert_eq!(
            gate.admit(&ahead, STARTED_AT_MS + ms(REPLAY_WINDOW) + 1),
            Err(Decline::Stale),
            "past the window the offer is no longer fresh"
        );
    }

    /// A clone is the same gate: an offer one runtime accepted is a replay
    /// to the next runtime of the process.
    #[test]
    fn every_clone_shares_one_replay_cache() {
        let gate = OfferGate::new(STARTED_AT_MS);
        let next_runtime = gate.clone();
        let accepted = offer(STARTED_AT_MS);
        assert_eq!(gate.admit(&accepted, STARTED_AT_MS), Ok(()));
        assert_eq!(
            next_runtime.admit(&accepted, STARTED_AT_MS + 1),
            Err(Decline::Replayed)
        );
    }

    #[test]
    fn a_full_cache_declines_new_offers_until_ids_expire() {
        let gate = OfferGate::new(STARTED_AT_MS);
        for _ in 0..MAX_REPLAY_ENTRIES {
            assert_eq!(gate.admit(&offer(STARTED_AT_MS), STARTED_AT_MS), Ok(()));
        }
        assert_eq!(
            gate.admit(&offer(STARTED_AT_MS), STARTED_AT_MS),
            Err(Decline::OverCap)
        );

        let last_remembered = STARTED_AT_MS + ms(REPLAY_WINDOW);
        assert_eq!(
            gate.admit(&offer(last_remembered), last_remembered),
            Err(Decline::OverCap)
        );
        let later = last_remembered + 1;
        assert_eq!(gate.admit(&offer(later), later), Ok(()));
    }

    #[test]
    fn every_decline_names_its_log_outcome() {
        let outcomes = [
            Decline::Auth,
            Decline::Stale,
            Decline::Replayed,
            Decline::OverCap,
            Decline::Unbound,
        ]
        .map(Decline::outcome);
        assert_eq!(
            outcomes,
            [
                "declined:auth",
                "declined:stale",
                "declined:replayed",
                "declined:over_cap",
                "declined:unbound"
            ]
        );
    }
}
