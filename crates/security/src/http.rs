//! Outbound HTTP proxy plumbing.
//!
//! A single optional egress proxy applied uniformly to every outbound HTTP
//! call Baybo makes. [`ProxySettings`] is the runtime form of the operator's
//! `proxy` config; the boot layer maps `baybo_config::ProxyConfig` into it so
//! this crate stays free of a `baybo-config` dependency.
//!
//! Two consumption shapes:
//! - [`client`] / [`client_builder`] for in-process `reqwest` clients (LLM
//!   providers, MCP HTTP, WebFetch, pricing, the CLI/TUI gateway probes).
//! - [`ProxySettings::env_vars`] for spawned children (bun channel sidecars,
//!   node MCP/browser stdio servers, external-agent CLIs) whose env is
//!   scrubbed at exec — they can only learn the proxy through the standard
//!   `*_PROXY` variables.
//!
//! Loopback (`localhost`, `127.0.0.1`, `::1`) is always kept direct: the
//! TUI/CLI talk to the local gateway and MCP servers may be local, so routing
//! those through the egress proxy would break them.

/// Hosts that must never go through the proxy, regardless of operator config —
/// the local gateway and any loopback MCP / CDP endpoint.
const ALWAYS_DIRECT: &str = "localhost,127.0.0.1,::1";

/// Env vars naming the proxy for a TLS (`https` / `wss`) target, most specific
/// group first, each group in the spellings curl and reqwest accept.
const TLS_PROXY_ENV: [[&str; 2]; 2] = [["HTTPS_PROXY", "https_proxy"], ["ALL_PROXY", "all_proxy"]];

/// Env vars carrying the operator's no-proxy list (comma-separated).
const NO_PROXY_ENV: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// Runtime proxy configuration. Cheap to clone (two small strings). The
/// `url` may embed `user:pass@` credentials, so `Debug` redacts them — never
/// log the raw URL.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySettings {
    /// Proxy URL applied to all schemes, e.g. `http://host:3128`,
    /// `https://host:3128`, or `socks5://user:pass@host:1080`.
    pub url: String,
    /// Extra no-proxy entries (hosts, IPs, CIDRs, domain suffixes), one per
    /// element. [`ALWAYS_DIRECT`] is always merged in on top of this.
    pub no_proxy: Option<Vec<String>>,
}

impl ProxySettings {
    /// The ambient proxy for a TLS (`https` / `wss`) target, read through `get`
    /// (production passes `std::env::var`): `HTTPS_PROXY`, else `ALL_PROXY`, with
    /// `NO_PROXY` as the extra no-proxy list. For callers that need the proxy as
    /// a value — to name the route in logs and errors, or to decide per target
    /// whether it is proxied — instead of leaving reqwest to read the env
    /// implicitly. The operator's `proxy` block, when set, takes precedence.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        // Within a group the first variable that is SET wins, even when empty —
        // the precedence reqwest (hyper-util) applies to every other client.
        let first_set = |group: &[&str]| {
            group
                .iter()
                .find_map(|k| get(k))
                .filter(|v| !v.trim().is_empty())
        };
        let url = TLS_PROXY_ENV.iter().find_map(|group| first_set(group))?;
        let no_proxy = first_set(&NO_PROXY_ENV).map(|list| {
            list.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });
        Some(Self {
            url: url.trim().to_string(),
            no_proxy,
        })
    }

    /// The proxy URL with any `user:pass@` replaced by `[REDACTED]` — the only
    /// form of the URL that may reach a log line or an error message.
    pub fn display_url(&self) -> String {
        redact_credentials(&self.url)
    }

    /// Effective no-proxy list as the single comma-separated string reqwest's
    /// `NoProxy` and the `NO_PROXY` env var both expect: loopback first, then
    /// any operator additions (blank entries dropped).
    pub fn no_proxy_list(&self) -> String {
        let extra: Vec<&str> = self
            .no_proxy
            .iter()
            .flatten()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if extra.is_empty() {
            ALWAYS_DIRECT.to_string()
        } else {
            format!("{ALWAYS_DIRECT},{}", extra.join(","))
        }
    }

    /// Build the `reqwest::Proxy` for an in-process client. Credentials are
    /// parsed out of [`Self::url`] by `Proxy::all`.
    pub fn to_proxy(&self) -> reqwest::Result<reqwest::Proxy> {
        Ok(reqwest::Proxy::all(&self.url)?
            .no_proxy(reqwest::NoProxy::from_string(&self.no_proxy_list())))
    }

    /// Standard proxy environment variables for a spawned child process.
    /// Both upper- and lower-case spellings are emitted because different
    /// runtimes (bun, node, curl-family libs) read different cases.
    pub fn env_vars(&self) -> Vec<(String, String)> {
        let no_proxy = self.no_proxy_list();
        let mut out: Vec<(String, String)> = Vec::with_capacity(8);
        for key in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            out.push((key.to_string(), self.url.clone()));
        }
        out.push(("NO_PROXY".to_string(), no_proxy.clone()));
        out.push(("no_proxy".to_string(), no_proxy));
        out
    }
}

impl std::fmt::Debug for ProxySettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxySettings")
            .field("url", &redact_credentials(&self.url))
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// A `reqwest::ClientBuilder` pre-seeded with the proxy when one is
/// configured, or a plain builder otherwise. Callers layer their own
/// timeout / resolver / redirect policy on top, then `.build()`.
pub fn client_builder(proxy: Option<&ProxySettings>) -> reqwest::Result<reqwest::ClientBuilder> {
    let builder = reqwest::Client::builder();
    match proxy {
        Some(p) => Ok(builder.proxy(p.to_proxy()?)),
        None => Ok(builder),
    }
}

/// A ready `reqwest::Client` for callers that need no extra configuration.
pub fn client(proxy: Option<&ProxySettings>) -> reqwest::Result<reqwest::Client> {
    client_builder(proxy)?.build()
}

/// Replace any `user:pass@` userinfo in a proxy URL with `[REDACTED]` so it
/// is safe to log. Keeps scheme/host/port for diagnostics. Falls back to the
/// input unchanged when there is no userinfo to hide.
fn redact_credentials(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    // Userinfo ends at the LAST '@' before any query/fragment, not at the first
    // '/': a pasted token-style password may carry an unencoded '/', and a proxy
    // URL has no meaningful path to protect.
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let redacted_rest = match rest[..end].rfind('@') {
        Some(at) => format!("[REDACTED]@{}", &rest[at + 1..]),
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

    fn settings(url: &str, no_proxy: Option<Vec<&str>>) -> ProxySettings {
        ProxySettings {
            url: url.to_string(),
            no_proxy: no_proxy.map(|v| v.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn builder_without_proxy_is_ok() {
        assert!(client_builder(None).is_ok());
        assert!(client(None).is_ok());
    }

    #[test]
    fn builder_with_http_proxy_is_ok() {
        let p = settings("http://127.0.0.1:3128", None);
        assert!(client(Some(&p)).is_ok());
    }

    #[test]
    fn socks5_proxy_is_accepted() {
        // Requires reqwest's `socks` feature.
        let p = settings("socks5://user:pass@127.0.0.1:1080", None);
        assert!(p.to_proxy().is_ok());
        assert!(client(Some(&p)).is_ok());
    }

    #[test]
    fn env_vars_cover_both_cases_and_keep_loopback_direct() {
        let p = settings("http://proxy.internal:3128", Some(vec![".corp.example"]));
        let vars = p.env_vars();
        let get = |k: &str| {
            vars.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            get("HTTP_PROXY").as_deref(),
            Some("http://proxy.internal:3128")
        );
        assert_eq!(
            get("http_proxy").as_deref(),
            Some("http://proxy.internal:3128")
        );
        assert_eq!(
            get("HTTPS_PROXY").as_deref(),
            Some("http://proxy.internal:3128")
        );
        assert_eq!(
            get("ALL_PROXY").as_deref(),
            Some("http://proxy.internal:3128")
        );
        let no_proxy = get("NO_PROXY").expect("NO_PROXY present");
        assert!(no_proxy.contains("localhost"));
        assert!(no_proxy.contains("127.0.0.1"));
        assert!(no_proxy.contains(".corp.example"));
        assert_eq!(get("no_proxy"), get("NO_PROXY"));
    }

    #[test]
    fn loopback_merged_when_no_extra_no_proxy() {
        let p = settings("http://proxy:3128", None);
        let no_proxy = p
            .env_vars()
            .into_iter()
            .find(|(k, _)| k == "NO_PROXY")
            .map(|(_, v)| v)
            .expect("NO_PROXY present");
        assert_eq!(no_proxy, ALWAYS_DIRECT);
    }

    #[test]
    fn debug_redacts_credentials() {
        let p = settings("socks5://alice:s3cret@proxy.host:1080", None);
        let shown = format!("{p:?}");
        assert!(!shown.contains("s3cret"), "creds leaked: {shown}");
        assert!(!shown.contains("alice"), "creds leaked: {shown}");
        assert!(shown.contains("[REDACTED]@proxy.host:1080"), "got: {shown}");
        assert!(shown.contains("socks5://"), "scheme dropped: {shown}");
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn from_env_is_none_without_a_tls_proxy_var() {
        assert_eq!(ProxySettings::from_env(env_of(&[])), None);
        // A plain-http-only proxy doesn't apply to a TLS target.
        assert_eq!(
            ProxySettings::from_env(env_of(&[("HTTP_PROXY", "http://p:3128")])),
            None
        );
        assert_eq!(
            ProxySettings::from_env(env_of(&[("HTTPS_PROXY", "  ")])),
            None
        );
        // A set-but-empty upper-case spelling shadows the lower-case one.
        assert_eq!(
            ProxySettings::from_env(env_of(&[
                ("HTTPS_PROXY", ""),
                ("https_proxy", "http://p:3128")
            ])),
            None
        );
    }

    #[test]
    fn from_env_prefers_https_proxy_over_all_proxy() {
        let p = ProxySettings::from_env(env_of(&[
            ("ALL_PROXY", "socks5h://s:1080"),
            ("https_proxy", "http://h:3128"),
        ]))
        .expect("proxy");
        assert_eq!(p.url, "http://h:3128");

        let p =
            ProxySettings::from_env(env_of(&[("all_proxy", "socks5h://s:1080")])).expect("proxy");
        assert_eq!(p.url, "socks5h://s:1080");
    }

    #[test]
    fn from_env_carries_no_proxy_and_keeps_loopback_direct() {
        let p = ProxySettings::from_env(env_of(&[
            ("HTTPS_PROXY", "http://h:3128"),
            ("no_proxy", " .corp.example, ,10.0.0.0/8 "),
        ]))
        .expect("proxy");
        assert_eq!(
            p.no_proxy,
            Some(vec![".corp.example".to_string(), "10.0.0.0/8".to_string()])
        );
        assert_eq!(
            p.no_proxy_list(),
            format!("{ALWAYS_DIRECT},.corp.example,10.0.0.0/8")
        );
    }

    #[test]
    fn display_url_redacts_credentials() {
        let p = settings("http://alice:s3cret@proxy.host:3128", None);
        assert_eq!(p.display_url(), "http://[REDACTED]@proxy.host:3128");
        let p = settings("socks5h://proxy.host:1080", None);
        assert_eq!(p.display_url(), "socks5h://proxy.host:1080");
        // A '/' inside the password must not end the userinfo early.
        let p = settings("http://alice:pa/ss@proxy.host:3128", None);
        assert_eq!(p.display_url(), "http://[REDACTED]@proxy.host:3128");
        let p = settings("alice:pa/ss@proxy.host:3128", None);
        assert_eq!(p.display_url(), "[REDACTED]@proxy.host:3128");
    }

    #[test]
    fn debug_leaves_credential_free_url_intact() {
        let p = settings("http://proxy.host:3128", None);
        assert!(format!("{p:?}").contains("http://proxy.host:3128"));
    }
}
