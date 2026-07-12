//! The APNs token + environment shared across the legs. The Tauri shell held
//! the token in a process global captured by an injected app-delegate method;
//! under SwiftUI the real `didRegisterForRemoteNotifications` delivers it, and
//! Swift hands it in through `BayboClient::set_apns_token` — this is where it
//! lands. Read by pairing (`DeviceHello.apns_token`), relay's best-effort APNs
//! refresh API call, and direct push registration.

use parking_lot::Mutex;

use crate::api::ApnsEnvironment;

pub(crate) struct ApnsState {
    token: Mutex<Option<String>>,
    /// The token the gateway has already been told about. In the steady state it
    /// equals `token`, and the refresh becomes a no-op — which matters because the
    /// refresh is fired on every foreground, once per resident chat store, so
    /// without this it is a dozen identical POSTs racing each other.
    posted: Mutex<Option<String>>,
    env: ApnsEnvironment,
}

impl ApnsState {
    pub(crate) fn new(env: ApnsEnvironment) -> Self {
        Self {
            token: Mutex::new(None),
            posted: Mutex::new(None),
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

    /// The token to send, or `None` if the gateway already has this one.
    pub(crate) fn token_needing_post(&self) -> Option<String> {
        let token = self.token()?;
        if self.posted.lock().as_ref() == Some(&token) {
            return None;
        }
        Some(token)
    }

    /// The gateway acknowledged this token; stop re-posting it.
    pub(crate) fn mark_posted(&self, token: String) {
        *self.posted.lock() = Some(token);
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
    fn a_token_the_gateway_already_has_is_not_posted_again() {
        let state = state();
        state.set_token("abc123".into());

        let first = state
            .token_needing_post()
            .expect("the gateway has not been told");
        assert_eq!(first, "abc123");
        state.mark_posted(first);

        assert_eq!(
            state.token_needing_post(),
            None,
            "the steady state sends nothing"
        );
    }

    /// A rotated token must get through. iOS reissues on restore-from-backup and on
    /// reinstall, and a device whose token the gateway never learns stops buzzing.
    #[test]
    fn a_rotated_token_is_posted() {
        let state = state();
        state.set_token("abc123".into());
        state.mark_posted("abc123".into());

        state.set_token("def456".into());

        assert_eq!(state.token_needing_post().as_deref(), Some("def456"));
    }

    /// A failed post must not be remembered as sent — `mark_posted` is only called
    /// on the gateway's acknowledgement.
    #[test]
    fn no_token_means_nothing_to_post() {
        assert_eq!(state().token_needing_post(), None);
    }
}
