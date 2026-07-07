//! Which gateway binding the app currently holds — the single source of truth for
//! the active chat/blob leg.
//!
//! "One app binds one Baybo": at most one of a **direct login** (URL + admin token)
//! or a **relay pairing** (scan-to-pair record) is live at a time — each supersedes
//! the other's credentials at write time (`direct::login` → `relay::forget_pairing`;
//! `relay::finish_pair` → `keychain::delete_direct_credentials`). So the leg every
//! chat/blob command routes to is a pure function of durable keychain state; the
//! caller never tags it.

use crate::{direct, keychain, relay};

/// The unbound-app error, stable because [`crate::api::BayboError::from_msg`]
/// matches it to fold the string into `BayboError::NotBound`.
pub(crate) const NOT_BOUND_MSG: &str = "not connected — pair or sign in first";

/// The leg the app's current binding routes to.
pub(crate) enum ActiveLeg {
    Relay,
    Direct,
}

/// The active-binding marker value each bind site persists (via
/// [`keychain::store_active_binding`]) and the resolver reads back — the single
/// source of truth for the two leg names crossing the keychain boundary.
pub(crate) const RELAY_MARKER: &str = "relay";
pub(crate) const DIRECT_MARKER: &str = "direct";

/// Resolve the active leg from durable identity. Errors when the app is unbound
/// (neither a direct login nor a relay pairing) — an unbound call fails loudly
/// instead of misrouting to a leg with no credentials.
///
/// Both credentials existing at once is reachable only transiently, if a best-effort
/// supersede delete hiccups. The active-binding marker records which bind happened
/// last, so the tie breaks toward the leg the user actually intended rather than a
/// static precedence (which could route to the superseded gateway). A legacy install
/// with no marker falls back to direct.
pub(crate) fn active_leg() -> Result<ActiveLeg, String> {
    match (direct::has_credentials()?, relay::has_pairing()?) {
        (true, false) => Ok(ActiveLeg::Direct),
        (false, true) => Ok(ActiveLeg::Relay),
        (false, false) => Err(NOT_BOUND_MSG.into()),
        (true, true) => {
            if keychain::read_active_binding()?.as_deref() == Some(RELAY_MARKER) {
                Ok(ActiveLeg::Relay)
            } else {
                // Marker says direct, or is absent (a legacy install): default direct.
                Ok(ActiveLeg::Direct)
            }
        }
    }
}
