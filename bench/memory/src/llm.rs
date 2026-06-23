//! Minimal OpenAI-compatible chat client (→ DeepSeek by default), used by the
//! judge. A single non-streaming `POST /chat/completions` at temperature 0,
//! returning the assistant text. (The answer side runs inside the real Baybo
//! agent via `baybo prompt`, not through this client.)

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::Deserialize;
use serde_json::json;

pub const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
/// DeepSeek's OpenAI-compatible endpoint root (`/chat/completions` is appended).
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
/// Generous 1h ceiling — a slow reasoning judge model, matching the bench's
/// other waits.
const HTTP_TIMEOUT: Duration = Duration::from_secs(3600);
/// How much of an error body to surface in messages.
const ERR_PREVIEW: usize = 500;
/// Judge calls hit a remote LLM API; transient failures (network resets, HTTP/2
/// stream errors, 5xx, 429) are retried with exponential backoff so one blip
/// doesn't abort a 1500-question run. A 4xx other than 429 is not retried.
const MAX_RETRIES: u32 = 5;
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// A model bound to a base URL + key. Share across concurrent tasks behind a `&`.
pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl ChatClient {
    /// Build a client for `model`. Reads the key from [`DEEPSEEK_API_KEY_ENV`];
    /// `base_url` defaults to [`DEFAULT_BASE_URL`].
    pub fn new(model: &str, base_url: Option<&str>) -> Result<Self> {
        let api_key = std::env::var(DEEPSEEK_API_KEY_ENV)
            .with_context(|| format!("{DEEPSEEK_API_KEY_ENV} must be set"))?;
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .context("build http client")?;
        Ok(Self {
            http,
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
            api_key,
            model: model.to_string(),
        })
    }

    /// One non-streaming completion at temperature 0: `system` + `user` in, the
    /// assistant text out. Transient failures are retried with exponential
    /// backoff (see [`MAX_RETRIES`]).
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": 0.0,
            "stream": false,
        });
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.api_key))
            .context("invalid api key header")?;

        let mut last_error = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_BASE_DELAY * 2u32.pow(attempt - 1)).await;
            }
            match self.try_once(&url, &body, &auth).await {
                Ok(text) => return Ok(text),
                Err(Attempt {
                    retryable: false,
                    error,
                }) => return Err(error),
                Err(Attempt {
                    retryable: true,
                    error,
                }) => {
                    tracing::warn!(attempt = attempt + 1, error = %error, "judge chat transient failure; retrying");
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("chat failed")))
            .with_context(|| format!("chat completions failed after {MAX_RETRIES} retries"))
    }

    /// A single attempt, tagging its error transient (worth retrying) or fatal.
    async fn try_once(
        &self,
        url: &str,
        body: &serde_json::Value,
        auth: &HeaderValue,
    ) -> std::result::Result<String, Attempt> {
        let resp = self
            .http
            .post(url)
            .header(AUTHORIZATION, auth.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(body)
            .send()
            .await
            .map_err(|e| {
                Attempt::transient(anyhow::Error::new(e).context("chat completions request failed"))
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // 5xx and 429 are transient; other 4xx (auth, bad request) are not.
            let retryable =
                status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            let error = anyhow::anyhow!("chat completions returned {status}: {}", preview(&text));
            return Err(Attempt { retryable, error });
        }
        let parsed: ApiResponse = serde_json::from_str(&text).map_err(|e| {
            Attempt::fatal(
                anyhow::Error::new(e).context(format!("parse chat response: {}", preview(&text))),
            )
        })?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }
}

fn preview(s: &str) -> String {
    s.chars().take(ERR_PREVIEW).collect()
}

/// A failed [`ChatClient::try_once`], tagged with whether a retry might help.
struct Attempt {
    retryable: bool,
    error: anyhow::Error,
}

impl Attempt {
    fn transient(error: anyhow::Error) -> Self {
        Self {
            retryable: true,
            error,
        }
    }
    fn fatal(error: anyhow::Error) -> Self {
        Self {
            retryable: false,
            error,
        }
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    choices: Vec<ApiChoice>,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiMessage,
}

#[derive(Deserialize)]
struct ApiMessage {
    #[serde(default)]
    content: Option<String>,
}
