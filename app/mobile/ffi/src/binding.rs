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
pub(crate) fn active_leg() -> Result<ActiveLeg, String> {
    let has_direct = direct::has_credentials()?;
    let has_relay = relay::has_pairing()?;
    // The marker only breaks the both-present tie, so a single-credential app
    // never pays for the extra keychain read.
    let marker = match (has_direct, has_relay) {
        (true, true) => keychain::read_active_binding()?,
        _ => None,
    };
    resolve_leg(has_direct, has_relay, marker.as_deref())
}

/// The routing decision itself, over durable state already read.
///
/// Both credentials existing at once is reachable only transiently, if a best-effort
/// supersede delete hiccups. The active-binding marker records which bind happened
/// last, so the tie breaks toward the leg the user actually intended rather than a
/// static precedence (which could route to the superseded gateway). A legacy install
/// with no marker — or one whose marker doesn't name relay — falls back to direct.
fn resolve_leg(
    has_direct: bool,
    has_relay: bool,
    marker: Option<&str>,
) -> Result<ActiveLeg, String> {
    match (has_direct, has_relay) {
        (true, false) => Ok(ActiveLeg::Direct),
        (false, true) => Ok(ActiveLeg::Relay),
        (false, false) => Err(NOT_BOUND_MSG.into()),
        (true, true) if marker == Some(RELAY_MARKER) => Ok(ActiveLeg::Relay),
        (true, true) => Ok(ActiveLeg::Direct),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_login_alone_routes_direct() {
        assert!(matches!(
            resolve_leg(true, false, None),
            Ok(ActiveLeg::Direct)
        ));
        // A stale relay marker cannot misroute an app that holds only direct
        // credentials — the marker is a tie-breaker, never a router.
        assert!(matches!(
            resolve_leg(true, false, Some(RELAY_MARKER)),
            Ok(ActiveLeg::Direct)
        ));
    }

    #[test]
    fn a_relay_pairing_alone_routes_relay() {
        assert!(matches!(
            resolve_leg(false, true, None),
            Ok(ActiveLeg::Relay)
        ));
    }

    #[test]
    fn an_unbound_app_fails_loudly() {
        assert_eq!(
            resolve_leg(false, false, None).err().as_deref(),
            Some(NOT_BOUND_MSG)
        );
    }

    /// Reachable in production: both bind sites supersede the other's credentials
    /// best-effort, so a keychain hiccup leaves both present. The marker names the
    /// bind that actually happened last.
    #[test]
    fn both_credentials_break_toward_the_marked_leg() {
        assert!(matches!(
            resolve_leg(true, true, Some(RELAY_MARKER)),
            Ok(ActiveLeg::Relay)
        ));
        assert!(matches!(
            resolve_leg(true, true, Some(DIRECT_MARKER)),
            Ok(ActiveLeg::Direct)
        ));
    }

    #[test]
    fn both_credentials_with_no_marker_fall_back_to_direct() {
        assert!(matches!(
            resolve_leg(true, true, None),
            Ok(ActiveLeg::Direct)
        ));
    }

    #[test]
    fn both_credentials_with_an_unknown_marker_fall_back_to_direct() {
        assert!(matches!(
            resolve_leg(true, true, Some("relay-v2")),
            Ok(ActiveLeg::Direct)
        ));
    }

    /// The markers cross the keychain boundary as bytes; a rename here silently
    /// re-resolves every both-present install to the superseded leg.
    #[test]
    fn the_marker_spellings_are_frozen() {
        assert_eq!(RELAY_MARKER, "relay");
        assert_eq!(DIRECT_MARKER, "direct");
    }
}
