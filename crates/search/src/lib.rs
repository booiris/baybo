//! Web search for the agent loop: one `WebSearch` tool over a pluggable
//! provider.
//!
//! The crate lives outside `baybo-tools` for the same reason `baybo-cron`,
//! `baybo-task`, `baybo-deck` and `baybo-memory` do — it owns a domain with
//! its own configuration and credential. It is **not** because a tool holding
//! a vault or its own config cannot live in `baybo-tools`: `mcp/` in that
//! crate holds an `Arc<SecretVault>`, loads `.mcp.json`, and registers tools
//! dynamically. The reason is the dependency graph. Folding this in would
//! give `baybo-tools` a `baybo-config` edge, and `baybo-tools` has 16
//! dependents — a touch inside it rechecks 20 crates where a touch here
//! rechecks 2.
//!
//! Separately, and for a different reason, registration happens from
//! `crates/baybo/src/runtime.rs` rather than `default_tools`:
//! `DefaultToolsConfig` carries neither a `SecretVault` nor a `BayboConfig`,
//! and is built in argv mode before the vault is open. That settles the
//! *registration seam*, not the crate boundary. Argv-mode boots get no search
//! tool, which is correct: `baybo config get` never runs an agent loop.
//!
//! **The tool returns links and snippets, never page bodies.** Following a
//! result is `WebFetch`'s job, and `WebFetch` is where the SSRF floor, the
//! redirect pinning and the blob archive live. Expanding a result inline here
//! would put attacker-influenced content into the transcript through a client
//! that has none of that.

pub mod boot;
pub mod error;
pub mod providers;
pub mod tool;

use async_trait::async_trait;

pub use crate::error::SearchError;
pub use crate::tool::{WEB_SEARCH_TOOL_NAME, WebSearchTool};

/// One provider-neutral hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// Provider-reported recency, verbatim (`"2 days ago"`, `"2026-08-01"`).
    /// Free text on purpose: Brave sends `age`, SearXNG sends `publishedDate`,
    /// and Tavily sends a date only on its news topic. Parsing to a
    /// `DateTime` would buy a per-provider format table for a field the model
    /// reads as prose. `None` is the common case, so nothing may depend on it.
    pub age: Option<String>,
}

/// The intersection of knobs every candidate provider can honour.
///
/// Provider-specific dials belong in that provider's own constructor, never in
/// the schema the LLM sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub query: String,
    pub max_results: usize,
    pub domains: DomainFilter,
    pub freshness: Option<Freshness>,
    /// Region hint from the operator's config, not from the model.
    pub country: Option<String>,
    /// Language hint from the operator's config, not from the model.
    pub language: Option<String>,
}

/// Allow-list and block-list are different postures, so they are one enum
/// rather than two vectors that could both be populated. Providers encode
/// what they can natively; enforcement is the tool's, not theirs — see
/// [`DomainFilter::admits`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DomainFilter {
    #[default]
    Unrestricted,
    Only(Vec<String>),
    Except(Vec<String>),
}

impl DomainFilter {
    /// An allow-list. Entries are normalized to bare hosts; an entry that is
    /// not usable as one is dropped, and the count of dropped entries comes
    /// back so the caller can tell the model rather than silently filtering
    /// everything away.
    pub fn only(entries: &[String]) -> (Self, usize) {
        let (hosts, dropped) = normalize_all(entries);
        (Self::Only(hosts), dropped)
    }

    /// A block-list, normalized like [`Self::only`].
    pub fn except(entries: &[String]) -> (Self, usize) {
        let (hosts, dropped) = normalize_all(entries);
        (Self::Except(hosts), dropped)
    }

    /// Whether this is a list that ended up with nothing in it — every entry
    /// was unusable. Distinct from [`Self::Unrestricted`], which is the state
    /// of having asked for no filter at all.
    pub fn is_empty_list(&self) -> bool {
        match self {
            Self::Unrestricted => false,
            Self::Only(d) | Self::Except(d) => d.is_empty(),
        }
    }

    /// Whether a result URL survives this filter.
    ///
    /// Applied by the tool to every result from every provider, including the
    /// ones that claim to filter server-side. Brave can only express a domain
    /// filter as a `site:` operator, and it drops operators silently when the
    /// filtered set is small — so a provider's own filtering is a ranking
    /// hint here, never the guarantee the model was promised.
    pub fn admits(&self, url: &str) -> bool {
        if matches!(self, Self::Unrestricted) {
            return true;
        }
        // A URL that will not parse cannot be checked against either list, so
        // it fails closed rather than reaching the model as if it had been.
        let Some(host) = host_of(url) else {
            return false;
        };
        match self {
            Self::Unrestricted => true,
            Self::Only(domains) => domains.iter().any(|d| host_matches(&host, d)),
            Self::Except(domains) => !domains.iter().any(|d| host_matches(&host, d)),
        }
    }
}

fn normalize_all(entries: &[String]) -> (Vec<String>, usize) {
    let hosts: Vec<String> = entries.iter().filter_map(|e| normalize_domain(e)).collect();
    let dropped = entries.len() - hosts.len();
    (hosts, dropped)
}

/// Reduce whatever was written to the bare, punycoded, lowercased host it
/// names, or `None` when there is no host in it.
///
/// This exists because both sides of the filter get written by hand and both
/// fail silently without it. An operator's `blocked_domains` entry of
/// `https://spam.io` or `spam.io:8080` fails **open** — the policy the tool
/// calls un-widenable blocks nothing — and a model's `allowed_domains` entry
/// in the same shape fails closed, discarding every on-domain hit. IDN is the
/// same trap from the other end: `url::Url` hands back the A-label
/// (`xn--mnchen-3ya.de`), so a literal `münchen.de` in the config could never
/// match anything.
pub(crate) fn normalize_domain(entry: &str) -> Option<String> {
    let trimmed = entry
        .trim()
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .trim();
    if trimmed.is_empty() {
        return None;
    }
    // `Url::parse` is what performs the IDN → punycode conversion, and it
    // needs a scheme to parse an authority at all.
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let host = url::Url::parse(&candidate).ok()?.host_str()?.to_string();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Lowercased, punycoded host of `url`. Callers compare through
/// [`host_matches`], which handles the subdomain relation.
pub(crate) fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
}

/// Whether `host` is `domain` or a subdomain of it. Both are normalized
/// first, so this holds however the entry was written.
///
/// The dot boundary is the point: a plain `ends_with` would let
/// `evil-rust-lang.org` satisfy a filter for `rust-lang.org`.
pub(crate) fn host_matches(host: &str, domain: &str) -> bool {
    let Some(domain) = normalize_domain(domain) else {
        return false;
    };
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// How far back a result may have been published. Providers map this onto
/// their own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "day" => Some(Self::Day),
            "week" => Some(Self::Week),
            "month" => Some(Self::Month),
            "year" => Some(Self::Year),
            _ => None,
        }
    }
}

/// What one provider call produced, plus the transport facts the tool turns
/// into a `ToolEventPayload::HttpFetch`. Carrying them here keeps event
/// emission in one place instead of handing every provider an event sink.
#[derive(Debug, Clone)]
pub struct SearchOutcome {
    pub results: Vec<SearchResult>,
    pub status: u16,
    pub bytes: u64,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Stable slug for logs and trace events (`"tavily"`, `"brave"`).
    fn name(&self) -> &'static str;

    /// Issue one search.
    ///
    /// Implementations own request encoding and response mapping and **nothing
    /// else**. No deadline is passed: the tool wraps every call in one
    /// `tokio::time::timeout` raced against the turn's cancellation token, so
    /// a new provider cannot forget to honour a bound. Dropping the future
    /// cancels the in-flight request.
    async fn search(&self, query: &SearchQuery) -> Result<SearchOutcome, SearchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_matching_respects_the_dot_boundary() {
        assert!(host_matches("rust-lang.org", "rust-lang.org"));
        assert!(host_matches("blog.rust-lang.org", "rust-lang.org"));
        assert!(!host_matches("evil-rust-lang.org", "rust-lang.org"));
        assert!(!host_matches("rust-lang.org.evil.com", "rust-lang.org"));
    }

    #[test]
    fn host_matching_normalizes_operator_input() {
        assert!(host_matches("blog.rust-lang.org", "*.rust-lang.org"));
        assert!(host_matches("blog.rust-lang.org", "  RUST-LANG.ORG  "));
        assert!(!host_matches("blog.rust-lang.org", ""));
    }

    #[test]
    fn only_admits_listed_hosts_and_their_subdomains() {
        let f = DomainFilter::Only(vec!["rust-lang.org".into()]);
        assert!(f.admits("https://blog.rust-lang.org/2026/x"));
        assert!(!f.admits("https://example.com/x"));
    }

    #[test]
    fn except_rejects_listed_hosts() {
        let f = DomainFilter::Except(vec!["example.com".into()]);
        assert!(!f.admits("https://www.example.com/x"));
        assert!(f.admits("https://rust-lang.org/x"));
    }

    /// A result whose URL will not parse cannot be checked against a filter,
    /// so it must not be handed to the model as if it had been.
    #[test]
    fn an_unparsable_url_is_refused_by_every_filter() {
        assert!(!DomainFilter::Only(vec!["a.com".into()]).admits("not a url"));
        assert!(!DomainFilter::Except(vec!["a.com".into()]).admits("not a url"));
        assert!(DomainFilter::Unrestricted.admits("not a url"));
    }

    /// The fail-open case: an operator who pastes a URL into `blocked_domains`
    /// used to get a policy that blocked nothing, with no error anywhere.
    #[test]
    fn a_domain_written_as_a_url_or_with_a_port_still_blocks() {
        for entry in [
            "https://banned.example",
            "banned.example/path",
            "banned.example:8443",
            "http://banned.example:8443/x?y=1",
            "BANNED.example.",
        ] {
            let (f, dropped) = DomainFilter::except(&[entry.to_string()]);
            assert_eq!(dropped, 0, "{entry} should normalize");
            assert!(
                !f.admits("https://banned.example/a"),
                "entry {entry:?} did not block"
            );
        }
    }

    /// The mirror trap: `url::Url` hands back the A-label, so an operator's
    /// literal unicode entry could never match the host it names.
    #[test]
    fn an_idn_domain_matches_its_punycode_host() {
        assert_eq!(
            normalize_domain("münchen.de").as_deref(),
            Some("xn--mnchen-3ya.de")
        );
        let (f, _) = DomainFilter::except(&["münchen.de".to_string()]);
        assert!(!f.admits("https://münchen.de/x"));
        assert!(!f.admits("https://xn--mnchen-3ya.de/x"));
    }

    #[test]
    fn an_unusable_domain_entry_is_dropped_and_counted() {
        let (f, dropped) = DomainFilter::only(&["".into(), "  ".into(), "ok.com".into()]);
        assert_eq!(dropped, 2);
        assert_eq!(f, DomainFilter::Only(vec!["ok.com".into()]));
    }

    #[test]
    fn freshness_parses_the_schema_enum_only() {
        assert_eq!(Freshness::parse("week"), Some(Freshness::Week));
        assert_eq!(Freshness::parse("Week"), None);
        assert_eq!(Freshness::parse("decade"), None);
    }
}
