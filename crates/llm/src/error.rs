use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    /// Transient provider failure: 5xx, 408 timeout, or transport
    /// errors (connect / timeout / network reset). Retriable —
    /// retrying with the same payload may succeed.
    #[error("LLM transient error: {0}")]
    Transient(String),

    /// Provider asked us to slow down (HTTP 429 or equivalent). Same
    /// retry semantics as `Transient` but carries an optional hint
    /// from the provider about how long to wait before the next
    /// attempt — caller may honour it instead of using its own
    /// backoff schedule.
    #[error("LLM rate limited: {message}")]
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },

    /// Provider rejected the request as malformed or unsupported (4xx
    /// other than 408 / 429). Not retriable: the same input will
    /// produce the same rejection.
    #[error("LLM bad request: {0}")]
    BadRequest(String),

    /// Provider auth check failed (401 / 403, OAuth refresh failure,
    /// missing credentials surfaced from the provider side). Not
    /// retriable — needs operator action (rotate key, re-login).
    #[error("LLM auth error: {0}")]
    Auth(String),

    /// Couldn't parse the provider's response (JSON body, SSE frame,
    /// missing required field). Not retriable — the response shape is
    /// stable across attempts; either a provider-side schema change
    /// or our parser is wrong.
    #[error("LLM decode error: {0}")]
    Decode(String),

    /// Local configuration error (unset env var, malformed URL, etc.)
    /// surfaced before or independently of a network call.
    #[error("LLM configuration error: {0}")]
    Config(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// A caller-installed pre-call guard rejected the request before
    /// it was dispatched. Distinct from `Auth` (which is provider-side
    /// rejection) so retry classification can skip burning attempts
    /// on a deterministic local gate (cost budget, rate limit, kill
    /// switch).
    #[error("LLM call rejected by local guard: {0}")]
    GuardRejected(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl LlmError {
    /// Whether retrying with the same input might succeed.
    ///
    /// Only `Transient` and `RateLimited` are retriable. Every other
    /// variant represents a deterministic failure (bad input, bad
    /// auth, malformed response, local guard, config) that retrying
    /// won't fix.
    pub fn is_retriable(&self) -> bool {
        match self {
            LlmError::Transient(_) | LlmError::RateLimited { .. } => true,
            LlmError::BadRequest(_)
            | LlmError::Auth(_)
            | LlmError::Decode(_)
            | LlmError::Config(_)
            | LlmError::ModelNotFound(_)
            | LlmError::GuardRejected(_)
            | LlmError::Internal(_) => false,
        }
    }
}

/// Map an HTTP status code from a provider to the appropriate
/// `LlmError` variant. The `message` is the formatted text the caller
/// wants to surface (typically `"<provider> <method>: status <code>:
/// <body>"`); this helper just chooses the variant based on the
/// retry / classification semantics of the status.
pub(crate) fn status_to_error(status: u16, message: String) -> LlmError {
    match status {
        408 => LlmError::Transient(message),
        429 => LlmError::RateLimited {
            retry_after: None,
            message,
        },
        401 | 403 => LlmError::Auth(message),
        s if (500..600).contains(&s) => LlmError::Transient(message),
        _ => LlmError::BadRequest(message),
    }
}

/// Map a `reqwest::Error` from a provider call to the appropriate
/// `LlmError` variant. Timeouts and connect failures are transient;
/// decode errors map to `Decode`; everything else falls back to
/// `Transient` (conservative — most reqwest send errors are transport
/// flake worth retrying once).
pub(crate) fn reqwest_to_error(e: reqwest::Error, context: impl Into<String>) -> LlmError {
    let msg = format!("{}: {e}", context.into());
    if e.is_timeout() || e.is_connect() {
        LlmError::Transient(msg)
    } else if e.is_decode() {
        LlmError::Decode(msg)
    } else {
        LlmError::Transient(msg)
    }
}

/// Map a `rig::completion::CompletionError` to the appropriate
/// `LlmError`. This is the classifier the main chat dispatch path
/// (`LlmClient::chat` / `chat_stream`) relies on, since rig is the
/// HTTP+JSON layer for openai/anthropic/gemini/minimax.
///
/// What we can tell apart from rig's enum:
///   * `HttpError(InvalidStatusCode*)` — rig surfaced a non-2xx with
///     the status; route through `status_to_error` so 401/403 become
///     `Auth`, 4xx → `BadRequest`, 5xx/408 → `Transient`, 429 →
///     `RateLimited`.
///   * `JsonError` / `ResponseError` — body parsed wrong / missing
///     fields → `Decode` (retrying the same body will fail the same
///     way).
///   * Other transport errors (`Protocol`, `StreamEnded`, `Instance`)
///     → `Transient`.
///   * `UrlError`, `InvalidHeaderValue`, `NoHeaders` → `Config` /
///     `Internal` — these mean we built the request wrong, retrying
///     can't fix it.
///
/// The residual is `ProviderError(String)`: rig stuffs the raw
/// response body in here without preserving the HTTP status, so we
/// can't distinguish 4xx from 5xx without parsing the body. Default
/// to `Transient` so 5xx still retries, accepting that 4xx will burn
/// the retry quota until rig surfaces status separately.
pub(crate) fn rig_completion_to_error(e: rig::completion::CompletionError) -> LlmError {
    use rig::completion::CompletionError as Rig;
    use rig::http_client::Error as RigHttp;
    match e {
        Rig::HttpError(inner) => match inner {
            RigHttp::InvalidStatusCode(status) => {
                status_to_error(status.as_u16(), format!("rig http: status {status}"))
            }
            RigHttp::InvalidStatusCodeWithMessage(status, body) => status_to_error(
                status.as_u16(),
                format!("rig http: status {status}: {body}"),
            ),
            RigHttp::InvalidContentType(h) => {
                LlmError::Decode(format!("rig http: invalid content-type: {h:?}"))
            }
            RigHttp::InvalidHeaderValue(e) => {
                LlmError::Internal(anyhow::anyhow!("rig http: invalid header value: {e}"))
            }
            RigHttp::NoHeaders => {
                LlmError::Internal(anyhow::anyhow!("rig http: response missing headers"))
            }
            RigHttp::StreamEnded => LlmError::Transient("rig http: stream ended early".to_string()),
            RigHttp::Protocol(e) => LlmError::Transient(format!("rig http: protocol: {e}")),
            RigHttp::Instance(e) => LlmError::Transient(format!("rig http: client: {e}")),
        },
        Rig::JsonError(inner) => LlmError::Decode(format!("rig json: {inner}")),
        Rig::ResponseError(msg) => LlmError::Decode(format!("rig response: {msg}")),
        Rig::UrlError(inner) => LlmError::Config(format!("rig url: {inner}")),
        Rig::RequestError(inner) => LlmError::Internal(anyhow::anyhow!("rig request: {inner}")),
        Rig::ProviderError(msg) => LlmError::Transient(format!("rig provider: {msg}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use rig::completion::CompletionError;
    use rig::http_client::Error as RigHttp;

    /// Status-driven routing: 401/403 must NOT retry, 429/5xx must
    /// retry, 4xx other than 408/429 must NOT retry. Without this
    /// mapping every rig error retries 3 times before failing.
    #[test]
    fn rig_invalid_status_routes_by_code() {
        let unauthorized = rig_completion_to_error(CompletionError::HttpError(
            RigHttp::InvalidStatusCode(StatusCode::UNAUTHORIZED),
        ));
        assert!(matches!(unauthorized, LlmError::Auth(_)));
        assert!(!unauthorized.is_retriable());

        let bad_request = rig_completion_to_error(CompletionError::HttpError(
            RigHttp::InvalidStatusCode(StatusCode::BAD_REQUEST),
        ));
        assert!(matches!(bad_request, LlmError::BadRequest(_)));
        assert!(!bad_request.is_retriable());

        let too_many = rig_completion_to_error(CompletionError::HttpError(
            RigHttp::InvalidStatusCode(StatusCode::TOO_MANY_REQUESTS),
        ));
        assert!(matches!(too_many, LlmError::RateLimited { .. }));
        assert!(too_many.is_retriable());

        let server = rig_completion_to_error(CompletionError::HttpError(
            RigHttp::InvalidStatusCode(StatusCode::SERVICE_UNAVAILABLE),
        ));
        assert!(matches!(server, LlmError::Transient(_)));
        assert!(server.is_retriable());
    }

    /// Schema mismatches and JSON parse failures must not retry — the
    /// same body will fail the same way next time.
    #[test]
    fn rig_response_and_json_errors_do_not_retry() {
        let resp = rig_completion_to_error(CompletionError::ResponseError("bad shape".into()));
        assert!(matches!(resp, LlmError::Decode(_)));
        assert!(!resp.is_retriable());
    }

    /// `ProviderError` is the residual rig variant that drops the
    /// status; we keep the old "default to retry" behaviour so 5xx
    /// bodies still retry. This test pins that contract so a future
    /// "let's just BadRequest it" change is forced to think about
    /// the 5xx regression first.
    #[test]
    fn rig_provider_error_defaults_to_transient() {
        let err = rig_completion_to_error(CompletionError::ProviderError(
            r#"{"error":{"message":"upstream overloaded"}}"#.into(),
        ));
        assert!(matches!(err, LlmError::Transient(_)));
        assert!(err.is_retriable());
    }
}
