//! Sign in with ChatGPT dynamic registration and token sharing.
use std::io;
use std::time::Duration;

use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use url::Url;

use super::legacy_oauth;
use super::token_bundle::{ChatGptConnection, OAuthTokenBundle};
use super::token_store::VaultTokenStore;
use crate::{LlmError, Result};

pub const ISSUER: &str = "https://auth.openai.com";
pub const CALLBACK_PORT: u16 = 1455;
pub(super) const SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const RESOURCE: &str = super::DEFAULT_BASE_URL;
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CALLBACK_LINE: usize = 8192;
const PKCE_VERIFIER_BYTES: usize = 64;
const STATE_BYTES: usize = 32;
const TOKEN_PATH: &str = "/api/accounts/oauth/token";
const DISCOVERY_PATH: &str = "/.well-known/openid-configuration";
const USER_AGENT: &str = concat!("baybo/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub(super) struct Registration {
    pub client_id: String,
    pub subject: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    revocation_endpoint: String,
}

#[derive(Deserialize)]
struct Tokens {
    #[serde(default)]
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    #[serde(default)]
    expires_in: i64,
    scope: Option<String>,
    #[serde(default)]
    token_type: String,
}

#[derive(Deserialize)]
struct Identity {
    sub: String,
    nonce: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
}

fn auth_error(message: impl std::fmt::Display) -> LlmError {
    LlmError::Auth(format!("ChatGPT sign-in: {message}"))
}

fn random_value(size: usize) -> String {
    let mut bytes = vec![0u8; size];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn trusted_endpoint(endpoint: &str) -> Result<()> {
    let parsed = Url::parse(endpoint).map_err(auth_error)?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("auth.openai.com")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(auth_error("discovery returned an untrusted endpoint"));
    }
    Ok(())
}

async fn discovery(http: &reqwest::Client) -> Result<Discovery> {
    let config: Discovery = http
        .get(format!("{ISSUER}{DISCOVERY_PATH}"))
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(auth_error)?
        .error_for_status()
        .map_err(auth_error)?
        .json()
        .await
        .map_err(|_| auth_error("invalid OpenID discovery response"))?;
    if config.issuer != ISSUER {
        return Err(auth_error("discovery issuer mismatch"));
    }
    trusted_endpoint(&config.jwks_uri)?;
    trusted_endpoint(&config.revocation_endpoint)?;
    Ok(config)
}

fn verify_with_keys(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    keys: &JwkSet,
) -> Result<Identity> {
    let header = decode_header(token).map_err(|_| auth_error("invalid ID token header"))?;
    let key = header
        .kid
        .as_deref()
        .and_then(|kid| keys.find(kid))
        .ok_or_else(|| auth_error("unknown ID token signing key"))?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "sub", "iss", "aud"]);
    validation.leeway = 5;
    let identity = decode::<Identity>(
        token,
        &DecodingKey::from_jwk(key).map_err(auth_error)?,
        &validation,
    )
    .map_err(|_| auth_error("ID token signature or claims failed validation"))?
    .claims;
    if identity.sub.is_empty()
        || nonce.is_some_and(|expected| identity.nonce.as_deref() != Some(expected))
    {
        return Err(auth_error("ID token subject or nonce mismatch"));
    }
    Ok(identity)
}

async fn verify_identity(
    token: &str,
    client_id: &str,
    nonce: Option<&str>,
    http: &reqwest::Client,
) -> Result<Identity> {
    let config = discovery(http).await?;
    let keys: JwkSet = http
        .get(config.jwks_uri)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(auth_error)?
        .error_for_status()
        .map_err(auth_error)?
        .json()
        .await
        .map_err(|_| auth_error("invalid OpenID signing keys"))?;
    verify_with_keys(token, client_id, nonce, &keys)
}

fn authorize_url(
    redirect: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
    client_id: &str,
    host_id: &str,
    request_consent: bool,
) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("client_id", client_id)
        .append_pair("ext_agent_host_id", host_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", SCOPES)
        .append_pair("resource", RESOURCE)
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge);
    if client_id == DYNAMIC_CLIENT {
        query.append_pair("agent_name_hint", "Baybo");
    } else if request_consent {
        query.append_pair("prompt", "consent");
    }
    format!("{ISSUER}/api/accounts/authorize?{}", query.finish())
}

fn callback_result(
    target: &str,
    expected_state: &str,
    requested_client: &str,
) -> Result<(String, String)> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).map_err(auth_error)?;
    if url.path() != "/auth/callback" {
        return Err(auth_error("callback path mismatch"));
    }
    let mut pairs = std::collections::HashMap::new();
    for (key, value) in url.query_pairs() {
        if pairs.insert(key, value).is_some() {
            return Err(auth_error("duplicate callback parameter"));
        }
    }
    if pairs.get("state").map(|s| s.as_ref()) != Some(expected_state) {
        return Err(auth_error("callback state mismatch"));
    }
    if pairs.contains_key("error") {
        return Err(auth_error(
            "authorization failed; start sign-in again; current credentials were kept",
        ));
    }
    let issued = pairs
        .get("client_id")
        .map(|s| s.as_ref())
        .unwrap_or(requested_client);
    if issued == DYNAMIC_CLIENT
        || issued.is_empty()
        || issued.chars().any(char::is_whitespace)
        || (requested_client != DYNAMIC_CLIENT && issued != requested_client)
    {
        return Err(auth_error("missing or mismatched issued client ID"));
    }
    let code = pairs
        .get("code")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| auth_error("callback missing authorization code"))?;
    Ok((code.to_string(), issued.to_owned()))
}

fn pasted_callback_result(
    input: &str,
    redirect: &str,
    state: &str,
    client_id: &str,
) -> Result<(String, String)> {
    if input.len() > MAX_CALLBACK_LINE {
        return Err(auth_error("callback URL is too long"));
    }
    let url = Url::parse(input.trim())
        .map_err(|_| auth_error("paste the complete callback URL from the browser address bar"))?;
    let expected = Url::parse(redirect).map_err(auth_error)?;
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port_or_known_default() != expected.port_or_known_default()
        || url.path() != expected.path()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(auth_error(
            "callback address mismatch; copy the complete callback URL for this sign-in attempt",
        ));
    }
    let query = url
        .query()
        .ok_or_else(|| auth_error("callback URL missing query parameters"))?;
    callback_result(&format!("{}?{query}", url.path()), state, client_id)
}

pub enum LoginCallback {
    Browser,
    Pasted(String),
}

async fn await_callback(
    listener: tokio::net::TcpListener,
    state: &str,
    client_id: &str,
) -> Result<(String, String)> {
    loop {
        let (stream, _) = listener.accept().await.map_err(auth_error)?;
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        let read = tokio::time::timeout(CALLBACK_READ_TIMEOUT, async {
            use tokio::io::AsyncReadExt;
            (&mut reader)
                .take(MAX_CALLBACK_LINE as u64)
                .read_until(b'\n', &mut line)
                .await
        })
        .await;
        if !matches!(read, Ok(Ok(_))) {
            continue;
        }
        let line = String::from_utf8(line).map_err(|_| auth_error("invalid callback request"))?;
        let target = line.split_whitespace().nth(1).unwrap_or("");
        // Browser favicon requests must not consume the pending sign-in.
        if !target.starts_with("/auth/callback?") {
            continue;
        }
        let result = callback_result(target, state, client_id);
        let (status, body) = if result.is_ok() {
            (
                "200 OK",
                "Baybo: authorization received. Return to the terminal to finish sign-in.",
            )
        } else {
            (
                "400 Bad Request",
                "Baybo: invalid authorization callback. See the terminal.",
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = tokio::time::timeout(
            CALLBACK_READ_TIMEOUT,
            reader.get_mut().write_all(response.as_bytes()),
        )
        .await;
        return result;
    }
}

pub async fn pkce_login(
    present_url: impl FnOnce(&str) -> io::Result<()> + Send + 'static,
    http: &reqwest::Client,
    store: &VaultTokenStore,
) -> Result<OAuthTokenBundle> {
    pkce_login_with_callback(
        |url, _| {
            present_url(url)?;
            Ok(LoginCallback::Browser)
        },
        http,
        store,
    )
    .await
}

pub async fn pkce_login_with_callback(
    present_url: impl FnOnce(&str, &str) -> io::Result<LoginCallback> + Send,
    http: &reqwest::Client,
    store: &VaultTokenStore,
) -> Result<OAuthTokenBundle> {
    let host = store.host_id().await?;
    let previous = store.load().await?;
    let registration = match previous
        .as_ref()
        .and_then(|bundle| bundle.connection.as_ref())
    {
        Some(connection) => Some(Registration {
            client_id: connection.client_id.clone(),
            subject: Some(connection.subject.clone()),
        }),
        None => store.registration().await?,
    };
    let client_id = registration
        .as_ref()
        .map(|r| r.client_id.as_str())
        .unwrap_or(DYNAMIC_CLIENT);
    let verifier = random_value(PKCE_VERIFIER_BYTES);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_value(STATE_BYTES);
    let nonce = random_value(STATE_BYTES);
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(auth_error)?
        }
        Err(error) => return Err(auth_error(error)),
    };
    let port = listener.local_addr().map_err(auth_error)?.port();
    let redirect = format!("http://127.0.0.1:{port}/auth/callback");
    let request_consent = previous
        .as_ref()
        .is_some_and(|b| b.connection.is_some() && !b.sharing_enabled());
    let deadline = tokio::time::Instant::now() + LOGIN_TIMEOUT;
    let callback = present_url(
        &authorize_url(
            &redirect,
            &challenge,
            &state,
            &nonce,
            client_id,
            &host,
            request_consent,
        ),
        &redirect,
    )
    .map_err(auth_error)?;
    if tokio::time::Instant::now() >= deadline {
        return Err(auth_error(
            "sign-in timed out; start sign-in again and use the new link",
        ));
    }
    let (code, issued) = match callback {
        LoginCallback::Pasted(input) => pasted_callback_result(&input, &redirect, &state, client_id)?,
        LoginCallback::Browser =>
        tokio::time::timeout_at(deadline, await_callback(listener, &state, client_id))
            .await
            .map_err(|_| {
                auth_error(
                    "browser callback timed out; start sign-in again and select the remote / paste callback option when using a browser on another computer",
                )
            })??,
    };
    let retained = Registration {
        client_id: issued.clone(),
        subject: registration.as_ref().and_then(|r| r.subject.clone()),
    };
    store.save_registration(&retained).await?;
    let response = http
        .post(format!("{ISSUER}{TOKEN_PATH}"))
        .header("User-Agent", USER_AGENT)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", issued.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(auth_error)?;
    if !response.status().is_success() {
        let status = response.status();
        let code = response
            .json::<OAuthError>()
            .await
            .ok()
            .map(|error| error.error)
            .unwrap_or_else(|| "unknown_error".into());
        return Err(auth_error(format!(
            "token exchange returned {status}, error={code}; issued client ID was retained for retry"
        )));
    }
    let tokens: Tokens = response
        .json()
        .await
        .map_err(|_| auth_error("invalid token response"))?;
    let id_token = tokens
        .id_token
        .as_deref()
        .ok_or_else(|| auth_error("missing ID token"))?;
    let identity = verify_identity(id_token, &issued, Some(&nonce), http).await?;
    if retained
        .subject
        .as_deref()
        .is_some_and(|subject| subject != identity.sub)
    {
        return Err(auth_error(
            "returning account identity changed; current credentials were kept",
        ));
    }
    let bundle = bundle_from_tokens(tokens, issued, identity.sub, None)?;
    store
        .save_registration(&Registration {
            client_id: retained.client_id,
            subject: bundle.connection.as_ref().map(|c| c.subject.clone()),
        })
        .await?;
    store.save_login(&bundle, http).await?;
    Ok(bundle)
}

fn bundle_from_tokens(
    tokens: Tokens,
    client_id: String,
    subject: String,
    previous: Option<&OAuthTokenBundle>,
) -> Result<OAuthTokenBundle> {
    let scopes: Vec<String> = match tokens.scope {
        Some(scope) => scope.split_whitespace().map(str::to_owned).collect(),
        None => previous
            .and_then(|b| b.connection.as_ref())
            .map(|c| c.scopes.clone())
            .unwrap_or_default(),
    };
    let sharing = scopes.iter().any(|scope| scope == SHARING_SCOPE);
    if sharing
        && (!tokens.token_type.eq_ignore_ascii_case("bearer")
            || tokens.expires_in <= 0
            || tokens.access_token.is_empty())
    {
        return Err(auth_error("invalid bearer token or expiry"));
    }
    let refresh_token = tokens
        .refresh_token
        .filter(|token| !token.is_empty())
        .or_else(|| previous.map(|bundle| bundle.refresh_token.clone()))
        .unwrap_or_default();
    if sharing && refresh_token.is_empty() {
        return Err(auth_error("missing refresh token"));
    }
    Ok(OAuthTokenBundle {
        connection: Some(ChatGptConnection {
            client_id,
            subject,
            scopes,
        }),
        access_token: tokens.access_token,
        refresh_token,
        id_token: tokens
            .id_token
            .or_else(|| previous.map(|b| b.id_token.clone()))
            .ok_or_else(|| auth_error("missing ID token"))?,
        account_id: None,
        expires_at: chrono::Utc::now()
            .timestamp()
            .saturating_add(tokens.expires_in),
        obtained_at: chrono::Utc::now().timestamp(),
    })
}

fn unusable_refresh(error: &str) -> bool {
    matches!(
        error,
        "invalid_grant"
            | "invalid_refresh_token"
            | "token_expired"
            | "refresh_token_expired"
            | "refresh_token_invalidated"
            | "refresh_token_reused"
    )
}

pub async fn refresh(
    bundle: &OAuthTokenBundle,
    http: &reqwest::Client,
) -> std::result::Result<OAuthTokenBundle, RefreshError> {
    refresh_at(ISSUER, bundle, http).await
}

pub(super) async fn refresh_at(
    issuer: &str,
    bundle: &OAuthTokenBundle,
    http: &reqwest::Client,
) -> std::result::Result<OAuthTokenBundle, RefreshError> {
    let Some(connection) = &bundle.connection else {
        return legacy_oauth::refresh_at(issuer, &bundle.refresh_token, http).await;
    };
    let response = http
        .post(format!("{issuer}{TOKEN_PATH}"))
        .header("User-Agent", USER_AGENT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", connection.client_id.as_str()),
            ("refresh_token", bundle.refresh_token.as_str()),
            ("resource", RESOURCE),
        ])
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let code = response
            .json::<OAuthError>()
            .await
            .ok()
            .map(|e| e.error)
            .unwrap_or_default();
        let message = format!("refresh returned {status}, error={code}");
        return Err(if unusable_refresh(&code) {
            RefreshError::Permanent(message)
        } else {
            RefreshError::Transient(message)
        });
    }
    let tokens: Tokens = response
        .json()
        .await
        .map_err(|_| RefreshError::Transient("invalid token response".into()))?;
    if let Some(id_token) = &tokens.id_token {
        let identity = verify_identity(id_token, &connection.client_id, None, http)
            .await
            .map_err(|e| RefreshError::Transient(e.to_string()))?;
        if identity.sub != connection.subject {
            return Err(RefreshError::Permanent("account identity changed".into()));
        }
    }
    bundle_from_tokens(
        tokens,
        connection.client_id.clone(),
        connection.subject.clone(),
        Some(bundle),
    )
    .map_err(|e| RefreshError::Transient(e.to_string()))
}

pub async fn revoke(bundle: &OAuthTokenBundle, http: &reqwest::Client) -> io::Result<()> {
    if bundle.refresh_token.is_empty() {
        return Ok(());
    }
    let Some(connection) = &bundle.connection else {
        return legacy_oauth::revoke(&bundle.refresh_token, http).await;
    };
    let config = discovery(http).await.map_err(io::Error::other)?;
    http.post(config.revocation_endpoint)
        .header("User-Agent", USER_AGENT)
        .form(&[
            ("token", bundle.refresh_token.as_str()),
            ("token_type_hint", "refresh_token"),
            ("client_id", connection.client_id.as_str()),
        ])
        .send()
        .await
        .map_err(io::Error::other)?
        .error_for_status()
        .map_err(io::Error::other)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("openai-subscription refresh permanently failed: {0}")]
    Permanent(String),
    #[error("openai-subscription refresh transiently failed: {0}")]
    Transient(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::{
        encoding::AsDer,
        rsa::{KeyPair, KeySize},
        signature::KeyPair as _,
    };
    use jsonwebtoken::{EncodingKey, Header, encode};
    use serde_json::json;

    fn signed_token(claims: serde_json::Value, key: &EncodingKey) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        encode(&header, &claims, key).unwrap()
    }

    #[test]
    fn identity_validation_rejects_signature_and_claim_mismatches() {
        let key_pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        const PEM_TEMPLATE: &str = r#"-----BEGIN PRIVATE KEY-----
{{key}}
-----END PRIVATE KEY-----"#;
        let pem = PEM_TEMPLATE.replace(
            "{{key}}",
            &base64::engine::general_purpose::STANDARD.encode(key_pair.as_der().unwrap().as_ref()),
        );
        let key = EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap();
        let public = key_pair.public_key();
        let base64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let keys: JwkSet = serde_json::from_value(json!({"keys": [{
            "kty": "RSA", "kid": "test-key", "alg": "RS256", "use": "sig",
            "n": base64.encode(public.modulus().big_endian_without_leading_zero()),
            "e": base64.encode(public.exponent().big_endian_without_leading_zero())
        }]}))
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let claims = json!({"iss": ISSUER, "aud": "oaiapp_test", "sub": "user-1", "exp": now + 3600, "nonce": "n"});
        let token = signed_token(claims.clone(), &key);
        assert_eq!(
            verify_with_keys(&token, "oaiapp_test", Some("n"), &keys)
                .unwrap()
                .sub,
            "user-1"
        );
        assert!(verify_with_keys(&token, "other-client", Some("n"), &keys).is_err());
        assert!(verify_with_keys(&token, "oaiapp_test", Some("wrong"), &keys).is_err());
        for (field, value) in [
            ("iss", json!("https://attacker.example")),
            ("exp", json!(now - 3600)),
            ("sub", json!("")),
        ] {
            let mut invalid = claims.clone();
            invalid[field] = value;
            assert!(
                verify_with_keys(
                    &signed_token(invalid, &key),
                    "oaiapp_test",
                    Some("n"),
                    &keys
                )
                .is_err()
            );
        }
        let mut parts: Vec<_> = token.split('.').map(str::to_owned).collect();
        parts[1] = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(json!({"sub": "attacker"}).to_string());
        assert!(verify_with_keys(&parts.join("."), "oaiapp_test", Some("n"), &keys).is_err());
    }

    #[test]
    fn registration_and_reauthorization_are_bound_to_the_pending_request() {
        let valid = "/auth/callback?code=c&state=s&client_id=oaiapp_test";
        assert_eq!(
            callback_result(valid, "s", DYNAMIC_CLIENT).unwrap(),
            ("c".into(), "oaiapp_test".into())
        );
        assert!(callback_result(valid, "wrong", DYNAMIC_CLIENT).is_err());
        assert!(callback_result(valid, "s", "oaiapp_other").is_err());
        assert!(callback_result("/auth/callback?code=c&state=s", "s", DYNAMIC_CLIENT).is_err());
        assert!(
            callback_result("/auth/callback?code=c&state=s&state=s", "s", "oaiapp_test").is_err()
        );
        assert!(
            callback_result(
                "/auth/callback?error=access_denied&state=s",
                "s",
                DYNAMIC_CLIENT
            )
            .is_err()
        );
        assert_eq!(
            callback_result("/auth/callback?code=c&state=s", "s", "oaiapp_test")
                .unwrap()
                .1,
            "oaiapp_test"
        );
    }

    #[test]
    fn pasted_callback_binds_address_state_and_client_without_echoing_input() {
        let redirect = "http://127.0.0.1:43123/auth/callback";
        let query = "code=secret-code&state=s&client_id=oaiapp_test";
        assert_eq!(
            pasted_callback_result(
                &format!("  {redirect}?{query}\n"),
                redirect,
                "s",
                DYNAMIC_CLIENT
            )
            .unwrap(),
            ("secret-code".into(), "oaiapp_test".into())
        );
        for input in [
            format!("https://127.0.0.1:43123/auth/callback?{query}"),
            format!("http://localhost:43123/auth/callback?{query}"),
            format!("http://127.0.0.1:1455/auth/callback?{query}"),
            format!("http://127.0.0.1:43123/other?{query}"),
            format!("http://user@127.0.0.1:43123/auth/callback?{query}"),
            format!("{redirect}?{query}#fragment"),
            format!("{redirect}?{query}&state=s"),
            format!("{redirect}?code=secret-code&state=wrong&client_id=oaiapp_test"),
            format!("{redirect}?code=secret-code&state=s"),
            format!("{redirect}?error=secret-code&state=s"),
            format!("https://auth.openai.com/oauth/authorize?{query}"),
            "secret-code".into(),
            redirect.into(),
            "x".repeat(MAX_CALLBACK_LINE + 1),
        ] {
            let error = pasted_callback_result(&input, redirect, "s", DYNAMIC_CLIENT)
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret-code"));
        }
        assert!(
            pasted_callback_result(
                &format!("{redirect}?{query}"),
                redirect,
                "s",
                "oaiapp_other"
            )
            .is_err()
        );
        assert_eq!(
            pasted_callback_result(
                &format!("{redirect}?code=secret-code&state=s"),
                redirect,
                "s",
                "oaiapp_test"
            )
            .unwrap()
            .1,
            "oaiapp_test"
        );
    }

    #[test]
    fn first_authorization_registers_baybo_and_returning_authorization_reuses_client() {
        let url = Url::parse(&authorize_url(
            "http://127.0.0.1:1455/auth/callback",
            "challenge",
            "state",
            "nonce",
            DYNAMIC_CLIENT,
            "urn:uuid:host",
            false,
        ))
        .unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("client_id").unwrap(), DYNAMIC_CLIENT);
        assert_eq!(query.get("agent_name_hint").unwrap(), "Baybo");
        assert_eq!(query.get("ext_agent_host_id").unwrap(), "urn:uuid:host");
        assert_eq!(query.get("resource").unwrap(), RESOURCE);
        assert_eq!(query.get("scope").unwrap(), SCOPES);
        assert_eq!(query.get("nonce").unwrap(), "nonce");
        assert_eq!(query.get("code_challenge_method").unwrap(), "S256");
        let returning = authorize_url(
            "redirect",
            "challenge",
            "s",
            "n",
            "oaiapp_test",
            "host",
            false,
        );
        assert!(!returning.contains("agent_name_hint"));
        assert!(!returning.contains("prompt=consent"));
        assert!(
            authorize_url(
                "redirect",
                "challenge",
                "s",
                "n",
                "oaiapp_test",
                "host",
                true
            )
            .contains("prompt=consent")
        );
    }

    fn token_response(scope: Option<&str>) -> Tokens {
        Tokens {
            access_token: "opaque-access-token".into(),
            refresh_token: Some("refresh".into()),
            id_token: Some("id".into()),
            expires_in: 3600,
            scope: scope.map(str::to_owned),
            token_type: "Bearer".into(),
        }
    }

    #[test]
    fn opaque_access_tokens_use_server_expiry_and_actual_granted_scope() {
        let before = chrono::Utc::now().timestamp();
        let connected = bundle_from_tokens(
            token_response(Some("openid profile")),
            "oaiapp_test".into(),
            "user".into(),
            None,
        )
        .unwrap();
        assert!(!connected.sharing_enabled());
        assert!(connected.expires_at >= before + 3600);
        let enabled = bundle_from_tokens(
            token_response(Some(SHARING_SCOPE)),
            "oaiapp_test".into(),
            "user".into(),
            None,
        )
        .unwrap();
        assert!(enabled.sharing_enabled());
        let mut refreshed = token_response(None);
        refreshed.refresh_token = None;
        refreshed.id_token = None;
        let next = bundle_from_tokens(
            refreshed,
            "oaiapp_test".into(),
            "user".into(),
            Some(&enabled),
        )
        .unwrap();
        assert_eq!(next.refresh_token, enabled.refresh_token);
        assert_eq!(next.connection, enabled.connection);
        assert!(
            !bundle_from_tokens(
                token_response(Some("openid")),
                "oaiapp_test".into(),
                "user".into(),
                Some(&enabled)
            )
            .unwrap()
            .sharing_enabled()
        );
    }

    async fn endpoint(
        body: &str,
        status: &str,
    ) -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let len = socket.read(&mut chunk).await.unwrap();
                if len == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..len]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .and_then(|value| value.parse().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            tx.send(String::from_utf8(request).unwrap()).unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (origin, rx)
    }

    #[tokio::test]
    async fn refresh_sends_issued_client_and_preserves_registration_on_rotation() {
        let bundle = bundle_from_tokens(
            token_response(Some(SHARING_SCOPE)),
            "oaiapp_test".into(),
            "user".into(),
            None,
        )
        .unwrap();
        let body = json!({"access_token": "rotated-access", "refresh_token": "rotated-refresh", "expires_in": 3600, "token_type": "Bearer"}).to_string();
        let (issuer, request) = endpoint(&body, "200 OK").await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let next = refresh_at(&issuer, &bundle, &http).await.unwrap();
        assert_eq!(next.connection, bundle.connection);
        assert_eq!(next.refresh_token, "rotated-refresh");
        let request = request.await.unwrap();
        assert!(request.starts_with("POST /api/accounts/oauth/token "));
        let form = request.split("\r\n\r\n").nth(1).unwrap();
        let pairs: std::collections::HashMap<_, _> =
            url::form_urlencoded::parse(form.as_bytes()).collect();
        assert_eq!(pairs.get("client_id").unwrap(), "oaiapp_test");
        assert_eq!(pairs.get("resource").unwrap(), RESOURCE);
        assert!(!request.contains(legacy_oauth::LEGACY_CLIENT_ID));
        assert!(!request.to_lowercase().contains("originator:"));
    }

    #[tokio::test]
    async fn only_terminal_refresh_errors_clear_credentials() {
        let bundle = bundle_from_tokens(
            token_response(Some(SHARING_SCOPE)),
            "oaiapp_test".into(),
            "user".into(),
            None,
        )
        .unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for (code, permanent) in [
            ("invalid_grant", true),
            ("refresh_token_reused", true),
            ("invalid_client", false),
            ("temporarily_unavailable", false),
        ] {
            let (issuer, request) =
                endpoint(&json!({"error":code}).to_string(), "400 Bad Request").await;
            let result = refresh_at(&issuer, &bundle, &http).await;
            assert_eq!(
                matches!(result, Err(RefreshError::Permanent(_))),
                permanent,
                "{code}"
            );
            request.await.unwrap();
        }
    }
    #[test]
    fn identity_only_sign_in_does_not_require_inference_tokens_or_refresh() {
        let tokens: Tokens =
            serde_json::from_value(json!({"id_token":"id", "scope":"openid profile email"}))
                .unwrap();
        let bundle = bundle_from_tokens(tokens, "oaiapp_test".into(), "user".into(), None).unwrap();
        assert!(!bundle.sharing_enabled());
        assert!(!bundle.is_near_expiry(60));
        assert!(bundle.require_sharing().is_err());
    }
}
