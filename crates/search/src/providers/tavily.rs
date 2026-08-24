//! Tavily — `POST /search`, `Authorization: Bearer`.
//!
//! The only shipped provider with native domain arrays and a native recency
//! window, so its request mapping is one-to-one with [`SearchQuery`].

use async_trait::async_trait;
use baybo_security::http::ProxySettings;
use serde::{Deserialize, Serialize};

use crate::error::SearchError;
use crate::providers::{build_client, endpoint, http_error, read_body};
use crate::{DomainFilter, Freshness, SearchOutcome, SearchProvider, SearchQuery, SearchResult};

pub const PROVIDER_NAME: &str = "tavily";

const DEFAULT_BASE_URL: &str = "https://api.tavily.com";
const SEARCH_PATH: &str = "search";

/// Tavily's documented ceilings on its own domain arrays. Over-long lists are
/// trimmed rather than sent, because the API rejects the whole request.
const MAX_INCLUDE_DOMAINS: usize = 300;
const MAX_EXCLUDE_DOMAINS: usize = 150;

pub struct TavilyProvider {
    client: reqwest::Client,
    endpoint: url::Url,
    api_key: String,
}

impl TavilyProvider {
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

#[derive(Debug, Serialize)]
struct Request<'a> {
    query: &'a str,
    max_results: usize,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    include_domains: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    exclude_domains: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    time_range: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    results: Vec<Hit>,
}

#[derive(Debug, Deserialize)]
struct Hit {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    /// Present only on the news topic; absent on the general searches this
    /// provider issues, so the normalized `age` is usually `None`.
    #[serde(default)]
    published_date: Option<String>,
}

fn time_range(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Day => "day",
        Freshness::Week => "week",
        Freshness::Month => "month",
        Freshness::Year => "year",
    }
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchOutcome, SearchError> {
        let (include, exclude) = match &query.domains {
            DomainFilter::Unrestricted => (&[][..], &[][..]),
            DomainFilter::Only(d) => (&d[..d.len().min(MAX_INCLUDE_DOMAINS)], &[][..]),
            DomainFilter::Except(d) => (&[][..], &d[..d.len().min(MAX_EXCLUDE_DOMAINS)]),
        };
        let body = Request {
            query: &query.query,
            max_results: query.max_results,
            include_domains: include,
            exclude_domains: exclude,
            time_range: query.freshness.map(time_range),
            country: query.country.as_deref(),
        };

        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.api_key)
            .json(&body)
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
                .results
                .into_iter()
                .map(|h| SearchResult {
                    title: h.title,
                    url: h.url,
                    snippet: h.content,
                    age: h.published_date,
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
    use crate::providers::test_support::{TestServer, query_for};

    #[tokio::test]
    async fn maps_a_response_to_normalized_results() {
        let server = TestServer::json(
            200,
            r#"{"results":[
                {"title":"Rust 1.98","url":"https://blog.rust-lang.org/x","content":"snippet one","score":0.9},
                {"title":"Other","url":"https://example.com/y","content":"snippet two","published_date":"2026-08-01"}
            ]}"#,
        )
        .await;
        let provider =
            TavilyProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let outcome = provider.search(&query_for("rust")).await.expect("search");

        assert_eq!(outcome.status, 200);
        assert!(outcome.bytes > 0);
        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results[0].title, "Rust 1.98");
        assert_eq!(outcome.results[0].snippet, "snippet one");
        assert_eq!(outcome.results[0].age, None);
        assert_eq!(outcome.results[1].age.as_deref(), Some("2026-08-01"));
    }

    #[tokio::test]
    async fn sends_the_key_as_a_bearer_header_never_in_the_url() {
        let server = TestServer::json(200, r#"{"results":[]}"#).await;
        let provider = TavilyProvider::new("sentinel-key".into(), Some(&server.base_url()), None)
            .expect("build");
        provider.search(&query_for("rust")).await.expect("search");

        let req = server.last_request();
        assert_eq!(
            req.headers.get("authorization").map(String::as_str),
            Some("Bearer sentinel-key")
        );
        assert!(!req.uri.contains("sentinel-key"), "uri = {}", req.uri);
    }

    #[tokio::test]
    async fn domain_filters_use_the_native_arrays() {
        let server = TestServer::json(200, r#"{"results":[]}"#).await;
        let provider =
            TavilyProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let mut q = query_for("rust");
        q.domains = DomainFilter::Only(vec!["rust-lang.org".into()]);
        provider.search(&q).await.expect("search");
        let body = server.last_request().json();
        assert_eq!(body["include_domains"][0], "rust-lang.org");
        assert!(body.get("exclude_domains").is_none());
        // The query string itself must stay clean — no `site:` smuggling.
        assert_eq!(body["query"], "rust");

        let mut q = query_for("rust");
        q.domains = DomainFilter::Except(vec!["spam.io".into()]);
        provider.search(&q).await.expect("search");
        let body = server.last_request().json();
        assert_eq!(body["exclude_domains"][0], "spam.io");
        assert!(body.get("include_domains").is_none());
    }

    #[tokio::test]
    async fn freshness_and_locale_map_onto_native_fields() {
        let server = TestServer::json(200, r#"{"results":[]}"#).await;
        let provider =
            TavilyProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let mut q = query_for("rust");
        q.freshness = Some(Freshness::Week);
        q.country = Some("japan".into());
        provider.search(&q).await.expect("search");

        let body = server.last_request().json();
        assert_eq!(body["time_range"], "week");
        assert_eq!(body["country"], "japan");
        assert_eq!(body["max_results"], 8);
    }

    #[tokio::test]
    async fn a_shape_mismatch_is_a_decode_error() {
        let server = TestServer::json(200, r#"{"nope":1}"#).await;
        let provider =
            TavilyProvider::new("k".into(), Some(&server.base_url()), None).expect("build");
        // `results` defaults to empty, so a missing key is tolerated…
        assert!(provider.search(&query_for("rust")).await.is_ok());

        let server = TestServer::json(200, r#"{"results":[{"title":"no url"}]}"#).await;
        let provider =
            TavilyProvider::new("k".into(), Some(&server.base_url()), None).expect("build");
        // …but a hit with no `url` is a contract break, not an empty result.
        let err = provider
            .search(&query_for("rust"))
            .await
            .expect_err("decode");
        assert!(matches!(err, SearchError::Decode { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_non_2xx_carries_status_and_byte_count() {
        let server = TestServer::json(429, r#"{"detail":"rate limited"}"#).await;
        let provider =
            TavilyProvider::new("k".into(), Some(&server.base_url()), None).expect("build");

        let err = provider.search(&query_for("rust")).await.expect_err("429");
        match err {
            SearchError::Http {
                status,
                bytes,
                body,
            } => {
                assert_eq!(status, 429);
                assert!(bytes > 0, "byte count must survive the failure path");
                assert!(body.contains("rate limited"));
            }
            other => panic!("expected Http, got {other}"),
        }
    }
}
