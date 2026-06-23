//! Inline OAuth 2.1 + PKCE + Dynamic Client Registration for `baybo mcp add`.
//!
//! The flow:
//!  1. Discover authorization-server metadata from `<base_url>/.well-known/...`
//!     (rmcp's `OAuthState` handles the discovery + the resource-server
//!     fallback the MCP spec defines).
//!  2. If no `client_id` was provided, register a public client via DCR.
//!  3. Generate PKCE + the authorization URL, open the user's browser.
//!  4. Spin up a localhost callback listener on `redirect_port` (or an
//!     ephemeral port if `None`), wait for `?code=&state=`.
//!  5. Exchange the code for tokens.
//!  6. Hand the access + refresh tokens back to the caller, who persists
//!     them to the vault.
//!
//! The flow runs inside the CLI process for the duration of a single
//! `baybo mcp add` invocation and tears the listener down on exit.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use baybo_security::SecretVault;
use oauth2::TokenResponse;
use rmcp::transport::auth::{
    AuthorizationManager, CredentialStore, OAuthClientConfig, OAuthState, OAuthTokenResponse,
    StoredCredentials,
};
use serde::Deserialize;
use tokio::sync::{Mutex, oneshot};
use tracing::{info, warn};

use crate::mcp::credentials::VaultCredentialStore;
use crate::mcp::error::{McpError, McpResult};

const REDIRECT_PATH: &str = "/oauth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
const CLIENT_NAME: &str = "Baybo MCP Client";

const CALLBACK_HTML: &str = "<!doctype html><html><body style=\"font-family:sans-serif;text-align:center;padding-top:6rem\"><h1>Authorization complete</h1><p>You can close this tab and return to the terminal.</p></body></html>";

#[derive(Debug)]
pub struct OAuthBundle {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub access_token_expires_at: Option<SystemTime>,
}

/// Run the inline OAuth flow against `server_url`. Returns the obtained
/// tokens; the caller is responsible for persisting them to the vault.
///
/// `client_id_hint` is the value of `--client-id` if the operator passed
/// one. When `None`, the flow attempts Dynamic Client Registration
/// against the discovered authorization server. Many providers (GitHub,
/// Google, …) do **not** implement DCR and require the operator to
/// pre-register an OAuth app — for those, this function returns a clear
/// error pointing the operator at `--client-id`.
pub async fn run_authorization_flow(
    server_url: &str,
    redirect_port: Option<u16>,
    client_id_hint: Option<&str>,
    client_secret: Option<&str>,
    vault: &Arc<SecretVault>,
    server_name: &str,
) -> McpResult<OAuthBundle> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        redirect_port.unwrap_or(0),
    )))
    .await
    .map_err(|e| McpError::OAuth(format!("bind localhost callback: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| McpError::OAuth(format!("local_addr: {e}")))?;
    let redirect_uri = format!("http://127.0.0.1:{}{REDIRECT_PATH}", bound.port());

    let (callback_tx, callback_rx) = oneshot::channel::<CallbackQuery>();
    let app_state = AppState {
        sender: std::sync::Arc::new(Mutex::new(Some(callback_tx))),
    };

    let app = Router::new()
        .route(REDIRECT_PATH, get(callback_handler))
        .with_state(app_state);

    let server_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!(error = %e, "oauth callback server stopped unexpectedly");
        }
    });

    // Two paths: with a pre-registered client we drive `AuthorizationManager`
    // directly to skip DCR (which providers like GitHub do not implement);
    // without one we fall back to `OAuthState`, which attempts DCR for us.
    let result = match client_id_hint {
        Some(client_id) => {
            run_with_preregistered_client(
                server_url,
                &redirect_uri,
                client_id,
                client_secret,
                callback_rx,
            )
            .await
        }
        None => run_with_oauth_state(server_url, &redirect_uri, callback_rx).await,
    };

    server_handle.abort();

    let (client_id, token_response) = result?;
    persist_credentials(vault, server_name, &client_id, &token_response).await?;
    Ok(bundle_from(client_id, token_response))
}

async fn persist_credentials(
    vault: &Arc<SecretVault>,
    server_name: &str,
    client_id: &str,
    token: &OAuthTokenResponse,
) -> McpResult<()> {
    let granted_scopes: Vec<String> = token
        .scopes()
        .map(|s| s.iter().map(|sc| sc.to_string()).collect())
        .unwrap_or_default();
    let token_received_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs());
    let stored = StoredCredentials::new(
        client_id.to_string(),
        Some(token.clone()),
        granted_scopes,
        token_received_at,
    );
    let store = VaultCredentialStore::new(Arc::clone(vault), server_name);
    store
        .save(stored)
        .await
        .map_err(|e| McpError::OAuth(format!("persist credentials to vault: {e}")))?;
    Ok(())
}

async fn run_with_oauth_state(
    server_url: &str,
    redirect_uri: &str,
    callback_rx: oneshot::Receiver<CallbackQuery>,
) -> McpResult<(String, OAuthTokenResponse)> {
    let mut oauth_state = OAuthState::new(server_url, None)
        .await
        .map_err(|e| McpError::OAuth(format!("oauth metadata discovery: {e}")))?;
    oauth_state
        .start_authorization(&[], redirect_uri, Some(CLIENT_NAME))
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.to_lowercase().contains("dynamic")
                && (msg.to_lowercase().contains("not supported")
                    || msg.to_lowercase().contains("registration failed"))
            {
                McpError::OAuth(
                    "the authorization server does not support Dynamic Client Registration. \
                     Register an OAuth app with the provider and re-run with `--client-id <id>` \
                     (and `--client-secret` if the app is confidential)."
                        .into(),
                )
            } else {
                McpError::OAuth(format!("start authorization: {msg}"))
            }
        })?;

    let auth_url = oauth_state
        .get_authorization_url()
        .await
        .map_err(|e| McpError::OAuth(format!("get authorization url: {e}")))?;

    let (code, state) = open_browser_and_wait(&auth_url, callback_rx).await?;

    oauth_state
        .handle_callback(&code, &state)
        .await
        .map_err(|e| McpError::OAuth(format!("token exchange: {e}")))?;

    let (client_id, token_response) = oauth_state
        .get_credentials()
        .await
        .map_err(|e| McpError::OAuth(format!("read credentials after auth: {e}")))?;
    let token_response = token_response.ok_or_else(|| {
        McpError::OAuth("authorization completed but no token response was returned".into())
    })?;

    Ok((client_id.to_string(), token_response))
}

async fn run_with_preregistered_client(
    server_url: &str,
    redirect_uri: &str,
    client_id: &str,
    client_secret: Option<&str>,
    callback_rx: oneshot::Receiver<CallbackQuery>,
) -> McpResult<(String, OAuthTokenResponse)> {
    let mut manager = AuthorizationManager::new(server_url)
        .await
        .map_err(|e| McpError::OAuth(format!("oauth metadata discovery: {e}")))?;
    let metadata = manager
        .discover_metadata()
        .await
        .map_err(|e| McpError::OAuth(format!("oauth metadata discovery: {e}")))?;
    manager.set_metadata(metadata);

    let mut config = OAuthClientConfig::new(client_id, redirect_uri);
    if let Some(secret) = client_secret {
        config = config.with_client_secret(secret);
    }
    manager
        .configure_client(config)
        .map_err(|e| McpError::OAuth(format!("configure pre-registered client: {e}")))?;

    let auth_url = manager
        .get_authorization_url(&[])
        .await
        .map_err(|e| McpError::OAuth(format!("build authorization url: {e}")))?;

    let (code, state) = open_browser_and_wait(&auth_url, callback_rx).await?;

    let token_response = manager
        .exchange_code_for_token(&code, &state)
        .await
        .map_err(|e| McpError::OAuth(format!("token exchange: {e}")))?;

    Ok((client_id.to_string(), token_response))
}

async fn open_browser_and_wait(
    auth_url: &str,
    callback_rx: oneshot::Receiver<CallbackQuery>,
) -> McpResult<(String, String)> {
    if let Err(e) = open::that(auth_url) {
        info!(
            url = %auth_url,
            error = %e,
            "failed to open browser; copy the URL above and open it manually"
        );
        eprintln!("Open this URL in your browser to authorize:\n  {auth_url}");
    }

    let CallbackQuery { code, state, error } =
        match tokio::time::timeout(CALLBACK_TIMEOUT, callback_rx).await {
            Ok(Ok(q)) => q,
            Ok(Err(_)) => {
                return Err(McpError::OAuth(
                    "OAuth callback channel closed before receiving code".into(),
                ));
            }
            Err(_) => {
                return Err(McpError::OAuth(format!(
                    "OAuth callback timed out after {} seconds",
                    CALLBACK_TIMEOUT.as_secs()
                )));
            }
        };

    if let Some(err) = error {
        return Err(McpError::OAuth(format!(
            "authorization server returned error: {err}"
        )));
    }
    let code = code.ok_or_else(|| McpError::OAuth("authorization callback missing code".into()))?;
    let state =
        state.ok_or_else(|| McpError::OAuth("authorization callback missing state".into()))?;
    Ok((code, state))
}

fn bundle_from(client_id: String, token_response: OAuthTokenResponse) -> OAuthBundle {
    let access_token = token_response.access_token().secret().to_string();
    let refresh_token = token_response
        .refresh_token()
        .map(|t| t.secret().to_string());
    let access_token_expires_at = token_response.expires_in().and_then(|d| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|now| UNIX_EPOCH + now + d)
    });

    OAuthBundle {
        client_id,
        access_token,
        refresh_token,
        access_token_expires_at,
    }
}

#[derive(Clone)]
struct AppState {
    sender: std::sync::Arc<Mutex<Option<oneshot::Sender<CallbackQuery>>>>,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback_handler(
    Query(params): Query<CallbackQuery>,
    State(state): State<AppState>,
) -> Html<&'static str> {
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(params);
    }
    Html(CALLBACK_HTML)
}
