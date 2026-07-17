//! The direct chat leg: a raw-MessagePack `/v1/channel-ws` socket — the same
//! protocol `app/web` speaks, with **no Noise and no chunking** (every WS message
//! is exactly one `wire::encode(&Frame)`).
//!
//! The generic frame pump + session lifecycle live in [`crate::transport`]; this
//! file is just the direct-specific seams: [`DirectSessions::establish`] (dial
//! with the stored gateway Bearer plus device id header, run the device
//! `Register`/`RegisterAck` handshake) and [`DirectCodec`] (encode/decode). The
//! admin listener validates the Bearer + device id header on the HTTP upgrade,
//! then marks the connection as `AuthedClient::Device`.

use futures_util::SinkExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use crate::core::{Frame, MobileError, decode, encode, register_device_frame, user_message_frame};
use crate::transport::{
    ChatTransport, Connection, FrameCodec, SessionLeg, TransportError, UserFrameFn, WsStream,
    recv_binary,
};

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

impl ChatTransport for super::DirectSessions {
    #[allow(clippy::manual_async_fn)]
    fn establish(
        &self,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send {
        async move {
            // A precondition failure must not tear down a healthy live session on a
            // foreground reconnect (matches the original, which only reset on a
            // failed dial in `establish_ws`).
            let http = self.http_client().map_err(TransportError::Precondition)?;
            let ws = self.establish_ws(&http).await?;

            let codec: Box<dyn FrameCodec> = Box::new(DirectCodec);

            // Direct user messages carry the device id + `channel_type=owner`,
            // matching the relay content leg while still using the admin-bearer
            // direct transport. Web and mobile share the one `owner` channel;
            // which of them is talking is an auth concern, not a channel one.
            let device_id = http.device_id().to_owned();
            let user_frame: UserFrameFn = Box::new(move |session_id, text, msg_id, attachments| {
                user_message_frame(session_id, &device_id, text, msg_id, attachments)
            });

            Ok(Connection {
                ws,
                codec,
                user_frame,
            })
        }
    }
}

impl super::DirectSessions {
    /// Complete the WS handshake as the admin-authenticated direct device.
    async fn establish_ws(&self, http: &super::DirectHttp) -> Result<WsStream, TransportError> {
        match dial_and_register(http).await {
            Ok(ws) => Ok(ws),
            Err(DialErr::Unauthorized) => {
                Err(TransportError::Other(super::INVALID_TOKEN_CODE.to_string()))
            }
            Err(DialErr::Other(s)) => Err(TransportError::Other(s)),
        }
    }
}

impl SessionLeg for super::DirectSessions {
    fn registry(&self) -> &crate::transport::SessionRegistry {
        self.chat_registry()
    }
}

/// Like [`transport::disconnect`](crate::transport::disconnect), used by logout so
/// a later reconnect can't resurrect the now keychain-creds-less direct session.
pub(crate) async fn forget(sessions: &super::DirectSessions) {
    sessions.chat_registry().disconnect().await;
    sessions.clear_http_client();
}

/// Distinguishes rejected admin auth from a hard failure.
enum DialErr {
    Unauthorized,
    Other(String),
}

/// Dial `/v1/channel-ws` with the gateway Bearer, then run the device Register /
/// RegisterAck handshake.
async fn dial_and_register(http: &super::DirectHttp) -> Result<WsStream, DialErr> {
    let url = super::channel_ws_url(http.base_url()).map_err(DialErr::Other)?;
    let mut req = url
        .as_str()
        .into_client_request()
        .map_err(|e| DialErr::Other(format!("bad ws url: {e}")))?;
    for (name, value) in http.headers() {
        req.headers_mut().insert(name.clone(), value.clone());
    }

    let mut ws = match connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(WsError::Http(resp)) if resp.status() == StatusCode::UNAUTHORIZED => {
            return Err(DialErr::Unauthorized);
        }
        Err(e) => return Err(DialErr::Other(format!("ws connect: {e}"))),
    };

    let register = encode(&register_device_frame())
        .map_err(|e| DialErr::Other(format!("encode register: {e}")))?;
    ws.send(Message::Binary(register))
        .await
        .map_err(|e| DialErr::Other(format!("send register: {e}")))?;

    match recv_frame(&mut ws).await.map_err(DialErr::Other)? {
        Frame::RegisterAck { ok: true, .. } => Ok(ws),
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
