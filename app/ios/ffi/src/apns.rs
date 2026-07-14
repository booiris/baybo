//! The APNs token + environment shared across the legs. The Tauri shell held
//! the token in a process global captured by an injected app-delegate method;
//! under SwiftUI the real `didRegisterForRemoteNotifications` delivers it, and
//! Swift hands it in through `BayboClient::set_apns_token` — this is where it
//! lands. Read by pairing (`DeviceHello.apns_token`), relay's best-effort APNs
//! refresh API call, and direct push registration.

use parking_lot::Mutex;

use crate::api::ApnsEnvironment;

/// What the gateway knows about this device's APNs token, and what is on its way
/// there. One lock over both, because every decision reads them together.
#[derive(Default)]
struct Binding {
    /// The token the gateway is known to hold.
    posted: Option<String>,
    /// Whether a POST is on the wire right now.
    ///
    /// At most one, ever. The refresh fires once per resident chat store on a
    /// foreground, so without a reservation the FIRST foreground after a token
    /// arrives sends a dozen identical POSTs at once — none of them has completed,
    /// so every caller still sees `posted != token`. And two in flight can land out
    /// of order across a rotation, leaving the gateway bound to the token iOS has
    /// already retired.
    posting: bool,
}

pub(crate) struct ApnsState {
    token: Mutex<Option<String>>,
    binding: Mutex<Binding>,
    env: ApnsEnvironment,
}

impl ApnsState {
    pub(crate) fn new(env: ApnsEnvironment) -> Self {
        Self {
            token: Mutex::new(None),
            binding: Mutex::new(Binding::default()),
            env,
        }
    }

    /// Store the (lowercase-hex) APNs device token. Empty strings are treated
    /// as "no token" so a defensive Swift caller can't wedge an empty binding.
    pub(crate) fn set_token(&self, token_hex: String) {
        let trimmed = token_hex.trim().to_string();
        *self.token.lock() = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
    }

    pub(crate) fn token(&self) -> Option<String> {
        self.token.lock().clone()
    }

    /// Claim the right to tell the gateway the current token.
    ///
    /// `None` means there is nothing to do: the gateway already has this token, or a
    /// POST is already carrying it (or an older one) there. The reservation is what
    /// makes at-most-one true — see `Binding::posting`.
    pub(crate) fn claim_post(&self) -> Option<String> {
        let token = self.token()?;
        let mut binding = self.binding.lock();
        if binding.posting || binding.posted.as_ref() == Some(&token) {
            return None;
        }
        binding.posting = true;
        Some(token)
    }

    /// The POST finished. On success the gateway now holds `token`; on failure the
    /// reservation simply lifts, and the next caller (or the chase in
    /// `refresh_relay_apns_best_effort`) tries again.
    pub(crate) fn finish_post(&self, token: String, delivered: bool) {
        let mut binding = self.binding.lock();
        binding.posting = false;
        if delivered {
            binding.posted = Some(token);
        }
    }

    pub(crate) fn env(&self) -> ApnsEnvironment {
        self.env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ApnsState {
        ApnsState::new(ApnsEnvironment::Sandbox)
    }

    /// The refresh fires on every foreground and once per resident chat store
    /// reconnecting. In the steady state it must send NOTHING — otherwise a single
    /// foreground puts a dozen identical POSTs on the wire, in front of the chat leg
    /// the user is waiting on.
    #[test]
    fn a_token_the_gateway_already_has_is_not_claimed_again() {
        let state = state();
        state.set_token("abc123".into());

        let first = state.claim_post().expect("the gateway has not been told");
        assert_eq!(first, "abc123");
        state.finish_post(first, true);

        assert_eq!(state.claim_post(), None, "the steady state sends nothing");
    }

    /// THE reason the reservation exists. A foreground reconnects up to a dozen
    /// resident stores at once, and every one of them asks. Until the first POST
    /// lands, none of them can see that the gateway is being told — so without the
    /// reservation, all twelve send the same thing.
    #[test]
    fn only_one_post_is_ever_in_flight() {
        let state = state();
        state.set_token("abc123".into());

        let claimed: Vec<_> = (0..12).filter_map(|_| state.claim_post()).collect();

        assert_eq!(claimed.len(), 1, "eleven callers must find nothing to do");
    }

    /// A rotated token must get through. iOS reissues on restore-from-backup and on
    /// reinstall, and a device whose token the gateway never learns stops buzzing.
    #[test]
    fn a_rotated_token_is_posted() {
        let state = state();
        state.set_token("abc123".into());
        let first = state.claim_post().expect("first");
        state.finish_post(first, true);

        state.set_token("def456".into());

        assert_eq!(state.claim_post().as_deref(), Some("def456"));
    }

    /// A rotation DURING a POST must not race it. Two in flight can land out of
    /// order and leave the gateway bound to the token iOS has already retired; the
    /// new one is claimed only once the old one is off the wire.
    #[test]
    fn a_rotation_mid_flight_waits_its_turn() {
        let state = state();
        state.set_token("abc123".into());
        let in_flight = state.claim_post().expect("first");

        state.set_token("def456".into());
        assert_eq!(
            state.claim_post(),
            None,
            "the newer token must not overtake the older one on the wire"
        );

        state.finish_post(in_flight, true);
        assert_eq!(
            state.claim_post().as_deref(),
            Some("def456"),
            "and it is picked up the moment the wire is free"
        );
    }

    /// A failed POST releases the reservation without recording delivery, so the
    /// next attempt retries the same token.
    #[test]
    fn a_failed_post_is_retried() {
        let state = state();
        state.set_token("abc123".into());
        let attempt = state.claim_post().expect("first");

        state.finish_post(attempt, false);

        assert_eq!(state.claim_post().as_deref(), Some("abc123"));
    }

    #[test]
    fn no_token_means_nothing_to_post() {
        assert_eq!(state().claim_post(), None);
    }
}
