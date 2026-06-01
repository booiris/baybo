//! Minimal OpenAI-compatible chat client (→ DeepSeek by default), used by the
//! judge. A single non-streaming `POST /chat/completions` at temperature 0,
//! returning the assistant text. (The answer side runs inside the real Aura
//! agent via `aura prompt`, not through this client.)

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
    /// assistant text out.
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
        let resp = self
            .http
            .post(&url)
            .header(AUTHORIZATION, auth)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .json(&body)
            .send()
            .await
            .context("chat completions request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("chat completions returned {status}: {}", preview(&text));
        }
        let parsed: ApiResponse = serde_json::from_str(&text)
            .with_context(|| format!("parse chat response: {}", preview(&text)))?;
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
