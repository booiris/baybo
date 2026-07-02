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

#[cfg(test)]
mod tests {
    use super::key_tag;

    #[test]
    fn key_tag_truncates_and_never_echoes_the_full_key() {
        assert_eq!(key_tag("abcdefgh-rest-stays-secret"), "abcdefgh…");
        assert_eq!(key_tag("short"), "short…");
        assert_eq!(key_tag(""), "…");
    }
}
