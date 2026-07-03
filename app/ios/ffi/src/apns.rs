//! The APNs token + environment shared across the legs. The Tauri shell held
//! the token in a process global captured by an injected app-delegate method;
//! under SwiftUI the real `didRegisterForRemoteNotifications` delivers it, and
//! Swift hands it in through `BayboClient::set_apns_token` — this is where it
//! lands. Read by pairing (`DeviceHello.apns_token`), the relay chat leg (the
//! best-effort `UpdateApnsToken` opening frame), and direct push registration.

use parking_lot::Mutex;

use crate::api::ApnsEnvironment;

pub(crate) struct ApnsState {
    token: Mutex<Option<String>>,
    env: ApnsEnvironment,
}

impl ApnsState {
    pub(crate) fn new(env: ApnsEnvironment) -> Self {
        Self {
            token: Mutex::new(None),
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

    pub(crate) fn env(&self) -> ApnsEnvironment {
        self.env
    }
}
