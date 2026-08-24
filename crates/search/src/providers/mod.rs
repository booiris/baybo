//! One module per provider. Everything genuinely identical across them —
//! the deadline, the cancellation race, phase timers, trace events, the
//! operator blocklist, domain enforcement, snippet trimming, rendering and
//! the `ToolError` mapping — lives in [`crate::tool`] and is written once.
//! What is left here is what actually differs: request encoding (POST-JSON
//! with native domain arrays versus GET with `site:` operators), the response
//! struct, and the field mapping.

pub mod brave;
pub mod searxng;
pub mod tavily;

use std::time::Duration;

use baybo_security::http::ProxySettings;
use baybo_security::http::client_builder;
use reqwest::redirect;

use crate::error::SearchError;

/// Time allowed for connect + TLS, separate from the tool's overall deadline
/// so a stuck DNS phase cannot eat the whole budget.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard ceiling on a response body. A search answer is a few KB; anything
/// approaching this is a broken or hostile endpoint, not a large result set.
pub(crate) const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Longest error body echoed back to the model. Enough to carry a provider's
/// own message, short enough that a 500 with an HTML error page does not
/// become the turn's largest tool result.
pub(crate) const ERROR_BODY_PREVIEW_BYTES: usize = 2 * 1024;

/// User agent every provider identifies with.
pub(crate) const USER_AGENT: &str = concat!("baybo-websearch/", env!("CARGO_PKG_VERSION"));

/// A `reqwest::Client` for talking to one known search endpoint.
///
/// **No `SafeResolver` is installed, deliberately.** That filter exists to
/// stop a *model- or attacker-chosen* hostname from resolving to internal
/// infrastructure; here the host is a compile-time constant or the operator's
/// own `base_url`, and the model contributes only a query string. Installing
/// it would also break the deployment that most deserves to work — a
/// self-hosted SearXNG on an RFC1918 address. See `docs/modules/security.md`.
///
/// Redirects are refused outright: a known API endpoint that 302s you is
/// broken or hijacked, and there is no legitimate hop to preserve.
pub(crate) fn build_client(proxy: Option<&ProxySettings>) -> Result<reqwest::Client, SearchError> {
    client_builder(proxy)
        .and_then(|b| {
            b.user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(redirect::Policy::none())
                .build()
        })
        .map_err(|e| SearchError::Config {
            reason: format!("could not build the search HTTP client: {e}"),
        })
}

/// Validate an operator-supplied base URL at construction time.
///
/// This is the one URL check the search path keeps, and `is_http_url` in
/// `baybo-config` is two `starts_with` calls — so the parse happens here:
/// `https://user:pass@evil.tld` passes a prefix check and must not pass this.
pub(crate) fn validate_base_url(raw: &str) -> Result<url::Url, SearchError> {
    let parsed = url::Url::parse(raw).map_err(|e| SearchError::Config {
        reason: format!("web_search.base_url is not a URL: {e}"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SearchError::Config {
            reason: "web_search.base_url must use http or https".to_string(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(SearchError::Config {
            reason: "web_search.base_url must not embed credentials — store the key with \
                     `baybo secret add`"
                .to_string(),
        });
    }
    // `endpoint` appends the provider's path to the raw string, which extends
    // whatever the last component happens to be. A query or fragment would
    // swallow the path segment and silently request the wrong endpoint —
    // refuse rather than build a URL nobody would recognise.
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(SearchError::Config {
            reason: "web_search.base_url must be a bare origin (optionally with a path prefix) — \
                     no query string and no fragment"
                .to_string(),
        });
    }
    Ok(parsed)
}

/// Join an operator's `base_url` (or the provider's default) with the
/// provider's endpoint path.
///
/// String concatenation rather than [`url::Url::join`] on purpose: `join`
/// treats a base without a trailing slash as a file and replaces its last
/// segment, which would silently discard the path prefix of a corporate
/// gateway like `https://gw.corp/tavily`.
pub(crate) fn endpoint(
    base_url: Option<&str>,
    default_base: &str,
    path: &str,
) -> Result<url::Url, SearchError> {
    let base = match base_url {
        Some(raw) => {
            validate_base_url(raw)?;
            raw
        }
        None => default_base,
    };
    let joined = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    url::Url::parse(&joined).map_err(|e| SearchError::Config {
        reason: format!("could not build the search endpoint URL: {e}"),
    })
}

/// Read a response body, refusing anything past [`MAX_RESPONSE_BYTES`].
///
/// `response.text()` buffers without limit, and the only other bound in play
/// is `ctx.timeout` (30 s) — long enough for a broken or hijacked endpoint to
/// stream gigabytes into memory, and `WebSearch` is `Concurrent`, so ten can
/// be in flight. `WebFetch` caps the same way (`MAX_RESPONSE_BYTES`, 1 MiB);
/// this is the search-shaped version of that guard. Truncating instead would
/// only turn the overrun into a confusing parse error.
pub(crate) async fn read_body(mut response: reqwest::Response) -> Result<String, SearchError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES)
    {
        return Err(oversized());
    }
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() as u64 + chunk.len() as u64 > MAX_RESPONSE_BYTES {
                    return Err(oversized());
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(SearchError::from_transport(&e)),
        }
    }
    String::from_utf8(buf).map_err(|e| SearchError::Decode {
        reason: format!("response is not valid UTF-8: {e}"),
    })
}

fn oversized() -> SearchError {
    SearchError::Decode {
        reason: format!("response exceeded the {MAX_RESPONSE_BYTES} byte cap"),
    }
}

/// Read a non-2xx response into a [`SearchError::Http`], keeping the byte
/// count so the caller can still emit a complete trace event.
pub(crate) async fn http_error(response: reqwest::Response) -> SearchError {
    let status = response.status().as_u16();
    let body = read_body(response).await.unwrap_or_default();
    let bytes = body.len() as u64;
    SearchError::Http {
        status,
        bytes,
        body: truncate_on_char_boundary(&body, ERROR_BODY_PREVIEW_BYTES),
    }
}

/// Truncate to at most `max_bytes`, never splitting a UTF-8 sequence.
pub(crate) fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Encode a domain filter as search operators, for providers that have no
/// native domain parameter. A *ranking hint only* — [`crate::DomainFilter`]
/// is enforced on the results by the tool, because Brave silently drops
/// operators when the filtered set is small.
///
/// Entries are already bare hosts: `DomainFilter::only` / `::except`
/// normalize at construction, so nothing here has to guess at the shape.
pub(crate) fn append_site_operators(query: &str, filter: &crate::DomainFilter) -> String {
    let (prefix, domains) = match filter {
        crate::DomainFilter::Unrestricted => return query.to_string(),
        crate::DomainFilter::Only(d) => ("site:", d),
        crate::DomainFilter::Except(d) => ("-site:", d),
    };
    let mut out = String::from(query);
    for domain in domains {
        out.push(' ');
        out.push_str(prefix);
        out.push_str(domain);
    }
    out
}

/// A real HTTP server on `127.0.0.1:0` that records what it was sent and
/// replies with a canned body.
///
/// The workspace has no mocking crate; the house pattern is an axum server on
/// an ephemeral port (`crates/tools/src/builtin/web_fetch.rs`). Every provider
/// takes its base URL from config, so pointing one at this server needs no
/// test-only constructor.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse;
    use parking_lot::Mutex;

    use crate::{DomainFilter, SearchQuery};

    #[derive(Debug, Clone, Default)]
    pub(crate) struct RecordedRequest {
        pub uri: String,
        pub headers: HashMap<String, String>,
        pub body: String,
    }

    impl RecordedRequest {
        pub fn json(&self) -> serde_json::Value {
            serde_json::from_str(&self.body).expect("request body is JSON")
        }

        /// Decoded `key=value` pairs from the query string.
        pub fn params(&self) -> HashMap<String, String> {
            let Some(qs) = self.uri.split_once('?').map(|(_, q)| q) else {
                return HashMap::new();
            };
            url::form_urlencoded::parse(qs.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        }
    }

    struct Shared {
        status: StatusCode,
        body: String,
        requests: Mutex<Vec<RecordedRequest>>,
    }

    pub(crate) struct TestServer {
        addr: std::net::SocketAddr,
        shared: Arc<Shared>,
    }

    impl TestServer {
        pub async fn json(status: u16, body: &str) -> Self {
            let shared = Arc::new(Shared {
                status: StatusCode::from_u16(status).expect("valid status"),
                body: body.to_string(),
                requests: Mutex::new(Vec::new()),
            });
            let app = axum::Router::new()
                .fallback(handler)
                .with_state(Arc::clone(&shared));
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind ephemeral port");
            let addr = listener.local_addr().expect("local addr");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self { addr, shared }
        }

        pub fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        pub fn last_request(&self) -> RecordedRequest {
            self.shared
                .requests
                .lock()
                .last()
                .cloned()
                .expect("the server received a request")
        }
    }

    async fn handler(
        State(shared): State<Arc<Shared>>,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        shared.requests.lock().push(RecordedRequest {
            uri: uri.to_string(),
            headers: headers
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
                .collect(),
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        (
            shared.status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            shared.body.clone(),
        )
    }

    /// A default query, so a test names only the field it is exercising.
    pub(crate) fn query_for(query: &str) -> SearchQuery {
        SearchQuery {
            query: query.to_string(),
            max_results: 8,
            domains: DomainFilter::Unrestricted,
            freshness: None,
            country: None,
            language: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DomainFilter;

    #[test]
    fn base_url_rejects_embedded_credentials() {
        let err = validate_base_url("https://user:pass@evil.tld").expect_err("must refuse");
        assert!(matches!(err, SearchError::Config { .. }), "{err}");
    }

    #[test]
    fn base_url_rejects_a_non_http_scheme() {
        assert!(validate_base_url("file:///etc/passwd").is_err());
        assert!(validate_base_url("not a url").is_err());
    }

    /// `endpoint` appends onto the raw string, so either of these would
    /// swallow the path segment and request a URL nobody would recognise:
    /// `https://gw.corp/tavily?t=x` + `search` → `…?t=x/search`.
    #[test]
    fn base_url_rejects_a_query_string_or_fragment() {
        assert!(validate_base_url("https://gw.corp/tavily?token=x").is_err());
        assert!(validate_base_url("https://gw.corp/tavily#frag").is_err());
        assert!(validate_base_url("https://gw.corp/tavily").is_ok());
    }

    #[test]
    fn base_url_accepts_a_plain_private_address() {
        // The self-hosted SearXNG case: an RFC1918 host is legitimate here.
        assert!(validate_base_url("http://10.0.0.5:8888").is_ok());
    }

    #[test]
    fn truncation_lands_on_a_char_boundary() {
        let s = "日本語のテキスト";
        let out = truncate_on_char_boundary(s, 7);
        assert!(out.is_char_boundary(out.len()));
        assert!(out.ends_with('…'));
        assert!(s.starts_with(out.trim_end_matches('…')));
    }

    #[test]
    fn site_operators_are_appended_per_domain() {
        // Built through the constructor, which is where `*.` and any other
        // spelling is normalized away — this function receives bare hosts.
        let (f, _) = DomainFilter::only(&["a.com".into(), "*.b.org".into()]);
        assert_eq!(
            append_site_operators("rust", &f),
            "rust site:a.com site:b.org"
        );

        let f = DomainFilter::Except(vec!["spam.io".into()]);
        assert_eq!(append_site_operators("rust", &f), "rust -site:spam.io");

        assert_eq!(
            append_site_operators("rust", &DomainFilter::Unrestricted),
            "rust"
        );
    }
}
