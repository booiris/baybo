//! The remote-host ("C") wire contract: route paths, the admission header, and
//! the serde types crossing the relay (WS) + push (HTTP) boundaries. Pure serde,
//! no transport and no baybo deps, so the server, the gateway, and the app all
//! depend on it instead of hand-mirroring the protocol.

pub mod push;
pub mod relay;

/// Join a base URL to a route path, trimming a trailing slash on the base.
fn join(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// Redacted log form of a credential-bearing identifier (a `remote_api_key`, a
/// content `relay_key`): its first 8 chars + `…`. Enough for the operator to
/// correlate a log line with the dashboard allow-list (or the peer's log) without
/// writing the full credential into a log file. Both sides of the wire log
/// through this so their tags match.
pub fn key_tag(key: &str) -> String {
    let head: String = key.chars().take(8).collect();
    format!("{head}…")
}

/// Max chars of an unvalidated `device_id` echoed into a log line — a well-formed
/// id is 68 chars (`ios-` + 64 hex); a pre-validation reject carries an arbitrary
/// attacker-controlled string, so bound it. The gateway and the push host both log
/// through [`device_id_log`] so the cap can't drift between the two records meant
/// to correlate.
const DEVICE_ID_LOG_MAX_CHARS: usize = 72;

/// Bounded, char-boundary-safe prefix of an unvalidated `device_id` for logging.
pub fn device_id_log(device_id: &str) -> String {
    device_id.chars().take(DEVICE_ID_LOG_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::{device_id_log, key_tag};

    #[test]
    fn key_tag_truncates_and_never_echoes_the_full_key() {
        assert_eq!(key_tag("abcdefgh-rest-stays-secret"), "abcdefgh…");
        assert_eq!(key_tag("short"), "short…");
        assert_eq!(key_tag(""), "…");
    }

    #[test]
    fn device_id_log_bounds_an_arbitrary_string_on_a_char_boundary() {
        assert_eq!(device_id_log("ios-abc"), "ios-abc");
        let long = "é".repeat(200);
        let logged = device_id_log(&long);
        assert_eq!(logged.chars().count(), super::DEVICE_ID_LOG_MAX_CHARS);
        // Truncation lands on a char boundary (no panic, valid UTF-8).
        assert!(logged.is_char_boundary(logged.len()));
    }
}
