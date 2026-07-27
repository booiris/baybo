//! Parse a `baybo://pair?h=<relay>&r=<rendezvous-id>&s=<secret>&k=<remote-api-key>`
//! QR payload. Both `r` (public rendezvous id) and `s` (the 256-bit secret, the
//! Noise PSK) are required — there is no typeable fallback, because a short
//! secret would be offline-crackable by a hostile relay. `h` and `k` are
//! optional; a missing `h` falls back to [`DEFAULT_ENDPOINT`].

use crate::api::PairTarget;

/// The hosted relay a QR without an explicit `h=` pairs against.
pub(crate) const DEFAULT_ENDPOINT: &str = "wss://proxy.baybo.space";

const PAIR_SCHEME_PREFIX: &str = "baybo://pair";

/// Parse a scanned QR payload into a [`PairTarget`]. `None` = not a pairing QR
/// (the scan screen keeps scanning). Deliberately LENIENT, because a QR that a
/// gateway really emitted must never fail to scan over a cosmetic difference:
/// scheme matching is case-insensitive, a fragment is stripped, and a malformed
/// percent escape is kept literal rather than rejecting the payload.
pub(crate) fn parse_pair_qr(text: &str) -> Option<PairTarget> {
    let text = text.trim();
    if text.len() < PAIR_SCHEME_PREFIX.len()
        || !text[..PAIR_SCHEME_PREFIX.len()].eq_ignore_ascii_case(PAIR_SCHEME_PREFIX)
    {
        return None;
    }
    let rest = &text[PAIR_SCHEME_PREFIX.len()..];
    let rest = rest.split('#').next().unwrap_or(rest);
    // Only `baybo://pair` itself, optionally followed by a query (or a `/?`
    // path-form some URL builders emit).
    let query = match rest.strip_prefix('/').unwrap_or(rest) {
        "" => "",
        q => q.strip_prefix('?')?,
    };

    let mut endpoint = None;
    let mut rendezvous_id = None;
    let mut secret = None;
    let mut remote_api_key = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match key {
            "h" => endpoint = Some(value),
            "r" => rendezvous_id = Some(value),
            "s" => secret = Some(value),
            "k" => remote_api_key = Some(value),
            _ => {}
        }
    }

    let rendezvous_id = rendezvous_id.filter(|r| !r.is_empty())?;
    let secret = secret.filter(|s| !s.is_empty())?;
    Some(PairTarget {
        endpoint: endpoint
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string()),
        rendezvous_id,
        secret,
        remote_api_key: remote_api_key.filter(|k| !k.is_empty()),
    })
}

/// Minimal percent-decoding for QR query values (`%XX` + `+` as space —
/// mirroring `URLSearchParams`, which the webview parser used). Infallible like
/// the original: a malformed escape stays literal, invalid UTF-8 is replaced.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => match (bytes.get(i + 1), bytes.get(i + 2)) {
                (Some(&hi), Some(&lo)) if hi.is_ascii_hexdigit() && lo.is_ascii_hexdigit() => {
                    out.push(hex_nibble(hi) << 4 | hex_nibble(lo));
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Value of an ASCII hex digit (caller guarantees `is_ascii_hexdigit`).
fn hex_nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        _ => b.to_ascii_lowercase() - b'a' + 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_payload() {
        let t = parse_pair_qr("baybo://pair?h=wss%3A%2F%2Frelay.example&r=rv1&s=aabb&k=key1")
            .expect("parses");
        assert_eq!(t.endpoint, "wss://relay.example");
        assert_eq!(t.rendezvous_id, "rv1");
        assert_eq!(t.secret, "aabb");
        assert_eq!(t.remote_api_key.as_deref(), Some("key1"));
    }

    #[test]
    fn missing_endpoint_falls_back_to_default() {
        let t = parse_pair_qr("baybo://pair?r=rv1&s=aabb").expect("parses");
        assert_eq!(t.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(t.remote_api_key, None);
    }

    #[test]
    fn requires_rendezvous_and_secret() {
        assert!(parse_pair_qr("baybo://pair?r=rv1").is_none());
        assert!(parse_pair_qr("baybo://pair?s=aabb").is_none());
        assert!(parse_pair_qr("baybo://pair").is_none());
        assert!(parse_pair_qr("https://example.com").is_none());
    }

    #[test]
    fn a_cosmetically_odd_payload_still_scans() {
        // Case-insensitive scheme.
        let t = parse_pair_qr("BAYBO://pair?r=rv1&s=aabb").expect("parses");
        assert_eq!(t.rendezvous_id, "rv1");
        // Fragments are stripped, not folded into the last value.
        let t = parse_pair_qr("baybo://pair?r=rv1&s=aabb#frag").expect("parses");
        assert_eq!(t.secret, "aabb");
        // A malformed escape in an IGNORED param must not reject the payload.
        let t = parse_pair_qr("baybo://pair?r=rv1&s=aabb&x=%zz").expect("parses");
        assert_eq!(t.secret, "aabb");
        // A malformed escape in a used value stays literal.
        let t = parse_pair_qr("baybo://pair?r=rv%1&s=aabb").expect("parses");
        assert_eq!(t.rendezvous_id, "rv%1");
    }
}
