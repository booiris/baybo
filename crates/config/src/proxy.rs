//! Optional outbound HTTP proxy.
//!
//! When set, every outbound HTTP call Aura makes — LLM providers, model
//! discovery, OpenRouter pricing, MCP HTTP servers, WebFetch, the CLI/TUI
//! gateway probes — and every spawned child that does its own HTTP (bun
//! channel sidecars, node MCP/browser stdio servers, external-agent CLIs)
//! routes through this proxy. Unset → direct connections (today's behavior).
//!
//! The *runtime* form is `aura_security::http::ProxySettings`; the boot layer
//! maps this config struct into it, keeping `aura-config` free of a `reqwest`
//! dependency.

use serde::{Deserialize, Serialize};

/// Supported proxy URL schemes (mirrors what `reqwest::Proxy::all` accepts
/// with the `socks` feature on).
const SUPPORTED_SCHEMES: &[&str] = &["http", "https", "socks5", "socks5h", "socks4", "socks4a"];

/// A single egress proxy applied to all outbound HTTP.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyConfig {
    /// Proxy URL applied to every scheme. Supports `http://`, `https://`,
    /// and `socks5://` (plus `socks5h`/`socks4`/`socks4a`), and may embed
    /// `user:pass@` credentials, e.g. `socks5://user:pass@host:1080`.
    pub url: String,

    /// Hosts/IPs/CIDRs/domain-suffixes that bypass the proxy (one entry per
    /// list element, e.g. `["example.com", ".internal"]`). Loopback
    /// (`localhost`, `127.0.0.1`, `::1`) is *always* kept direct regardless
    /// of this field, so the local gateway and loopback MCP/CDP endpoints
    /// keep working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<Vec<String>>,
}

impl ProxyConfig {
    /// The scheme portion (`socks5` in `socks5://…`), lower-cased, if the URL
    /// is well-formed enough to have one.
    pub fn scheme(&self) -> Option<String> {
        self.url
            .split_once("://")
            .map(|(s, _)| s.to_ascii_lowercase())
    }

    pub(crate) fn has_supported_scheme(&self) -> bool {
        self.scheme()
            .is_some_and(|s| SUPPORTED_SCHEMES.contains(&s.as_str()))
    }
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("url", &redact_credentials(&self.url))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// Replace any `user:pass@` userinfo with `[REDACTED]` so the proxy URL is
/// safe to log (it may carry credentials). Keeps scheme/host/port.
fn redact_credentials(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let redacted_rest = match authority.rsplit_once('@') {
        Some((_creds, host)) => format!("[REDACTED]@{host}{}", &rest[authority.len()..]),
        None => rest.to_string(),
    };
    match scheme {
        Some(s) => format!("{s}://{redacted_rest}"),
        None => redacted_rest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_no_proxy_is_elided() {
        let c = ProxyConfig {
            url: "http://proxy:3128".into(),
            no_proxy: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("no_proxy"), "None no_proxy elided: {json}");
    }

    #[test]
    fn round_trip() {
        let c = ProxyConfig {
            url: "socks5://host:1080".into(),
            no_proxy: Some(vec![".internal".into()]),
        };
        let back: ProxyConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn scheme_detection() {
        let c = ProxyConfig {
            url: "SOCKS5://Host:1080".into(),
            no_proxy: None,
        };
        assert_eq!(c.scheme().as_deref(), Some("socks5"));
        assert!(c.has_supported_scheme());
    }

    #[test]
    fn unsupported_scheme_detected() {
        let c = ProxyConfig {
            url: "ftp://host:21".into(),
            no_proxy: None,
        };
        assert!(!c.has_supported_scheme());
    }

    #[test]
    fn debug_redacts_credentials() {
        let c = ProxyConfig {
            url: "socks5://alice:s3cret@proxy:1080".into(),
            no_proxy: None,
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("s3cret"), "creds leaked: {shown}");
        assert!(shown.contains("[REDACTED]@proxy:1080"), "got: {shown}");
    }
}
