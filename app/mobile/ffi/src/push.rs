//! Provider-tagged push token state shared across transport legs.

use parking_lot::Mutex;

use crate::api::PushToken;

/// What the gateway is known to hold and whether an update is currently in
/// flight. One lock covers both because every claim reads them together.
#[derive(Default)]
struct Binding {
    posted: Option<PushToken>,
    posting: bool,
    generation: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PushClaim {
    pub(crate) token: PushToken,
    generation: u64,
}

pub(crate) struct PushState {
    token: Mutex<Option<PushToken>>,
    binding: Mutex<Binding>,
}

impl PushState {
    pub(crate) fn new() -> Self {
        Self {
            token: Mutex::new(None),
            binding: Mutex::new(Binding::default()),
        }
    }

    pub(crate) fn set_token(&self, token: PushToken) {
        *self.token.lock() = token.normalized();
    }

    pub(crate) fn token(&self) -> Option<PushToken> {
        self.token.lock().clone()
    }

    /// Claim the right to post the current target to the gateway. At most one
    /// update is in flight, preventing an older token from racing a rotation.
    pub(crate) fn claim_post(&self) -> Option<PushClaim> {
        let token = self.token()?;
        let mut binding = self.binding.lock();
        if binding.posting || binding.posted.as_ref() == Some(&token) {
            return None;
        }
        binding.posting = true;
        Some(PushClaim {
            token,
            generation: binding.generation,
        })
    }

    pub(crate) fn finish_post(&self, claim: PushClaim, delivered: bool) {
        let mut binding = self.binding.lock();
        binding.posting = false;
        if delivered && claim.generation == binding.generation {
            binding.posted = Some(claim.token);
        }
    }

    /// A new paired gateway cannot inherit the old gateway's posted-target
    /// cache. The generation also prevents an old in-flight response from
    /// repopulating that cache after the binding changes.
    pub(crate) fn invalidate_gateway(&self) {
        let mut binding = self.binding.lock();
        binding.generation = binding.generation.wrapping_add(1);
        binding.posted = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApnsEnvironment;

    fn apns(token: &str) -> PushToken {
        PushToken::Apns {
            token: token.to_string(),
            environment: ApnsEnvironment::Sandbox,
        }
    }

    #[test]
    fn an_unchanged_target_is_not_posted_again() {
        let state = PushState::new();
        state.set_token(apns("abc123"));
        let first = state.claim_post().expect("first update");
        state.finish_post(first, true);
        assert_eq!(state.claim_post(), None);
    }

    #[test]
    fn only_one_update_is_in_flight() {
        let state = PushState::new();
        state.set_token(apns("abc123"));
        assert_eq!((0..12).filter_map(|_| state.claim_post()).count(), 1);
    }

    #[test]
    fn a_provider_or_token_change_is_posted() {
        let state = PushState::new();
        state.set_token(apns("abc123"));
        let first = state.claim_post().expect("first update");
        state.finish_post(first, true);

        state.set_token(PushToken::Fcm {
            token: "def456".into(),
        });
        assert_eq!(
            state.claim_post().map(|claim| claim.token),
            Some(PushToken::Fcm {
                token: "def456".into(),
            })
        );
    }

    #[test]
    fn a_change_mid_flight_waits_for_the_first_update() {
        let state = PushState::new();
        state.set_token(apns("abc123"));
        let first = state.claim_post().expect("first update");
        state.set_token(apns("def456"));
        assert_eq!(state.claim_post(), None);
        state.finish_post(first, true);
        assert_eq!(
            state.claim_post().map(|claim| claim.token),
            Some(apns("def456"))
        );
    }

    #[test]
    fn failed_updates_are_retried_and_empty_tokens_are_ignored() {
        let state = PushState::new();
        state.set_token(apns("abc123"));
        let first = state.claim_post().expect("first update");
        state.finish_post(first, false);
        assert_eq!(
            state.claim_post().map(|claim| claim.token),
            Some(apns("abc123"))
        );

        let empty = PushState::new();
        empty.set_token(apns("   "));
        assert_eq!(empty.claim_post(), None);
    }

    #[test]
    fn a_new_gateway_invalidates_the_posted_target_and_old_in_flight_result() {
        let state = PushState::new();
        state.set_token(apns("abc123"));
        let old_gateway = state.claim_post().expect("old gateway update");
        state.invalidate_gateway();
        state.finish_post(old_gateway, true);

        assert_eq!(
            state.claim_post().map(|claim| claim.token),
            Some(apns("abc123")),
            "the new gateway still needs the same target"
        );
    }
}
