use serde::{Deserialize, Serialize};

/// Network access policy for sandboxed executions.
///
/// Follows deny-by-default: only domains explicitly listed in
/// `allowed_domains` may be accessed, and loopback must be
/// explicitly permitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkPolicy {
    /// Domains that sandboxed code is allowed to contact.
    pub allowed_domains: Vec<String>,
    /// Whether loopback (127.0.0.1 / localhost) access is permitted.
    pub allow_loopback: bool,
}

/// Proxy that enforces network access rules for sandboxed executions.
#[derive(Debug)]
pub struct NetworkProxy {
    policy: NetworkPolicy,
}

impl NetworkProxy {
    /// Create a new network proxy with the given policy.
    pub fn new(policy: NetworkPolicy) -> Self {
        Self { policy }
    }

    /// Check whether access to the given host and port is allowed.
    ///
    /// Returns `true` if the host matches an allowed domain or is
    /// a permitted loopback address.
    pub fn check_allowed(&self, host: &str, _port: u16) -> bool {
        if is_loopback(host) {
            return self.policy.allow_loopback;
        }

        self.policy
            .allowed_domains
            .iter()
            .any(|domain| host_matches_domain(host, domain))
    }

    /// Return a reference to the underlying policy.
    pub fn policy(&self) -> &NetworkPolicy {
        &self.policy
    }
}

/// Check whether a host string refers to a loopback address.
fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Check whether a host matches a domain pattern.
///
/// Supports exact match and suffix match (e.g. domain `example.com`
/// matches host `api.example.com`).
fn host_matches_domain(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    // Suffix match: host ends with ".domain"
    host.strip_suffix(domain)
        .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy_with(domains: Vec<&str>, loopback: bool) -> NetworkProxy {
        NetworkProxy::new(NetworkPolicy {
            allowed_domains: domains.into_iter().map(String::from).collect(),
            allow_loopback: loopback,
        })
    }

    #[test]
    fn denies_all_by_default() {
        let proxy = proxy_with(vec![], false);
        assert!(!proxy.check_allowed("example.com", 443));
        assert!(!proxy.check_allowed("localhost", 8080));
    }

    #[test]
    fn allows_exact_domain() {
        let proxy = proxy_with(vec!["api.example.com"], false);
        assert!(proxy.check_allowed("api.example.com", 443));
        assert!(!proxy.check_allowed("other.example.com", 443));
    }

    #[test]
    fn allows_subdomain_match() {
        let proxy = proxy_with(vec!["example.com"], false);
        assert!(proxy.check_allowed("example.com", 443));
        assert!(proxy.check_allowed("api.example.com", 443));
        assert!(!proxy.check_allowed("notexample.com", 443));
    }

    #[test]
    fn loopback_denied_when_disabled() {
        let proxy = proxy_with(vec![], false);
        assert!(!proxy.check_allowed("localhost", 80));
        assert!(!proxy.check_allowed("127.0.0.1", 80));
        assert!(!proxy.check_allowed("::1", 80));
    }

    #[test]
    fn loopback_allowed_when_enabled() {
        let proxy = proxy_with(vec![], true);
        assert!(proxy.check_allowed("localhost", 80));
        assert!(proxy.check_allowed("127.0.0.1", 80));
        assert!(proxy.check_allowed("::1", 80));
    }
}
