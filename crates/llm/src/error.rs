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

    /// The account's plan quota is spent until it resets. Arrives as a
    /// 429 like an ordinary throttle and is nothing like one: the wall
    /// stands for hours, so every retry is a call that cannot succeed.
    /// Not retriable — `resets_in` is how long the caller would have to
    /// wait, not a backoff to sleep through.
    #[error("LLM quota exhausted: {message}")]
    QuotaExhausted {
        resets_in: Option<Duration>,
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
    /// variant represents a failure that retrying won't fix — bad
    /// input, bad auth, malformed response, local guard, config, or a
    /// spent quota that resets hours from now.
    pub fn is_retriable(&self) -> bool {
        match self {
            LlmError::Transient(_) | LlmError::RateLimited { .. } => true,
            LlmError::QuotaExhausted { .. }
            | LlmError::BadRequest(_)
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
/// wants to surface (typically
/// `"<provider> <method>: status <code>: <body>"`); this helper just
/// chooses the variant based on the retry / classification semantics of
/// the status.
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

/// Connect-phase timeout for the LLM HTTP client — a provider that can't be
/// reached fails fast (as a retriable `Transient`) instead of hanging the turn.
const LLM_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Per-read (idle) timeout: trips when a *connected* request goes silent for
/// this long. It resets on every byte, so a long-but-streaming completion is
/// unaffected — only a stalled connection trips it, and that maps to
/// `Transient` (retriable). Deliberately not a total `.timeout()`, which would
/// cut off a legitimately long generation.
const LLM_READ_TIMEOUT: Duration = Duration::from_secs(600);

/// Build a `reqwest::Client` honoring the optional egress proxy, mapping a
/// proxy/build failure to `LlmError::Config`. Shared by the rig client
/// builders and by provider live-model discovery so every in-crate HTTP
/// client picks up the configured proxy uniformly, plus connect/idle timeouts
/// so a stalled network surfaces as a retriable error instead of hanging the
/// agent turn indefinitely.
pub(crate) fn proxied_client(
    proxy: Option<&baybo_security::http::ProxySettings>,
) -> Result<reqwest::Client, LlmError> {
    baybo_security::http::client_builder(proxy)
        .and_then(|b| {
            b.connect_timeout(LLM_CONNECT_TIMEOUT)
                .read_timeout(LLM_READ_TIMEOUT)
                .build()
        })
        .map_err(|e| LlmError::Config(format!("build proxied http client: {e}")))
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
/// The residual is `ProviderError(String)`: rig puts a non-2xx answer
/// in here without its status — the raw body on a unary call (Gemini's,
/// among others), or the formatted `http_client::Error` when the stream
/// fails to open. [`provider_error_status`] reads the status back out of
/// either shape and routes it like any other; a message that carries
/// none stays `Transient`, so an unrecognised 5xx still retries.
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
        // Our own providers box their `LlmError` here to carry the
        // classification through rig's error type, which has no typed slot
        // for it. Anything else really is a request-construction failure.
        Rig::RequestError(inner) => match inner.downcast::<LlmError>() {
            Ok(ours) => *ours,
            Err(inner) => LlmError::Internal(anyhow::anyhow!("rig request: {inner}")),
        },
        Rig::ProviderError(msg) => {
            let message = format!("rig provider: {msg}");
            match provider_error_status(&msg) {
                Some(status) => status_to_error(status, message),
                None => LlmError::Transient(message),
            }
        }
    }
}

/// Prefix of rig's `http_client::Error::InvalidStatusCode*` display, which
/// rig's stream adapters wrap into `ProviderError` when the stream fails to
/// open: `Invalid status code: 400 Bad Request` or
/// `Invalid status code 400 Bad Request with message: …`.
const RIG_INVALID_STATUS_PREFIX: &str = "Invalid status code";

/// Prefix rig's Anthropic stream adapter puts in front of that display
/// (`SSE Error: Invalid status code: 400 Bad Request`); Anthropic, MiniMax
/// and OpenCode's Messages models all stream through it.
const RIG_ANTHROPIC_SSE_ERROR_PREFIX: &str = "SSE Error: ";

/// The HTTP status a rig `ProviderError` message carries, if any: either
/// rig's own status display, or an error body whose `error.code` is a
/// status — Google's `{"error":{"code":400,"status":"INVALID_ARGUMENT"}}`,
/// sent bare or as a one-element array.
fn provider_error_status(msg: &str) -> Option<u16> {
    let msg = msg.trim();
    let status_display = msg
        .strip_prefix(RIG_ANTHROPIC_SSE_ERROR_PREFIX)
        .unwrap_or(msg);
    if let Some(rest) = status_display.strip_prefix(RIG_INVALID_STATUS_PREFIX) {
        let digits: String = rest
            .trim_start_matches(':')
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        return digits.parse().ok().filter(is_http_error_status);
    }
    let body: serde_json::Value = serde_json::from_str(msg).ok()?;
    let error = match &body {
        serde_json::Value::Array(items) => items.first()?.get("error")?,
        other => other.get("error")?,
    };
    error
        .get("code")?
        .as_u64()
        .and_then(|code| u16::try_from(code).ok())
        .filter(is_http_error_status)
}

fn is_http_error_status(status: &u16) -> bool {
    (400..600).contains(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use rig::completion::CompletionError;
    use rig::http_client::Error as RigHttp;

    /// A spent plan quota arrives as a 429 and must not be retried like
    /// one, and the classification has to survive rig's error type — the
    /// compaction path boxes it through `RequestError`, and a round trip
    /// that loses it puts the caller back on ten doomed attempts.
    #[test]
    fn a_spent_quota_does_not_retry_and_survives_the_rig_boundary() {
        let spent = LlmError::QuotaExhausted {
            resets_in: Some(Duration::from_secs(98_178)),
            message: "openai-subscription: 429".into(),
        };
        assert!(!spent.is_retriable());

        let round_tripped = rig_completion_to_error(CompletionError::RequestError(Box::new(spent)));
        assert!(
            matches!(
                round_tripped,
                LlmError::QuotaExhausted { resets_in: Some(d), .. } if d == Duration::from_secs(98_178)
            ),
            "got {round_tripped:?}"
        );
        assert!(!round_tripped.is_retriable());

        // A rate limit still retries — the two must not collapse.
        assert!(
            rig_completion_to_error(CompletionError::RequestError(Box::new(
                LlmError::RateLimited {
                    retry_after: None,
                    message: "slow down".into(),
                }
            )))
            .is_retriable()
        );

        // Something that is not ours stays a request-construction failure.
        let foreign = rig_completion_to_error(CompletionError::RequestError(Box::new(
            std::io::Error::other("boom"),
        )));
        assert!(matches!(foreign, LlmError::Internal(_)));
    }

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
    /// status; a message with no recoverable status keeps the old
    /// "default to retry" behaviour so 5xx bodies still retry. This test
    /// pins that contract so a future "let's just BadRequest it" change
    /// is forced to think about the 5xx regression first.
    #[test]
    fn rig_provider_error_defaults_to_transient() {
        for body in [
            r#"{"error":{"message":"upstream overloaded"}}"#,
            r#"{"error":{"code":"server_error","message":"upstream overloaded"}}"#,
            "upstream overloaded",
            "Invalid status code: garbled",
        ] {
            let err = rig_completion_to_error(CompletionError::ProviderError(body.into()));
            assert!(matches!(err, LlmError::Transient(_)), "{body}: {err:?}");
            assert!(err.is_retriable());
        }
    }

    /// A Gemini 400 reaches us as a `ProviderError` holding only the body
    /// (unary) or rig's status display (stream open). Either way it must
    /// surface at once instead of burning the retry budget on a request
    /// that will be rejected the same way every time; 429 and 5xx in the
    /// same shapes keep retrying.
    #[test]
    fn rig_provider_error_routes_a_recoverable_status() {
        let classify =
            |msg: &str| rig_completion_to_error(CompletionError::ProviderError(msg.into()));
        for msg in [
            r#"{"error":{"code":400,"message":"Please ensure that the number of function response parts is equal to the number of function call parts of the function call turn.","status":"INVALID_ARGUMENT"}}"#,
            r#"[{"error":{"code":400,"status":"INVALID_ARGUMENT"}}]"#,
            "Invalid status code: 400 Bad Request",
            r#"Invalid status code 400 Bad Request with message: {"error":{"code":400}}"#,
            "SSE Error: Invalid status code: 400 Bad Request",
        ] {
            let err = classify(msg);
            assert!(matches!(err, LlmError::BadRequest(_)), "{msg}: {err:?}");
        }
        let denied = classify(r#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#);
        assert!(matches!(denied, LlmError::Auth(_)), "{denied:?}");
        let rejected_key = classify("SSE Error: Invalid status code: 401 Unauthorized");
        assert!(
            matches!(rejected_key, LlmError::Auth(_)),
            "{rejected_key:?}"
        );
        let throttled = classify(r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED"}}"#);
        assert!(
            matches!(throttled, LlmError::RateLimited { .. }),
            "{throttled:?}"
        );
        for msg in [
            "Invalid status code: 503 Service Unavailable",
            "SSE Error: Invalid status code: 529 <unknown status code>",
        ] {
            let err = classify(msg);
            assert!(matches!(err, LlmError::Transient(_)), "{msg}: {err:?}");
        }
    }
}
