//! Brave — `GET /res/v1/web/search`, `X-Subscription-Token` header.
//!
//! Brave has no native domain parameter: a domain filter can only be
//! expressed as `site:` / `-site:` operators inside `q`. Those operators are
//! documented as experimental, multi-`site:` OR is unreliable, and Brave
//! *silently drops* them when too few results match. So they go out as a
//! ranking hint and the guarantee the model was given is enforced on the
//! results by [`crate::tool`] — never here.
//!
//! `q` is capped at 400 characters and 50 words. Operators eat into that
//! budget, so they are appended only while they fit.

use async_trait::async_trait;
use baybo_security::http::ProxySettings;
use serde::Deserialize;

use crate::error::SearchError;
use crate::providers::{append_site_operators, build_client, endpoint, http_error, read_body};
use crate::{Freshness, SearchOutcome, SearchProvider, SearchQuery, SearchResult};

pub const PROVIDER_NAME: &str = "brave";

const DEFAULT_BASE_URL: &str = "https://api.search.brave.com";
const SEARCH_PATH: &str = "res/v1/web/search";
const API_KEY_HEADER: &str = "X-Subscription-Token";

/// Brave's documented ceilings on `q`. Exceeding either is a 422 that neither
/// the model nor the operator can act on, so the operator-hint half of the
/// query is dropped instead of sent.
const MAX_QUERY_CHARS: usize = 400;
const MAX_QUERY_WORDS: usize = 50;

pub struct BraveProvider {
    client: reqwest::Client,
    endpoint: url::Url,
    api_key: String,
}

impl BraveProvider {
    pub fn new(
        api_key: String,
        base_url: Option<&str>,
        proxy: Option<&ProxySettings>,
    ) -> Result<Self, SearchError> {
        Ok(Self {
            client: build_client(proxy)?,
            endpoint: endpoint(base_url, DEFAULT_BASE_URL, SEARCH_PATH)?,
            api_key,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    web: Web,
}

#[derive(Debug, Default, Deserialize)]
struct Web {
    #[serde(default)]
    results: Vec<Hit>,
}

#[derive(Debug, Deserialize)]
struct Hit {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    description: String,
    /// Human-readable recency ("2 days ago"). `page_age` is the ISO fallback.
    #[serde(default)]
    age: Option<String>,
    #[serde(default)]
    page_age: Option<String>,
}

fn freshness_code(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Day => "pd",
        Freshness::Week => "pw",
        Freshness::Month => "pm",
        Freshness::Year => "py",
    }
}

/// The query Brave is actually sent: the model's text, plus as many `site:`
/// operators as fit inside Brave's documented limits.
fn bounded_query(query: &SearchQuery) -> String {
    let hinted = append_site_operators(&query.query, &query.domains);
    if hinted.chars().count() <= MAX_QUERY_CHARS
        && hinted.split_whitespace().count() <= MAX_QUERY_WORDS
    {
        return hinted;
    }
    // The hint is optional; the query is not. Drop the operators rather than
    // send a request Brave will refuse.
    query
        .query
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .split_whitespace()
        .take(MAX_QUERY_WORDS)
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl SearchProvider for BraveProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchOutcome, SearchError> {
        let count = query.max_results.to_string();
        let mut params: Vec<(&str, String)> = vec![("q", bounded_query(query)), ("count", count)];
        if let Some(f) = query.freshness {
            params.push(("freshness", freshness_code(f).to_string()));
        }
        if let Some(country) = &query.country {
            params.push(("country", country.clone()));
        }
        if let Some(lang) = &query.language {
            params.push(("search_lang", lang.clone()));
        }

        let response = self
            .client
            .get(self.endpoint.clone())
            .header(API_KEY_HEADER, &self.api_key)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&params)
            .send()
            .await
            .map_err(|e| SearchError::from_transport(&e))?;

        let status = response.status();
        if !status.is_success() {
            return Err(http_error(response).await);
        }
        let raw = read_body(response).await?;
        let bytes = raw.len() as u64;
        let parsed: Response = serde_json::from_str(&raw).map_err(|e| SearchError::Decode {
            reason: e.to_string(),
        })?;

        Ok(SearchOutcome {
            results: parsed
                .web
                .results
                .into_iter()
                .map(|h| SearchResult {
                    title: h.title,
                    url: h.url,
                    snippet: h.description,
                    age: h.age.or(h.page_age),
                })
                .collect(),
            status: status.as_u16(),
            bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DomainFilter;
    use crate::providers::test_support::{TestServer, query_for};

    #[tokio::test]
    async fn maps_a_response_to_normalized_results() {
        let server = TestServer::json(
            200,
            r#"{"web":{"results":[
                {"title":"Rust 1.98","url":"https://blog.rust-lang.org/x","description":"snippet","age":"2 days ago"},
                {"title":"Older","url":"https://example.com/y","description":"other","page_age":"2026-01-02T00:00:00"}
            ]}}"#,
        )
        .await;
        let provider =
            BraveProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let outcome = provider.search(&query_for("rust")).await.expect("search");

        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].snippet, "snippet");
        assert_eq!(outcome.results[0].age.as_deref(), Some("2 days ago"));
        // `age` is absent on the second hit, so `page_age` stands in.
        assert_eq!(
            outcome.results[1].age.as_deref(),
            Some("2026-01-02T00:00:00")
        );
    }

    #[tokio::test]
    async fn a_response_with_no_web_block_is_empty_not_an_error() {
        let server = TestServer::json(200, r#"{"query":{"original":"rust"}}"#).await;
        let provider =
            BraveProvider::new("k".into(), Some(&server.base_url()), None).expect("build");
        let outcome = provider.search(&query_for("rust")).await.expect("search");
        assert!(outcome.results.is_empty());
    }

    #[tokio::test]
    async fn sends_the_key_as_a_header_never_in_the_url() {
        let server = TestServer::json(200, r#"{"web":{"results":[]}}"#).await;
        let provider = BraveProvider::new("sentinel-key".into(), Some(&server.base_url()), None)
            .expect("build");
        provider.search(&query_for("rust")).await.expect("search");

        let req = server.last_request();
        assert_eq!(
            req.headers
                .get(&API_KEY_HEADER.to_ascii_lowercase())
                .map(String::as_str),
            Some("sentinel-key")
        );
        assert!(!req.uri.contains("sentinel-key"), "uri = {}", req.uri);
    }

    #[tokio::test]
    async fn domain_filters_ride_along_as_site_operators() {
        let server = TestServer::json(200, r#"{"web":{"results":[]}}"#).await;
        let provider =
            BraveProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let mut q = query_for("rust");
        q.domains = DomainFilter::Only(vec!["rust-lang.org".into()]);
        provider.search(&q).await.expect("search");
        assert_eq!(
            server.last_request().params().get("q").map(String::as_str),
            Some("rust site:rust-lang.org")
        );

        let mut q = query_for("rust");
        q.domains = DomainFilter::Except(vec!["spam.io".into()]);
        provider.search(&q).await.expect("search");
        assert_eq!(
            server.last_request().params().get("q").map(String::as_str),
            Some("rust -site:spam.io")
        );
    }

    #[tokio::test]
    async fn freshness_and_locale_map_onto_brave_codes() {
        let server = TestServer::json(200, r#"{"web":{"results":[]}}"#).await;
        let provider =
            BraveProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let mut q = query_for("rust");
        q.freshness = Some(Freshness::Week);
        q.country = Some("jp".into());
        q.language = Some("ja".into());
        provider.search(&q).await.expect("search");

        let params = server.last_request().params();
        assert_eq!(params.get("freshness").map(String::as_str), Some("pw"));
        assert_eq!(params.get("country").map(String::as_str), Some("jp"));
        assert_eq!(params.get("search_lang").map(String::as_str), Some("ja"));
        assert_eq!(params.get("count").map(String::as_str), Some("8"));
    }

    /// A long allow-list would push `q` past Brave's 400-char / 50-word cap
    /// and earn a 422. The operators are a hint; the query is the request.
    #[tokio::test]
    async fn an_oversized_hint_is_dropped_rather_than_sent() {
        let server = TestServer::json(200, r#"{"web":{"results":[]}}"#).await;
        let provider =
            BraveProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let mut q = query_for("rust");
        q.domains = DomainFilter::Only(
            (0..40)
                .map(|i| format!("averyveryverylongdomainname{i}.example.com"))
                .collect(),
        );
        provider.search(&q).await.expect("search");

        let sent = server
            .last_request()
            .params()
            .get("q")
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            sent, "rust",
            "the hint must be dropped, not truncated mid-operator"
        );
        assert!(sent.chars().count() <= MAX_QUERY_CHARS);
    }

    #[tokio::test]
    async fn a_non_2xx_carries_status_and_byte_count() {
        let server = TestServer::json(401, r#"{"error":"bad token"}"#).await;
        let provider =
            BraveProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        match provider.search(&query_for("rust")).await.expect_err("401") {
            SearchError::Http { status, bytes, .. } => {
                assert_eq!(status, 401);
                assert!(bytes > 0);
            }
            other => panic!("expected Http, got {other}"),
        }
    }
}
