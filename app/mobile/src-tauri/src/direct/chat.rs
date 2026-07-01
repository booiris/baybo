//! The direct chat leg: a raw-MessagePack `/v1/channel-ws` socket — the same
//! protocol `app/web` speaks, with **no Noise and no chunking** (every WS message
//! is exactly one `wire::encode(&Frame)`).
//!
//! The generic frame pump + session lifecycle live in [`crate::transport`]; this
//! file is just the direct-specific seams: [`DirectSessions::establish`] (mint /
//! rotate the channel token, dial, run the web `Register`/`RegisterAck` handshake)
//! and [`DirectCodec`] (encode/decode). Auth is a minted **channel token** (header
//! `x-baybo-channel-token`), not the admin Bearer; on a 401 upgrade the token is
//! rotated once over REST and the dial retried. A `RegisterAck{ok:false}` is not a
//! token problem (the token is checked at the upgrade) — its reason is surfaced.

use baybo_mobile_core::{
    Frame, MobileError, WireAttachment, decode, encode, fetch_history_frame, register_http_frame,
    subscribe_frame, web_user_message_frame,
};
use futures_util::SinkExt;
use tauri::AppHandle;
use tauri::ipc::Channel;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::rest;
use crate::transport::{
    ChatTransport, Connection, FrameCodec, HistoryFrameFn, SessionRegistry, TransportError,
    UserFrameFn, WsStream, recv_binary,
};

/// The direct leg's Tauri-managed state: the shared session registry plus the
/// current session id + channel token, stashed so a reconnect or a blob op reuses
/// it (and outlives the pump). The token is the durable bit the relay leg has no
/// analog for, so it lives here on the transport, not in the generic registry.
#[derive(Default)]
pub struct DirectSessions {
    registry: SessionRegistry,
    session: Mutex<Option<DirectSessionInfo>>,
}

#[derive(Clone)]
struct DirectSessionInfo {
    session_id: String,
    channel_token: String,
}

/// The gateway-minted session id returned to the webview by `direct_session_create`.
#[derive(serde::Serialize)]
pub struct DirectSessionRef {
    pub session_id: String,
}

/// The direct frame codec: raw MessagePack, one frame per WS message — no Noise,
/// no reassembly.
struct DirectCodec;

impl FrameCodec for DirectCodec {
    fn encode_outbound(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, TransportError> {
        Ok(vec![encode(frame).map_err(MobileError::from)?])
    }

    fn decode_inbound(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TransportError> {
        // A frame variant this client's `wire` version doesn't know decodes to an
        // error; skip it (Ok(empty)) rather than end the session — forward-compat
        // with a newer gateway.
        Ok(decode(bytes).ok().into_iter().collect())
    }
}

impl ChatTransport for DirectSessions {
    fn establish(
        &self,
        session_id: &str,
        since_ordinal: Option<i64>,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send {
        let session_id = session_id.to_string();
        async move {
            // A precondition failure must not tear down a healthy live session on a
            // foreground reconnect (matches the original, which only reset on a
            // failed dial in `establish_ws`).
            let creds = super::credentials()
                .map_err(TransportError::Precondition)?
                .ok_or_else(|| {
                    TransportError::Precondition("not connected; sign in first".into())
                })?;
            let ws = self.establish_ws(&creds, &session_id).await?;

            let codec: Box<dyn FrameCodec> = Box::new(DirectCodec);
            let opening = vec![subscribe_frame(&session_id, since_ordinal)];

            // Direct user messages register as a web client (`web-operator` +
            // `channel_type=http`).
            let sid = session_id.clone();
            let user_frame: UserFrameFn = Box::new(move |text, msg_id, attachments| {
                web_user_message_frame(&sid, text, msg_id, attachments)
            });

            // History requests bind only the session id (identity-agnostic). The
            // direct path normally recovers via REST (`direct_history`), but the
            // same `/v1/channel-ws` leg also answers `FetchHistory`, so the command
            // stays transport-agnostic.
            let sid = session_id.clone();
            let history_frame: HistoryFrameFn = Box::new(move |before_ordinal, limit| {
                fetch_history_frame(&sid, before_ordinal, limit)
            });

            Ok(Connection {
                ws,
                codec,
                opening,
                opening_best_effort: Vec::new(),
                user_frame,
                history_frame,
            })
        }
    }
}

impl DirectSessions {
    /// Resolve a channel token for `session_id` and complete the WS handshake. On a
    /// rejected token (401 upgrade) it rotates once and retries; any other failure
    /// surfaces.
    async fn establish_ws(
        &self,
        creds: &super::DirectCredentials,
        session_id: &str,
    ) -> Result<WsStream, TransportError> {
        let token = self
            .channel_token_for(&creds.base_url, &creds.token, session_id)
            .await?;
        match dial_and_register(&creds.base_url, &token).await {
            Ok(ws) => Ok(ws),
            Err(DialErr::TokenDead) => {
                let cred = rest::rotate_token(&creds.base_url, &creds.token, session_id)
                    .await
                    .map_err(TransportError::Other)?;
                self.stash_token(session_id, &cred.channel_token).await;
                dial_and_register(&creds.base_url, &cred.channel_token)
                    .await
                    .map_err(|e| match e {
                        DialErr::TokenDead => {
                            TransportError::Other("channel token rejected".to_string())
                        }
                        DialErr::Other(s) => TransportError::Other(s),
                    })
            }
            Err(DialErr::Other(s)) => Err(TransportError::Other(s)),
        }
    }

    /// The channel token for `session_id`: reuse the stashed one if it matches,
    /// otherwise mint a fresh one (the relaunch case — only the id survived).
    async fn channel_token_for(
        &self,
        base: &str,
        admin_token: &str,
        session_id: &str,
    ) -> Result<String, TransportError> {
        {
            let st = self.session.lock().await;
            if let Some(s) = &*st
                && s.session_id == session_id
            {
                return Ok(s.channel_token.clone());
            }
        }
        let cred = rest::rotate_token(base, admin_token, session_id)
            .await
            .map_err(TransportError::Other)?;
        self.stash_token(session_id, &cred.channel_token).await;
        Ok(cred.channel_token)
    }

    async fn stash_token(&self, session_id: &str, token: &str) {
        *self.session.lock().await = Some(DirectSessionInfo {
            session_id: session_id.to_string(),
            channel_token: token.to_string(),
        });
    }
}

/// Mint a fresh chat session (gateway-assigned id + channel token) and stash it.
pub async fn session_create(sessions: &DirectSessions) -> Result<DirectSessionRef, String> {
    let creds = super::credentials()?.ok_or("not connected; sign in first")?;
    let cred = rest::mint_session(&creds.base_url, &creds.token).await?;
    *sessions.session.lock().await = Some(DirectSessionInfo {
        session_id: cred.session_id.clone(),
        channel_token: cred.channel_token,
    });
    Ok(DirectSessionRef {
        session_id: cred.session_id,
    })
}

/// Open the direct (raw-MessagePack) `/v1/channel-ws` leg for `session_id` and
/// stream frames to `on_frame`.
pub async fn connect(
    app: AppHandle,
    sessions: &DirectSessions,
    session_id: String,
    since_ordinal: Option<i64>,
    on_frame: Channel<Frame>,
) -> Result<(), String> {
    sessions
        .registry
        .connect(sessions, app, &session_id, since_ordinal, on_frame)
        .await
        .map_err(|e| e.to_string())
}

/// Queue a user message on the live direct session.
pub async fn send(
    sessions: &DirectSessions,
    text: String,
    msg_id: String,
    attachments: Vec<WireAttachment>,
) -> Result<(), String> {
    sessions
        .registry
        .send(text, msg_id, attachments)
        .await
        .map_err(|e| e.to_string())
}

/// Queue a backward transcript-history request on the live direct session. The
/// `Frame::HistoryPage` reply streams back through `on_frame`. (The direct path
/// usually uses the REST [`super::history`] instead; this keeps the shared
/// `chat_fetch_history` command transport-agnostic.)
pub async fn fetch_history(
    sessions: &DirectSessions,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<(), String> {
    sessions
        .registry
        .fetch_history(before_ordinal, limit)
        .await
        .map_err(|e| e.to_string())
}

/// Tear down the live pump (the stashed session id + token survive for reconnect).
pub async fn disconnect(sessions: &DirectSessions) {
    sessions.registry.disconnect().await;
}

/// Like [`disconnect`] but also drops the stashed session id + channel token. The
/// logout path uses this so a later reconnect can't resurrect the (now
/// keychain-creds-less) session, and the broad channel token doesn't linger in
/// memory after the user disconnects.
pub async fn forget(sessions: &DirectSessions) {
    sessions.registry.disconnect().await;
    *sessions.session.lock().await = None;
}

/// `(base_url, channel_token)` for the blob legs — base from the keychain creds,
/// token from the live session.
pub(super) async fn channel_context(sessions: &DirectSessions) -> Result<(String, String), String> {
    let base = super::credentials()?
        .ok_or("not connected; sign in first")?
        .base_url;
    let token = sessions
        .session
        .lock()
        .await
        .as_ref()
        .map(|s| s.channel_token.clone())
        .ok_or("no active direct session")?;
    Ok((base, token))
}

/// Distinguishes a rejected channel token (rotate + retry) from a hard failure.
enum DialErr {
    TokenDead,
    Other(String),
}

/// Dial `/v1/channel-ws` with the channel token, then run the web Register /
/// RegisterAck handshake. Returns the ready socket, or [`DialErr::TokenDead`] on a
/// 401 upgrade or `RegisterAck{ok:false}`.
async fn dial_and_register(base_url: &str, channel_token: &str) -> Result<WsStream, DialErr> {
    let url = super::channel_ws_url(base_url).map_err(DialErr::Other)?;
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| DialErr::Other(format!("bad ws url: {e}")))?;
    let value = HeaderValue::from_str(channel_token)
        .map_err(|e| DialErr::Other(format!("bad channel token: {e}")))?;
    req.headers_mut()
        .insert(HeaderName::from_static(super::CHANNEL_TOKEN_HEADER), value);

    let mut ws = match connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(WsError::Http(resp)) if resp.status() == StatusCode::UNAUTHORIZED => {
            return Err(DialErr::TokenDead);
        }
        Err(e) => return Err(DialErr::Other(format!("ws connect: {e}"))),
    };

    let register = encode(&register_http_frame())
        .map_err(|e| DialErr::Other(format!("encode register: {e}")))?;
    ws.send(Message::Binary(register))
        .await
        .map_err(|e| DialErr::Other(format!("send register: {e}")))?;

    match recv_frame(&mut ws).await.map_err(DialErr::Other)? {
        Frame::RegisterAck { ok: true, .. } => Ok(ws),
        // A rejected Register is NOT a token problem — the channel token is checked
        // at the HTTP upgrade (surfaced as the 401 above). Surface the server's
        // reason instead of pointlessly rotating the working token.
        Frame::RegisterAck { ok: false, reason } => Err(DialErr::Other(
            reason.unwrap_or_else(|| "register rejected".into()),
        )),
        _ => Err(DialErr::Other("unexpected handshake reply".into())),
    }
}

/// Read the next binary WS message and decode it as a `Frame` (skipping ping/pong)
/// — used for the `RegisterAck` reply before the pump takes over.
async fn recv_frame(ws: &mut WsStream) -> Result<Frame, String> {
    let bytes = recv_binary(ws).await.map_err(|e| e.to_string())?;
    decode(&bytes).map_err(|e| format!("decode frame: {e}"))
}
