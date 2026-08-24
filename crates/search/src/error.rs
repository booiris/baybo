use thiserror::Error;

/// Failures a [`crate::SearchProvider`] can report.
///
/// Deliberately carries no URL anywhere: `reqwest::Error`'s `Display` embeds
/// the request target, and a tool error string reaches the model. Every
/// variant is built from a classified reason rather than from the transport
/// error itself.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Non-2xx from the provider. `body` is already truncated, and carries no
    /// credential — every supported provider authenticates with a header.
    /// `bytes` is the response length so the caller can still emit a complete
    /// `HttpFetch` trace event on the failure path.
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        bytes: u64,
        body: String,
    },

    /// The request never produced a response (DNS, connect, TLS, body read).
    #[error("{reason}")]
    Transport { reason: &'static str },

    /// A response arrived but did not match the provider's documented shape.
    #[error("provider returned an unreadable response: {reason}")]
    Decode { reason: String },

    /// The provider could not be constructed from the operator's config.
    /// Produced only by a provider constructor, never by a search.
    #[error("{reason}")]
    Config { reason: String },
}

impl SearchError {
    /// Classify a transport failure into a stable reason with no URL in it.
    pub(crate) fn from_transport(e: &reqwest::Error) -> Self {
        let reason = if e.is_timeout() {
            "the search provider did not respond in time"
        } else if e.is_connect() {
            "could not connect to the search provider"
        } else if e.is_decode() {
            "the search provider's response could not be read"
        } else if e.is_body() {
            "the search provider closed the connection mid-response"
        } else {
            "the request to the search provider failed"
        };
        Self::Transport { reason }
    }
}
