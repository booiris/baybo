//! SearXNG — `GET <base_url>/search?format=json`, no credential.
//!
//! Two operational facts shape this module, and neither is a detail:
//!
//! 1. **JSON is off by default.** `search: formats: [html, json]` must be in
//!    the instance's `settings.yml`; an unset format answers `403`. That is
//!    the single most likely first-run failure, so the 403 path says so
//!    instead of reporting a permission problem.
//! 2. **Engines get suspended.** SearXNG parks an upstream engine for an hour
//!    after a 429 and for a day after a CAPTCHA, then keeps answering `200`
//!    with whatever is left. Degradation is silent by default, so
//!    `unresponsive_engines` is surfaced rather than swallowed — an empty
//!    result set from a half-suspended instance otherwise reads to the model
//!    as "this does not exist".

use async_trait::async_trait;
use baybo_security::http::ProxySettings;
use serde::Deserialize;

use crate::error::SearchError;
use crate::providers::{append_site_operators, build_client, endpoint, http_error, read_body};
use crate::{Freshness, SearchOutcome, SearchProvider, SearchQuery, SearchResult};

pub const PROVIDER_NAME: &str = "searxng";

const SEARCH_PATH: &str = "search";
const JSON_FORMAT: &str = "json";

/// What a `403` from a SearXNG instance almost always means. Worth naming:
/// the fix is one line of `settings.yml`, and nothing else about the error
/// points at it.
const JSON_FORMAT_HINT: &str = "SearXNG refused the request (403). The JSON output format is \
                                disabled by default — add `search: formats: [html, json]` to the \
                                instance's settings.yml and restart it.";

pub struct SearxngProvider {
    client: reqwest::Client,
    endpoint: url::Url,
}

impl SearxngProvider {
    /// `base_url` is required: SearXNG has no hosted endpoint, so the
    /// operator's own instance is the only address there is. `baybo-config`
    /// rejects the section without one; this is the second gate.
    pub fn new(base_url: &str, proxy: Option<&ProxySettings>) -> Result<Self, SearchError> {
        Ok(Self {
            client: build_client(proxy)?,
            endpoint: endpoint(Some(base_url), base_url, SEARCH_PATH)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    results: Vec<Hit>,
    /// `[["google", "CAPTCHA"], …]` — engines that produced nothing this
    /// call. Shape is a list of pairs, so it is read loosely.
    #[serde(default)]
    unresponsive_engines: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct Hit {
    #[serde(default)]
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(rename = "publishedDate", default)]
    published_date: Option<String>,
}

fn time_range(freshness: Freshness) -> &'static str {
    match freshness {
        // SearXNG's vocabulary is day/month/year only; a week maps up to the
        // narrowest window that cannot hide a result the model asked for.
        Freshness::Day => "day",
        Freshness::Week | Freshness::Month => "month",
        Freshness::Year => "year",
    }
}

#[async_trait]
impl SearchProvider for SearxngProvider {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    async fn search(&self, query: &SearchQuery) -> Result<SearchOutcome, SearchError> {
        let mut params: Vec<(&str, String)> = vec![
            ("q", append_site_operators(&query.query, &query.domains)),
            ("format", JSON_FORMAT.to_string()),
        ];
        if let Some(f) = query.freshness {
            params.push(("time_range", time_range(f).to_string()));
        }
        if let Some(lang) = &query.language {
            params.push(("language", lang.clone()));
        }

        let response = self
            .client
            .get(self.endpoint.clone())
            .query(&params)
            .send()
            .await
            .map_err(|e| SearchError::from_transport(&e))?;

        let status = response.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            let mut err = http_error(response).await;
            if let SearchError::Http { body, .. } = &mut err {
                *body = JSON_FORMAT_HINT.to_string();
            }
            return Err(err);
        }
        if !status.is_success() {
            return Err(http_error(response).await);
        }
        let raw = read_body(response).await?;
        let bytes = raw.len() as u64;
        let parsed: Response = serde_json::from_str(&raw).map_err(|e| SearchError::Decode {
            reason: e.to_string(),
        })?;

        if !parsed.unresponsive_engines.is_empty() {
            tracing::warn!(
                engines = ?parsed.unresponsive_engines,
                "SearXNG answered with suspended engines; results are partial"
            );
        }

        Ok(SearchOutcome {
            results: parsed
                .results
                .into_iter()
                .take(query.max_results)
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
    use crate::DomainFilter;
    use crate::providers::test_support::{TestServer, query_for};

    #[tokio::test]
    async fn maps_a_response_to_normalized_results() {
        let server = TestServer::json(
            200,
            r#"{"results":[
                {"title":"Rust 1.98","url":"https://blog.rust-lang.org/x","content":"snippet","publishedDate":"2026-08-07T00:00:00"},
                {"title":"Other","url":"https://example.com/y","content":"other"}
            ]}"#,
        )
        .await;
        let provider = SearxngProvider::new(&server.base_url(), None).expect("build");

        let outcome = provider.search(&query_for("rust")).await.expect("search");

        assert_eq!(outcome.results.len(), 2);
        assert_eq!(
            outcome.results[0].age.as_deref(),
            Some("2026-08-07T00:00:00")
        );
        assert_eq!(outcome.results[1].age, None);
    }

    /// SearXNG paginates rather than taking a count, so the cap is ours.
    #[tokio::test]
    async fn the_result_count_is_capped_client_side() {
        let hits: Vec<String> = (0..30)
            .map(|i| format!(r#"{{"title":"t{i}","url":"https://e{i}.com/","content":"c"}}"#))
            .collect();
        let server = TestServer::json(200, &format!(r#"{{"results":[{}]}}"#, hits.join(","))).await;
        let provider = SearxngProvider::new(&server.base_url(), None).expect("build");

        let outcome = provider.search(&query_for("rust")).await.expect("search");
        assert_eq!(outcome.results.len(), 8);
    }

    #[tokio::test]
    async fn always_asks_for_json_and_carries_the_filter_as_operators() {
        let server = TestServer::json(200, r#"{"results":[]}"#).await;
        let provider = SearxngProvider::new(&server.base_url(), None).expect("build");

        let mut q = query_for("rust");
        q.domains = DomainFilter::Only(vec!["rust-lang.org".into()]);
        q.freshness = Some(Freshness::Week);
        q.language = Some("zh".into());
        provider.search(&q).await.expect("search");

        let params = server.last_request().params();
        assert_eq!(params.get("format").map(String::as_str), Some("json"));
        assert_eq!(
            params.get("q").map(String::as_str),
            Some("rust site:rust-lang.org")
        );
        // A week has no SearXNG equivalent; it widens to the next window
        // rather than silently dropping the filter.
        assert_eq!(params.get("time_range").map(String::as_str), Some("month"));
        assert_eq!(params.get("language").map(String::as_str), Some("zh"));
    }

    /// The first-run failure, and the one message that actually helps.
    #[tokio::test]
    async fn a_403_explains_that_json_output_is_disabled() {
        let server = TestServer::json(403, "Forbidden").await;
        let provider = SearxngProvider::new(&server.base_url(), None).expect("build");

        match provider.search(&query_for("rust")).await.expect_err("403") {
            SearchError::Http { status, body, .. } => {
                assert_eq!(status, 403);
                assert!(body.contains("settings.yml"), "body = {body}");
            }
            other => panic!("expected Http, got {other}"),
        }
    }

    #[tokio::test]
    async fn suspended_engines_do_not_fail_the_search() {
        let server = TestServer::json(
            200,
            r#"{"results":[{"title":"t","url":"https://e.com/","content":"c"}],
                "unresponsive_engines":[["google","CAPTCHA"]]}"#,
        )
        .await;
        let provider = SearxngProvider::new(&server.base_url(), None).expect("build");
        let outcome = provider.search(&query_for("rust")).await.expect("search");
        assert_eq!(outcome.results.len(), 1);
    }

    /// The only bound on a body used to be `ctx.timeout`, which is long
    /// enough for a broken endpoint to stream itself into memory.
    #[tokio::test]
    async fn an_oversized_response_is_refused_rather_than_buffered() {
        let filler = "x".repeat(2 * 1024 * 1024);
        let server = TestServer::json(
            200,
            &format!(
                r#"{{"results":[{{"title":"{filler}","url":"https://e.com/","content":"c"}}]}}"#
            ),
        )
        .await;
        let provider = SearxngProvider::new(&server.base_url(), None).expect("build");
        let err = provider
            .search(&query_for("rust"))
            .await
            .expect_err("over cap");
        match err {
            SearchError::Decode { reason } => assert!(reason.contains("cap"), "{reason}"),
            other => panic!("expected Decode, got {other}"),
        }
    }

    #[tokio::test]
    async fn a_base_url_with_a_path_prefix_is_preserved() {
        let provider = SearxngProvider::new("https://gw.corp/searxng", None).expect("build");
        assert_eq!(provider.endpoint.as_str(), "https://gw.corp/searxng/search");
    }
}
