//! OAuth 2.0 PKCE + device-code flows against `auth.openai.com`.
//!
//! Three async entry points:
//!  - [`pkce_login`] — opens a one-shot HTTP listener on `127.0.0.1`,
//!    constructs the authorize URL, prints it for the user to open in a
//!    browser, awaits the callback with `?code=...`, then exchanges the
//!    code for tokens.
//!  - [`device_code_login`] — calls `/api/accounts/deviceauth/usercode` to
//!    mint a user code, prints it for the user to enter in a browser, then
//!    polls `/api/accounts/deviceauth/token` until the code is approved
//!    and exchanges the resulting authorization code for tokens.
//!  - [`refresh`] — exchanges a refresh_token for a fresh bundle.
//!
//! All three return [`OAuthTokenBundle`]; persistence is the caller's job
//! (via [`super::token_store::VaultTokenStore::save`]).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::time::sleep;
use tracing::{debug, info, warn};
use url::form_urlencoded;

use super::token_bundle::OAuthTokenBundle;
use crate::{LlmError, Result};

/// OAuth client_id for the Codex CLI app. Public per the codex-rs source —
/// see `codex-rs/login/src/auth/manager.rs`. We re-use it because the
/// `auth.openai.com` issuer only mints OAuth tokens for registered clients,
/// and there isn't a separate "external tools" client to register against.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
const SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// Codex's anti-bot allowlist requires this header on every request to the
/// authorize URL and the Codex backend. Without it the edge returns 403 with
/// a Cloudflare challenge.
pub const ORIGINATOR: &str = "codex_cli_rs";
/// `auth.openai.com`'s edge silently rewrites the body to an empty / HTML
/// response when the request comes in without a recognizable browser-like
/// User-Agent (reqwest's default `reqwest/0.x` triggers it). Pin our own
/// so the device-code endpoints actually return JSON.
const USER_AGENT: &str = concat!("aura/", env!("CARGO_PKG_VERSION"));
const PKCE_VERIFIER_BYTES: usize = 64;
/// Cap on the body snippet we surface in decode errors, in chars. Long
/// HTML error pages from the edge can run into the tens of kilobytes —
/// truncating keeps the error single-line readable while still pointing
/// at the failure mode.
const ERROR_BODY_TRUNCATE: usize = 512;

fn truncate_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= ERROR_BODY_TRUNCATE {
        trimmed.to_string()
    } else {
        let mut end = ERROR_BODY_TRUNCATE;
        while !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

fn parse_oauth_json<T: serde::de::DeserializeOwned>(body: &str, context: &str) -> Result<T> {
    serde_json::from_str(body).map_err(|e| {
        LlmError::Decode(format!(
            "openai-subscription: {context} parse: {e}; body: {}",
            truncate_body(body)
        ))
    })
}

/// Local callback port the PKCE flow listens on. Matches the Codex CLI's
/// default; OpenAI's authorize page expects this exact URI.
pub const CALLBACK_PORT: u16 = 1455;

/// Maximum time we'll wait for the browser to open the URL and the
/// user to complete sign-in. After this, the listener is dropped and
/// the call returns an error so we don't leak the port + spawned task
/// indefinitely if the user walks away.
const PKCE_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// PKCE flow: generate verifier+challenge, bind a one-shot listener on
/// 127.0.0.1:CALLBACK_PORT, hand the authorize URL to `present_url`,
/// wait (bounded) for the browser callback, exchange code for tokens.
pub async fn pkce_login(
    present_url: impl FnOnce(&str) -> io::Result<()> + Send + 'static,
    client: &reqwest::Client,
) -> Result<OAuthTokenBundle> {
    let pkce = PkceCodes::generate()?;
    let redirect_uri = format!("http://localhost:{CALLBACK_PORT}/auth/callback");
    let state = generate_state()?;

    // Bind BEFORE handing the URL to the user — a very fast click can
    // otherwise race the bind and produce "connection refused".
    let listener = bind_local_listener(CALLBACK_PORT).map_err(|e| {
        LlmError::Config(format!(
            "openai-subscription: cannot bind 127.0.0.1:{CALLBACK_PORT} for OAuth callback ({e}) \
             — close any other Codex / aura login in flight, or pass --device-code"
        ))
    })?;

    let authorize_url = build_authorize_url(&redirect_uri, &pkce.code_challenge, &state);
    debug!(authorize_url = %authorize_url, "opening OAuth authorize URL");
    present_url(&authorize_url).map_err(|e| {
        LlmError::Config(format!("openai-subscription: failed to present URL: {e}"))
    })?;

    // TcpListener::accept is sync — run in spawn_blocking and bound the
    // wait so we don't leak port + task if the user abandons the flow.
    let expected_state = state.clone();
    let callback_task = tokio::task::spawn_blocking(move || -> io::Result<String> {
        await_callback(listener, &expected_state)
    });
    let callback = match tokio::time::timeout(PKCE_CALLBACK_TIMEOUT, callback_task).await {
        Ok(join_res) => join_res
            .map_err(|e| {
                LlmError::Internal(anyhow::anyhow!(
                    "openai-subscription: callback task panicked: {e}"
                ))
            })?
            .map_err(|e| LlmError::Auth(format!("openai-subscription: callback failed: {e}")))?,
        Err(_) => {
            return Err(LlmError::Auth(format!(
                "openai-subscription: PKCE callback timed out after {}s — pass --device-code if \
                 you can't open a browser on this host",
                PKCE_CALLBACK_TIMEOUT.as_secs()
            )));
        }
    };

    let raw =
        exchange_code_for_tokens(&callback, &redirect_uri, &pkce.code_verifier, client).await?;
    let (access, refresh, id) = unpack_token_response(raw)?;
    OAuthTokenBundle::from_token_response(access, refresh, id)
}

/// Device-code flow: mint a user code, ask the user to enter it on
/// `https://auth.openai.com/codex/device`, poll for approval, then exchange
/// the resulting authorization code for tokens.
pub async fn device_code_login(
    present_prompt: impl FnOnce(&DeviceCode) + Send,
    client: &reqwest::Client,
) -> Result<OAuthTokenBundle> {
    let api_base = format!("{ISSUER}/api/accounts");

    // Step 1: ask for a user code. Read body as text first so a non-JSON
    // response (HTML challenge, empty body, OAuth error envelope) ends up
    // in the decode error instead of being swallowed by reqwest.
    let resp = client
        .post(format!("{api_base}/deviceauth/usercode"))
        .header("originator", ORIGINATOR)
        .header("User-Agent", USER_AGENT)
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| crate::reqwest_to_error(e, "openai-subscription: device code request"))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        crate::reqwest_to_error(e, "openai-subscription: device code response read")
    })?;
    if !status.is_success() {
        return Err(crate::status_to_error(
            status.as_u16(),
            format!(
                "openai-subscription: device code request returned {status}: {}",
                truncate_body(&body)
            ),
        ));
    }
    let usercode_resp: UserCodeResponse = parse_oauth_json(&body, "device code response")?;

    let device = DeviceCode {
        user_code: usercode_resp.user_code.clone(),
        verification_url: format!("{ISSUER}/codex/device"),
    };
    present_prompt(&device);

    // Step 2: poll until approved or 15 minutes elapse.
    let interval = usercode_resp.interval.max(1);
    let max_wait = Duration::from_secs(15 * 60);
    let start = Instant::now();
    let token_url = format!("{api_base}/deviceauth/token");
    let code_resp = loop {
        if start.elapsed() >= max_wait {
            return Err(LlmError::Auth(
                "openai-subscription: device-code authorization timed out (15 min)".into(),
            ));
        }
        let resp = client
            .post(&token_url)
            .header("originator", ORIGINATOR)
            .header("User-Agent", USER_AGENT)
            .json(&serde_json::json!({
                "device_auth_id": usercode_resp.device_auth_id,
                "user_code": usercode_resp.user_code,
            }))
            .send()
            .await
            .map_err(|e| crate::reqwest_to_error(e, "openai-subscription: device-code poll"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND {
            // "User hasn't approved yet" — back off and retry.
            sleep(Duration::from_secs(interval)).await;
            continue;
        }
        let body = resp.text().await.map_err(|e| {
            crate::reqwest_to_error(e, "openai-subscription: device-code poll body")
        })?;
        if status.is_success() {
            break parse_oauth_json::<DeviceCodeSuccess>(&body, "device-code poll")?;
        }
        return Err(crate::status_to_error(
            status.as_u16(),
            format!(
                "openai-subscription: device-code poll failed ({status}): {}",
                truncate_body(&body)
            ),
        ));
    };

    // Step 3: the device-code response gives us a fresh PKCE pair + auth code;
    // exchange it the same way the browser flow does.
    let redirect_uri = format!("{ISSUER}/deviceauth/callback");
    let raw = exchange_code_for_tokens_with_redirect(
        &code_resp.authorization_code,
        &redirect_uri,
        &code_resp.code_verifier,
        client,
    )
    .await?;
    let (access, refresh, id) = unpack_token_response(raw)?;
    OAuthTokenBundle::from_token_response(access, refresh, id)
}

/// `auth.openai.com/oauth/token` returns each field optional in the wire
/// schema even though they're always present in practice. Treat any of the
/// three being missing as a hard failure with a clear diagnostic.
fn unpack_token_response(resp: TokenEndpointResponse) -> Result<(String, String, String)> {
    let access = resp.access_token.ok_or_else(|| {
        LlmError::Decode("openai-subscription: token response missing access_token".into())
    })?;
    let refresh = resp.refresh_token.ok_or_else(|| {
        LlmError::Decode("openai-subscription: token response missing refresh_token".into())
    })?;
    let id = resp.id_token.ok_or_else(|| {
        LlmError::Decode("openai-subscription: token response missing id_token".into())
    })?;
    Ok((access, refresh, id))
}

/// Exchange a refresh_token for a new bundle. `Permanent` = re-login
/// required (caller should clear the vault); `Transient` = try again later.
pub async fn refresh(
    refresh_token: &str,
    client: &reqwest::Client,
) -> std::result::Result<OAuthTokenBundle, RefreshError> {
    let endpoint = format!("{ISSUER}/oauth/token");
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .finish();
    let resp = client
        .post(&endpoint)
        .header("originator", ORIGINATOR)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

    let status = resp.status();
    let body_text = resp
        .text()
        .await
        .map_err(|e| RefreshError::Transient(format!("refresh response read: {e}")))?;
    if status.is_success() {
        let body: TokenEndpointResponse = serde_json::from_str(&body_text).map_err(|e| {
            RefreshError::Transient(format!(
                "refresh response parse: {e}; body: {}",
                truncate_body(&body_text)
            ))
        })?;
        // Server may rotate the refresh_token; keep the old one if it didn't.
        let next_refresh = body
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string());
        let access = body.access_token.ok_or_else(|| {
            RefreshError::Transient("refresh response missing access_token".into())
        })?;
        let id = body
            .id_token
            .ok_or_else(|| RefreshError::Transient("refresh response missing id_token".into()))?;
        let bundle = OAuthTokenBundle::from_token_response(access, next_refresh, id)
            .map_err(|e| RefreshError::Transient(format!("bundle build: {e}")))?;
        info!(
            event = "openai_subscription_token_refresh",
            outcome = "success",
            "refreshed openai-subscription token"
        );
        return Ok(bundle);
    }

    let body_snippet = truncate_body(&body_text);
    if status == reqwest::StatusCode::UNAUTHORIZED {
        warn!(
            event = "openai_subscription_token_refresh",
            outcome = "permanent",
            "refresh permanently failed: {body_snippet}"
        );
        return Err(RefreshError::Permanent(body_snippet));
    }
    warn!(
        event = "openai_subscription_token_refresh",
        outcome = "transient",
        %status,
        "refresh transiently failed"
    );
    Err(RefreshError::Transient(format!(
        "status={status}: {body_snippet}"
    )))
}

/// Outcome of a [`refresh`] attempt.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// 401 from the token endpoint — `refresh_token_expired` /
    /// `refresh_token_reused` / `refresh_token_invalidated`. The vault entry
    /// must be cleared and the user must re-login.
    #[error("openai-subscription refresh permanently failed: {0}")]
    Permanent(String),
    /// 5xx, network blip, parse error. Don't clear the vault entry — caller
    /// should surface the error and let the next call retry.
    #[error("openai-subscription refresh transiently failed: {0}")]
    Transient(String),
}

/// Best-effort RFC 7009 revoke. Caller decides success/failure handling;
/// either way the local vault entry is the user's source of truth for
/// "logged out".
pub async fn revoke(refresh_token: &str, client: &reqwest::Client) -> std::io::Result<()> {
    let endpoint = format!("{ISSUER}/oauth/revoke");
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", CLIENT_ID)
        .append_pair("token", refresh_token)
        .append_pair("token_type_hint", "refresh_token")
        .finish();
    let resp = client
        .post(&endpoint)
        .header("originator", ORIGINATOR)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(std::io::Error::other)?;
    let status = resp.status();
    if status.is_success() {
        info!(
            event = "openai_subscription_token_revoke",
            outcome = "success",
            "openai-subscription: refresh token revoked server-side"
        );
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        warn!(
            event = "openai_subscription_token_revoke",
            outcome = "non_success_status",
            %status,
            "openai-subscription: revoke returned non-success status: {body}"
        );
        Err(std::io::Error::other(format!(
            "revoke status {status}: {body}"
        )))
    }
}

/// User-facing payload for the device-code prompt.
pub struct DeviceCode {
    pub user_code: String,
    pub verification_url: String,
}

#[derive(Deserialize)]
struct UserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    // OpenAI's edge has shipped both shapes here:
    // older `5` (u64), current `"5"` (string). Match openclaw's
    // `normalizePositiveMilliseconds`: accept either, fall back to
    // `default_poll_interval` when missing or unparseable.
    #[serde(
        default = "default_poll_interval",
        deserialize_with = "deserialize_seconds"
    )]
    interval: u64,
}

fn default_poll_interval() -> u64 {
    5
}

fn deserialize_seconds<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr<'a> {
        Num(u64),
        // Borrow when possible to avoid allocating for the common short form.
        Str(std::borrow::Cow<'a, str>),
    }
    match Option::<NumOrStr>::deserialize(deserializer)? {
        None => Ok(default_poll_interval()),
        Some(NumOrStr::Num(n)) => Ok(n),
        Some(NumOrStr::Str(s)) => s
            .trim()
            .parse::<u64>()
            .map_err(|e| D::Error::custom(format!("interval `{s}` is not a u64: {e}"))),
    }
}

#[derive(Deserialize)]
struct DeviceCodeSuccess {
    authorization_code: String,
    code_verifier: String,
    #[serde(default)]
    #[allow(dead_code)] // server returns it; we don't need it client-side
    code_challenge: String,
}

#[derive(Deserialize)]
struct TokenEndpointResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

struct PkceCodes {
    code_verifier: String,
    code_challenge: String,
}

impl PkceCodes {
    fn generate() -> Result<Self> {
        let mut bytes = [0u8; PKCE_VERIFIER_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let digest = Sha256::digest(code_verifier.as_bytes());
        let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        Ok(Self {
            code_verifier,
            code_challenge,
        })
    }
}

fn generate_state() -> Result<String> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn build_authorize_url(redirect_uri: &str, code_challenge: &str, state: &str) -> String {
    let qs = form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        // These two flags make the issuer mint a token shape that includes
        // the `chatgpt_account_id` and `chatgpt_plan_type` claims we need
        // to set the `ChatGPT-Account-Id` header and to display plan info.
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR)
        .finish();
    format!("{ISSUER}/oauth/authorize?{qs}")
}

fn bind_local_listener(port: u16) -> io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
}

fn await_callback(listener: TcpListener, expected_state: &str) -> io::Result<String> {
    listener.set_nonblocking(false)?;
    // Single shot: take the first inbound connection and parse the request.
    let (mut stream, _peer) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let request_line = read_request_line(&mut stream)?;
    let path_and_query = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP request line"))?;

    let url = format!("http://localhost{path_and_query}");
    let parsed =
        url::Url::parse(&url).map_err(|e| io::Error::other(format!("callback URL parse: {e}")))?;
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }

    if let Some(err) = error {
        write_response(&mut stream, 400, "text/plain", err.as_bytes()).ok();
        return Err(io::Error::other(format!("OAuth callback error: {err}")));
    }
    let code = code.ok_or_else(|| io::Error::other("OAuth callback missing `code` param"))?;
    let state = state.ok_or_else(|| io::Error::other("OAuth callback missing `state` param"))?;
    if state != expected_state {
        write_response(&mut stream, 400, "text/plain", b"state mismatch").ok();
        return Err(io::Error::other(
            "OAuth callback state mismatch (possible CSRF)",
        ));
    }

    let body =
        b"<html><body><h1>aura: ChatGPT subscription linked</h1><p>You can close this tab.</p></body></html>";
    write_response(&mut stream, 200, "text/html; charset=utf-8", body).ok();
    Ok(code)
}

fn read_request_line(stream: &mut TcpStream) -> io::Result<String> {
    // Read up to 4 KiB looking for the first \r\n. The request line is the
    // only piece we care about; we ignore headers and body entirely.
    let mut buf = [0u8; 4096];
    let mut total = 0;
    loop {
        if total == buf.len() {
            return Err(io::Error::other("HTTP request line too long"));
        }
        let n = stream.read(&mut buf[total..])?;
        if n == 0 {
            return Err(io::Error::other("client closed before sending request"));
        }
        total += n;
        if let Some(idx) = buf[..total].windows(2).position(|w| w == b"\r\n") {
            return Ok(String::from_utf8_lossy(&buf[..idx]).to_string());
        }
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "OK",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

async fn exchange_code_for_tokens(
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    client: &reqwest::Client,
) -> Result<TokenEndpointResponse> {
    exchange_code_for_tokens_with_redirect(code, redirect_uri, code_verifier, client).await
}

async fn exchange_code_for_tokens_with_redirect(
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    client: &reqwest::Client,
) -> Result<TokenEndpointResponse> {
    let endpoint = format!("{ISSUER}/oauth/token");
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code_verifier", code_verifier)
        .finish();
    let resp = client
        .post(&endpoint)
        .header("originator", ORIGINATOR)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| crate::reqwest_to_error(e, "openai-subscription: token exchange transport"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| crate::reqwest_to_error(e, "openai-subscription: token exchange body read"))?;
    if !status.is_success() {
        return Err(crate::status_to_error(
            status.as_u16(),
            format!(
                "openai-subscription: token exchange returned {status}: {}",
                truncate_body(&body)
            ),
        ));
    }
    parse_oauth_json::<TokenEndpointResponse>(&body, "token exchange")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_codes_are_well_formed() {
        let pkce = PkceCodes::generate().unwrap();
        // verifier: 64 random bytes → ~86 base64url chars
        assert!(pkce.code_verifier.len() >= 80);
        assert!(
            pkce.code_verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        // challenge: SHA-256 → 32 bytes → 43 base64url chars
        assert_eq!(pkce.code_challenge.len(), 43);
    }

    #[test]
    fn truncate_body_preserves_short_input_and_trims_whitespace() {
        assert_eq!(truncate_body("ok"), "ok");
        assert_eq!(truncate_body("  spaced  "), "spaced");
    }

    #[test]
    fn truncate_body_caps_long_input_with_ellipsis() {
        let long = "x".repeat(ERROR_BODY_TRUNCATE + 100);
        let out = truncate_body(&long);
        assert!(out.ends_with('…'));
        assert_eq!(
            out.chars().filter(|c| *c == 'x').count(),
            ERROR_BODY_TRUNCATE
        );
    }

    #[test]
    fn parse_oauth_json_failure_includes_body_in_error() {
        // The whole reason this helper exists: the user can't see what the
        // server returned without the body in the error message. Use
        // `serde_json::Value` so we don't need to touch wire-struct derives
        // just to get an unwrap_err in the test.
        let err = match parse_oauth_json::<serde_json::Value>(
            "<html>cf challenge</html>",
            "device code response",
        ) {
            Ok(v) => panic!("expected parse failure on HTML body, got Ok({v:?})"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("device code response parse"), "msg: {msg}");
        assert!(msg.contains("<html>cf challenge</html>"), "msg: {msg}");
    }

    #[test]
    fn usercode_response_accepts_interval_as_number_or_string() {
        // Number form (older edge response).
        let parsed: UserCodeResponse =
            serde_json::from_str(r#"{"device_auth_id":"id","user_code":"CODE","interval":5}"#)
                .unwrap();
        assert_eq!(parsed.interval, 5);

        // String form (current edge response, what tripped the original bug).
        let parsed: UserCodeResponse =
            serde_json::from_str(r#"{"device_auth_id":"id","user_code":"CODE","interval":"5"}"#)
                .unwrap();
        assert_eq!(parsed.interval, 5);

        // Whitespace around the digits stays accepted.
        let parsed: UserCodeResponse = serde_json::from_str(
            r#"{"device_auth_id":"id","user_code":"CODE","interval":"  7  "}"#,
        )
        .unwrap();
        assert_eq!(parsed.interval, 7);

        // Missing field falls back to the default cadence.
        let parsed: UserCodeResponse =
            serde_json::from_str(r#"{"device_auth_id":"id","user_code":"CODE"}"#).unwrap();
        assert_eq!(parsed.interval, default_poll_interval());

        // Garbage string surfaces as a parse error rather than panicking.
        let err = match serde_json::from_str::<UserCodeResponse>(
            r#"{"device_auth_id":"id","user_code":"CODE","interval":"oops"}"#,
        ) {
            Ok(_) => panic!("expected parse error for non-numeric interval string"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("interval"), "msg: {err}");
    }

    #[test]
    fn user_agent_pinned_to_aura_crate_version() {
        // The default reqwest User-Agent (`reqwest/0.x`) trips the edge's
        // bot heuristics on `auth.openai.com` and the body comes back
        // empty/HTML — pinning a stable identifier is what makes the
        // device-code endpoint return JSON. Lock the shape so a refactor
        // doesn't accidentally drop it back to default.
        assert!(USER_AGENT.starts_with("aura/"));
    }

    #[test]
    fn authorize_url_contains_required_params() {
        let url = build_authorize_url(
            "http://localhost:1455/auth/callback",
            "abc-challenge",
            "xyz-state",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("code_challenge=abc-challenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("originator=codex_cli_rs"));
        assert!(url.contains("state=xyz-state"));
        assert!(url.contains("id_token_add_organizations=true"));
    }
}
