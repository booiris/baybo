//! Stub-WS chat-TUI smoke probe. Built only with the `test-support`
//! feature (see the `required-features` gate in `Cargo.toml`) so it never
//! ships in a release build, and launched inside a tmux pane by
//! `crates/tui/tests/chat_render.rs` so the real inline-viewport chat UI
//! renders against a real terminal.
//!
//! It stands up an in-process stub gateway that speaks just enough of
//! `aura_channels::wire` to get the TUI talking — `RegisterAck`, an empty
//! `HistorySnapshot`, and a scripted assistant reply (streamed delta +
//! final message) for every user message — then connects a real
//! [`WsTransport`] to it and runs the real [`TuiAdapter`]. No gateway,
//! agent, or LLM is involved; the UI plumbing under test is exactly the
//! production path.
//!
//! The process exits when the TUI shuts down (Ctrl+C on an empty prompt)
//! or after [`SAFETY_TIMEOUT`], so an orphaned probe can never wedge a
//! tmux pane.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::MessageRole;
use aura_channels::wire::{self, Frame, Message};
use aura_model::{ChannelType, SessionId};
use aura_tui::TuiAdapter;
use aura_tui::client::WsTransport;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const SESSION: &str = "smoke-session";
/// Distinctive so the test can tell the assistant reply apart from the
/// echoed user line.
const REPLY_PREFIX: &str = "stub-reply for: ";
/// Backstop so a probe whose driver died never lingers.
const SAFETY_TIMEOUT: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(stub_gateway(listener));

    let session_id = SessionId::from(SESSION);
    let transport = Arc::new(WsTransport::connect(addr, String::new(), session_id).await?);

    let shutdown = Arc::new(Notify::new());
    let on_exit = Arc::clone(&shutdown);
    let tui = TuiAdapter::new(transport).with_on_exit(Arc::new(move || on_exit.notify_one()));
    tui.start().await?;

    tokio::select! {
        _ = shutdown.notified() => {}
        _ = tokio::time::sleep(SAFETY_TIMEOUT) => {}
    }
    Ok(())
}

/// Accept one TUI connection and answer its frames forever.
async fn stub_gateway(listener: TcpListener) {
    let Ok((stream, _)) = listener.accept().await else {
        return;
    };
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let session_id = SessionId::from(SESSION);
    let (mut sink, mut source) = ws.split();

    while let Some(Ok(msg)) = source.next().await {
        let WsMessage::Binary(bytes) = msg else {
            continue;
        };
        let Ok(frame) = wire::decode(&bytes) else {
            continue;
        };
        match frame {
            Frame::Register { .. } => {
                send(
                    &mut sink,
                    &Frame::RegisterAck {
                        ok: true,
                        reason: None,
                    },
                )
                .await;
            }
            Frame::Subscribe { .. } => {
                send(
                    &mut sink,
                    &Frame::HistorySnapshot {
                        session_id: session_id.clone(),
                        entries: Vec::new(),
                    },
                )
                .await;
            }
            Frame::Message(user_msg) => {
                let reply = format!("{REPLY_PREFIX}{}", user_msg.content);
                send(
                    &mut sink,
                    &Frame::AnswerDelta {
                        session_id: session_id.clone(),
                        user_id: String::new(),
                        text: reply.clone(),
                    },
                )
                .await;
                send(
                    &mut sink,
                    &Frame::Message(assistant_reply(&session_id, reply)),
                )
                .await;
            }
            _ => {}
        }
    }
}

fn assistant_reply(session_id: &SessionId, content: String) -> Message {
    Message {
        content,
        session_id: session_id.clone(),
        user_id: String::new(),
        channel_type: ChannelType::from("tui"),
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
        role: MessageRole::Assistant,
        ordinal: None,
    }
}

async fn send(sink: &mut SplitWsSink, frame: &Frame) {
    if let Ok(bytes) = wire::encode(frame) {
        let _ = sink.send(WsMessage::Binary(bytes)).await;
    }
}

type SplitWsSink = futures::stream::SplitSink<WebSocketStream<TcpStream>, WsMessage>;
