//! Shared URL parsing + SSRF policy. Used by `WebFetch` and the
//! `browser_navigate` tool. Pure refactor of the helpers that originally
//! lived in `web_fetch.rs`; error messages are kept caller-neutral so
//! each tool can prefix its own name (`WebFetch: ...`,
//! `browser_navigate: ...`).

use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

pub(crate) fn host_to_literal_ip(host: &str) -> Option<IpAddr> {
    if let Ok(addr) = host.parse::<IpAddr>() {
        return Some(addr);
    }
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .and_then(|s| s.parse::<IpAddr>().ok())
}

pub(crate) fn validate_url_with(s: &str, allow_loopback: bool) -> Result<url::Url, String> {
    let parsed = url::Url::parse(s).map_err(|e| format!("invalid url `{s}`: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!("scheme `{other}` not allowed (use http or https)"));
        }
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?;
    let host_lc = host.to_ascii_lowercase();
    if host_lc.is_empty() {
        return Err("empty host".to_string());
    }
    if !allow_loopback
        && (host_lc == "localhost"
            || host_lc == "localhost.localdomain"
            || host_lc.ends_with(".localhost"))
    {
        return Err(format!("host `{host}` blocked"));
    }
    if let Some(addr) = host_to_literal_ip(&host_lc)
        && aura_security::is_blocked_ip(&addr, allow_loopback)
    {
        return Err(format!("ip `{addr}` blocked"));
    }
    Ok(parsed)
}

pub(crate) struct SafeResolver {
    pub(crate) allow_loopback: bool,
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let host = name.as_str().to_string();
            let lookup = format!("{host}:0");
            let resolved: Vec<SocketAddr> = tokio::net::lookup_host(lookup)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            let safe: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|sa| !aura_security::is_blocked_ip(&sa.ip(), allow_loopback))
                .collect();
            if safe.is_empty() {
                return Err(format!("host `{host}` resolved only to blocked IP ranges").into());
            }
            let iter: Addrs = Box::new(safe.into_iter());
            Ok(iter)
        })
    }
}
