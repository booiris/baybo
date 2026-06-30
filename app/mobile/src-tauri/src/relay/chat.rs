//! The relay chat leg: dial the gateway over the relay's content-join leg, run the
//! Noise IK handshake, and exchange `Frame`s over the established E2E channel.
//!
//! The generic frame pump + session lifecycle live in [`crate::transport`]; this
//! file is just the relay-specific seams: [`RelaySessions::establish`] (dial +
//! Noise) and [`RelayCodec`] (seal/open). The crypto + frame codec themselves live
//! in the host-tested core ([`ContentHandshake`] / [`ContentSession`]).
//!
//! One session at a time: opening a new one aborts the previous pump. Content is
//! relay-only — the app reaches the (possibly NAT'd) gateway through C's blind
//! content-join leg.

use std::time::Duration;

use baybo_mobile_core::{
    ContentHandshake, ContentSession, Frame, WireAttachment, apns_token_frame, fetch_history_frame,
    subscribe_frame, user_message_frame,
};
use device_proto::noise::StaticKeypair;
use futures_util::SinkExt;
use tauri::AppHandle;
use tauri::ipc::Channel;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{Error as WsError, Message, http::StatusCode};

use super::pairing::{PairedRecord, load_paired_record};
use crate::transport::{
    ChatTransport, Connection, FrameCodec, HistoryFrameFn, SessionRegistry, TransportError,
    UserFrameFn, WsStream, recv_binary,
};

/// Retry budget for the content dial while the gateway's relay control link is
/// briefly absent. The gateway re-dials the relay on a fixed backoff after any
/// drop (5s `RECONNECT_BACKOFF` in the gateway's `relay_content`), so a phone that
/// opens chat inside that window would otherwise get a hard error; this budget
/// outlasts it. Only a `503 gateway not connected` is retried — a permanent
/// refusal (e.g. `401` for an unadmitted key) surfaces at once.
const CONTENT_DIAL_RETRIES: usize = 14;
const CONTENT_DIAL_RETRY_DELAY: Duration = Duration::from_millis(500);

/// The relay leg's Tauri-managed state: just the shared session registry. The
/// durable pairing record is reloaded from the keychain on each connect, so the
/// leg itself is otherwise stateless.
#[derive(Default)]
pub struct RelaySessions {
    registry: SessionRegistry,
}

/// The relay frame codec: every `Frame` rides Noise (sealed + chunked on send,
/// decrypted + reassembled on receipt).
struct RelayCodec {
    session: ContentSession,
}

impl FrameCodec for RelayCodec {
    fn encode_outbound(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, TransportError> {
        Ok(self.session.seal(frame)?)
    }

    fn decode_inbound(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TransportError> {
        // A decrypt failure means the Noise stream desynced — unrecoverable, so the
        // pump ends the session on this `Err`.
        Ok(self.session.open(bytes)?)
    }
}

impl ChatTransport for RelaySessions {
    fn establish(
        &self,
        session_id: &str,
        since_ordinal: Option<i64>,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send {
        let session_id = session_id.to_string();
        async move {
            // Preconditions surface as `Precondition` so a transient failure here
            // doesn't tear down a healthy live session on a foreground reconnect
            // (matches the original, which only reset on a failed dial below).
            let record = load_paired_record()
                .map_err(TransportError::Precondition)?
                .ok_or_else(|| {
                    TransportError::Precondition("not paired; pair a gateway first".into())
                })?;
            let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
            if record.relay_node_id.is_empty() {
                return Err(TransportError::Precondition(
                    "paired gateway has no relay route; re-pair".into(),
                ));
            }

            let established = dial_relay(&record.relay_node_id, &record, &local).await?;
            let codec: Box<dyn FrameCodec> = Box::new(RelayCodec {
                session: established.session,
            });

            // Self-pull: Subscribe so the gateway streams live agent output and
            // replays any thread rows above `since_ordinal` (the catch-up gap on a
            // reconnect; `None` = no catch-up).
            let opening = vec![subscribe_frame(&session_id, since_ordinal)];

            // Best-effort: tell the gateway our current APNs token so it keeps C's
            // push binding fresh across token rotation — skipped when iOS hasn't
            // issued a token yet, and non-fatal if it can't be sealed.
            let mut opening_best_effort = Vec::new();
            if let Some(token) = crate::push_register::apns_token() {
                let env = if cfg!(debug_assertions) {
                    "sandbox"
                } else {
                    "production"
                };
                opening_best_effort.push(apns_token_frame(&token, env));
            }

            // Relay user messages carry the device id + `channel_type=ios`.
            let device_id = record.device_id.clone();
            let sid = session_id.clone();
            let user_frame: UserFrameFn = Box::new(move |text, msg_id, attachments| {
                user_message_frame(&sid, &device_id, text, msg_id, attachments)
            });

            // History requests bind only the session id (identity-agnostic).
            let sid = session_id.clone();
            let history_frame: HistoryFrameFn = Box::new(move |before_ordinal, limit| {
                fetch_history_frame(&sid, before_ordinal, limit)
            });

            Ok(Connection {
                ws: established.ws,
                codec,
                opening,
                opening_best_effort,
                user_frame,
                history_frame,
            })
        }
    }
}

/// Open the relay content session for `session_id`, streaming decrypted gateway
/// frames to `on_frame`.
pub async fn connect(
    app: AppHandle,
    sessions: &RelaySessions,
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

/// Queue a user message (with any uploaded `attachments`) on the live session.
pub async fn send(
    sessions: &RelaySessions,
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

/// Queue a backward transcript-history request on the live session. The
/// `Frame::HistoryPage` reply streams back through `on_frame` (the webview's frame
/// switch consumes it) — this returns once the request is enqueued, not the page.
pub async fn fetch_history(
    sessions: &RelaySessions,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<(), String> {
    sessions
        .registry
        .fetch_history(before_ordinal, limit)
        .await
        .map_err(|e| e.to_string())
}

/// Tear down the live session (if any).
pub async fn disconnect(sessions: &RelaySessions) {
    sessions.registry.disconnect().await;
}

/// An established, handshaken relay leg ready to wrap as a [`Connection`].
struct Established {
    ws: WsStream,
    session: ContentSession,
}

/// Dial the blind relay's content-join leg for the gateway's `relay_node_id`. The
/// relay admits this leg by the instance key (symmetric with the gateway's host
/// leg); end-to-end, the gateway authenticates this device by matching the Noise
/// IK initiator's static against an approved device row.
///
/// Retries a `503 gateway not connected` (the gateway is offline or mid-reconnect
/// to the relay) for a bounded window; every other failure surfaces at once.
async fn dial_relay(
    node_id: &str,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, TransportError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    if record.relay_url.is_empty() {
        return Err(TransportError::Other(
            "no relay url for this pairing".into(),
        ));
    }
    let base = record.relay_url.trim_end_matches('/');
    let url = remote_host_protocol::relay::content_join_url(base, node_id);

    let mut attempt = 0usize;
    let ws = loop {
        // Rebuilt per attempt (`into_client_request` yields an owned request).
        // Present the admission key the QR carried at pairing — the relay admits
        // the phone leg too.
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| TransportError::Other(format!("bad relay url {base}: {e}")))?;
        if !record.remote_api_key.is_empty() {
            let value = record
                .remote_api_key
                .parse()
                .map_err(|e| TransportError::Other(format!("bad instance key header: {e}")))?;
            req.headers_mut()
                .insert(remote_host_protocol::relay::REMOTE_API_KEY_HEADER, value);
        }
        match connect_async(req).await {
            Ok((ws, _)) => break ws,
            // The relay's `content_join` returns 503 while no gateway holds a live
            // control connection for this node; it re-dials the relay on a fixed
            // backoff, so retry briefly rather than failing the open.
            Err(WsError::Http(resp))
                if resp.status() == StatusCode::SERVICE_UNAVAILABLE
                    && attempt < CONTENT_DIAL_RETRIES =>
            {
                attempt += 1;
                tokio::time::sleep(CONTENT_DIAL_RETRY_DELAY).await;
            }
            Err(WsError::Http(resp)) if resp.status() == StatusCode::SERVICE_UNAVAILABLE => {
                return Err(TransportError::Other(format!(
                    "gateway offline: {base} has no relay control connection; ensure the paired gateway is running with relay enabled"
                )));
            }
            Err(e) => return Err(TransportError::Other(format!("relay connect {base}: {e}"))),
        }
    };
    handshake_over(ws, record, local).await
}

/// Run the Noise IK initiator handshake over an established relay WS and return
/// the ready content session.
async fn handshake_over(
    mut ws: WsStream,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, TransportError> {
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| TransportError::Other(format!("start handshake: {e}")))?;
    ws.send(Message::Binary(msg1))
        .await
        .map_err(|e| TransportError::Other(format!("send handshake: {e}")))?;
    let msg2 = recv_binary(&mut ws).await?;
    let session = handshake
        .finish(&msg2)
        .map_err(|e| TransportError::Other(format!("finish handshake: {e}")))?;
    Ok(Established { ws, session })
}
