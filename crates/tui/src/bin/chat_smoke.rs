//! Stub-WS chat-TUI smoke probe. Built only with the `test-support`
//! feature (see the `required-features` gate in `Cargo.toml`) so it never
//! ships in a release build, and launched inside a tmux pane by
//! `crates/tui/tests/chat_render.rs` so the real inline-viewport chat UI
//! renders against a real terminal.
//!
//! It stands up an in-process stub gateway that speaks just enough of
//! `baybo_channels::wire` to get the TUI talking — `RegisterAck`, an empty
//! `HistorySnapshot`, then a scripted response per user message — and
//! connects a real [`WsTransport`] to it driving the real [`TuiAdapter`].
//! No gateway, agent, or LLM is involved; the UI plumbing under test is
//! exactly the production path.
//!
//! The stub dispatches on the typed message (see
//! [`baybo_tui::smoke_contract`]): `tool`, `subagent`, `approval`, `task`,
//! or — for anything else — a plain streamed echo. This lets the render
//! test exercise tool-call lines, the approval modal, and the
//! TaskList-is-dropped contract from a real terminal.
//!
//! The process exits when the TUI shuts down (Ctrl+C on an empty prompt)
//! or after [`SAFETY_TIMEOUT`], so an orphaned probe can never wedge a
//! tmux pane.

use std::sync::Arc;
use std::time::Duration;

use baybo_channels::MessageRole;
use baybo_channels::wire::{self, Frame, Message};
use baybo_model::ResourceAccess;
use baybo_model::{ChannelType, SessionId};
use baybo_tui::TuiAdapter;
use baybo_tui::client::WsTransport;
use baybo_tui::smoke_contract::*;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const SESSION: &str = "smoke-session";
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
                send(&mut sink, &register_ack()).await;
            }
            Frame::Subscribe { .. } => {
                send(&mut sink, &history_snapshot(&session_id)).await;
            }
            Frame::Message(user_msg) => {
                dispatch(&mut sink, &session_id, user_msg.content.trim()).await;
            }
            // The TUI echoes this back when the user resolves the approval
            // modal; close the loop with a follow-up reply.
            Frame::ResolveApproval { .. } => {
                send(&mut sink, &reply(&session_id, APPROVAL_REPLY)).await;
            }
            _ => {}
        }
    }
}

/// Route a typed message to its scripted scenario.
async fn dispatch(sink: &mut SplitWsSink, sid: &SessionId, text: &str) {
    match text {
        SAY_TOOL => {
            tool_call(sink, sid, "c-tool", TOOL_NAME, TOOL_LABEL, TOOL_SUMMARY).await;
            send(sink, &reply(sid, TOOL_REPLY)).await;
        }
        SAY_SUBAGENT => {
            tool_call(
                sink,
                sid,
                "c-sub",
                SUBAGENT_TOOL,
                SUBAGENT_LABEL,
                SUBAGENT_SUMMARY,
            )
            .await;
            send(sink, &reply(sid, SUBAGENT_REPLY)).await;
        }
        SAY_APPROVAL => {
            send(sink, &approval_request(sid)).await;
            // The follow-up reply is sent when the TUI returns ResolveApproval.
        }
        SAY_TASK => {
            // The TUI drops TaskList (web-dashboard-only), so the subject
            // must never render; only the trailing reply should.
            send(sink, &task_list(sid)).await;
            send(sink, &reply(sid, TASK_REPLY)).await;
        }
        other => {
            let echo = format!("{REPLY_PREFIX}{other}");
            send(
                sink,
                &Frame::AnswerDelta {
                    session_id: sid.clone(),
                    user_id: String::new(),
                    text: echo.clone(),
                },
            )
            .await;
            send(sink, &reply(sid, &echo)).await;
        }
    }
}

async fn tool_call(
    sink: &mut SplitWsSink,
    sid: &SessionId,
    call_id: &str,
    tool: &str,
    label: &str,
    summary: &str,
) {
    send(
        sink,
        &Frame::ToolStarted {
            session_id: sid.clone(),
            user_id: String::new(),
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            label: Some(label.to_string()),
        },
    )
    .await;
    send(
        sink,
        &Frame::ToolCompleted {
            session_id: sid.clone(),
            user_id: String::new(),
            call_id: call_id.to_string(),
            status: "ok".to_string(),
            summary: summary.to_string(),
            approval: None,
        },
    )
    .await;
}

fn approval_request(sid: &SessionId) -> Frame {
    Frame::ApprovalRequested {
        call_id: "c-appr".to_string(),
        tool_call_id: None,
        session_id: sid.clone(),
        user_id: String::new(),
        tool: APPROVAL_TOOL.to_string(),
        accesses: vec![ResourceAccess::ExecCommand {
            command: APPROVAL_COMMAND.to_string(),
        }],
        params_preview: APPROVAL_COMMAND.to_string(),
        description: Some(APPROVAL_DESC.to_string()),
    }
}

fn task_list(sid: &SessionId) -> Frame {
    Frame::TaskList {
        session_id: sid.clone(),
        user_id: String::new(),
        tasks: vec![baybo_channels::wire::TaskView {
            id: "t1".to_string(),
            subject: TASK_SUBJECT.to_string(),
            status: "in_progress".to_string(),
            depends_on: Vec::new(),
        }],
    }
}

fn register_ack() -> Frame {
    Frame::RegisterAck {
        ok: true,
        reason: None,
    }
}

fn history_snapshot(sid: &SessionId) -> Frame {
    Frame::HistorySnapshot {
        session_id: sid.clone(),
        entries: Vec::new(),
    }
}

fn reply(sid: &SessionId, content: &str) -> Frame {
    Frame::Message(Message {
        content: content.to_string(),
        session_id: sid.clone(),
        user_id: String::new(),
        channel_type: ChannelType::from("tui"),
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
        role: MessageRole::Assistant,
        ordinal: None,
    })
}

async fn send(sink: &mut SplitWsSink, frame: &Frame) {
    if let Ok(bytes) = wire::encode(frame) {
        let _ = sink.send(WsMessage::Binary(bytes)).await;
    }
}

type SplitWsSink = futures::stream::SplitSink<WebSocketStream<TcpStream>, WsMessage>;
