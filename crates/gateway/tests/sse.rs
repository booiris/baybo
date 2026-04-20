//! `HttpAdapter` SSE fan-out — crate-level coverage.
//!
//! The adapter owns the bridge between the router (which emits
//! `AgentOutput::{Delta, Message, Notice}` through the `ChannelAdapter`
//! trait) and the per-session `broadcast::Sender<SseEvent>` that SSE
//! handlers subscribe to. These tests exercise the trait implementation
//! against a live `tokio::broadcast` so we catch ordering, missing-
//! subscriber, and shutdown semantics that the individual helper unit
//! tests can't.

use std::sync::Arc;

use aura_channels::{
    AgentOutput, ChannelAdapter, ChannelError, IncomingMessage, Message, NoticeLevel,
    OutgoingMessage, SseEvent,
};
use aura_gateway::HttpAdapter;
use aura_model::{ChannelType, ContentBlock, MessageMetadata, User};
use chrono::Utc;
use tokio::sync::mpsc;

fn sample_user() -> User {
    User {
        id: "u-1".into(),
        name: Some("tester".into()),
        channel: ChannelType::Http,
    }
}

fn sample_incoming(session_id: &str) -> IncomingMessage {
    IncomingMessage {
        message: Message {
            id: "m-1".into(),
            session_id: session_id.into(),
            channel: ChannelType::Http,
            sender: sample_user(),
            content: vec![ContentBlock::Text("hi".into())],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
    }
}

#[tokio::test]
async fn stream_delta_and_response_fan_out_in_order() {
    let adapter = Arc::new(HttpAdapter::new());
    let mut rx = adapter.subscribe("sess-1").await;

    adapter
        .send(AgentOutput::Delta {
            session_id: "sess-1".into(),
            channel: ChannelType::Http,
            text: "hello ".into(),
        })
        .await
        .expect("delta");
    adapter
        .send(AgentOutput::Delta {
            session_id: "sess-1".into(),
            channel: ChannelType::Http,
            text: "world".into(),
        })
        .await
        .expect("delta");
    adapter
        .send(AgentOutput::Message(OutgoingMessage {
            session_id: "sess-1".into(),
            channel: ChannelType::Http,
            content: vec![ContentBlock::Text("hello world".into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }))
        .await
        .expect("response");

    match rx.recv().await.expect("first") {
        SseEvent::Delta { text } => assert_eq!(text, "hello "),
        other => panic!("expected delta, got {other:?}"),
    }
    match rx.recv().await.expect("second") {
        SseEvent::Delta { text } => assert_eq!(text, "world"),
        other => panic!("expected delta, got {other:?}"),
    }
    match rx.recv().await.expect("third") {
        SseEvent::Response { text } => assert_eq!(text, "hello world"),
        other => panic!("expected response, got {other:?}"),
    }
}

#[tokio::test]
async fn notice_levels_serialise_to_strings() {
    let adapter = Arc::new(HttpAdapter::new());
    let mut rx = adapter.subscribe("sess-1").await;

    adapter
        .send(AgentOutput::Notice {
            session_id: "sess-1".into(),
            channel: ChannelType::Http,
            level: NoticeLevel::Warn,
            text: "heads up".into(),
        })
        .await
        .expect("warn");
    adapter
        .send(AgentOutput::Notice {
            session_id: "sess-1".into(),
            channel: ChannelType::Http,
            level: NoticeLevel::Error,
            text: "blocked".into(),
        })
        .await
        .expect("error");

    match rx.recv().await.expect("first") {
        SseEvent::Notice { level, text } => {
            assert_eq!(level, "warn");
            assert_eq!(text, "heads up");
        }
        other => panic!("expected notice, got {other:?}"),
    }
    match rx.recv().await.expect("second") {
        SseEvent::Notice { level, text } => {
            assert_eq!(level, "error");
            assert_eq!(text, "blocked");
        }
        other => panic!("expected notice, got {other:?}"),
    }
}

#[tokio::test]
async fn outbound_for_other_session_is_not_seen() {
    let adapter = Arc::new(HttpAdapter::new());
    let mut rx = adapter.subscribe("sess-1").await;

    // Fan-out to a different session id must not reach this subscriber.
    adapter
        .send(AgentOutput::Delta {
            session_id: "sess-OTHER".into(),
            channel: ChannelType::Http,
            text: "should not appear".into(),
        })
        .await
        .expect("delta");
    adapter
        .send(AgentOutput::Delta {
            session_id: "sess-1".into(),
            channel: ChannelType::Http,
            text: "visible".into(),
        })
        .await
        .expect("delta");

    match rx.recv().await.expect("first") {
        SseEvent::Delta { text } => assert_eq!(text, "visible"),
        other => panic!("expected delta, got {other:?}"),
    }
}

#[tokio::test]
async fn submit_before_start_fails() {
    let adapter = Arc::new(HttpAdapter::new());
    let err = adapter
        .submit(sample_incoming("sess-1"))
        .await
        .expect_err("submit without start must fail");
    assert!(
        matches!(err, ChannelError::Config(_)),
        "expected Config error, got {err:?}"
    );
}

#[tokio::test]
async fn submit_after_start_reaches_router_intake() {
    let adapter = Arc::new(HttpAdapter::new());
    let (tx, mut rx) = mpsc::channel::<IncomingMessage>(4);
    adapter.start(tx).await.expect("start");

    adapter
        .submit(sample_incoming("sess-1"))
        .await
        .expect("submit");

    let got = rx.recv().await.expect("router intake");
    assert_eq!(got.message.session_id, "sess-1");
    assert_eq!(got.message.channel, ChannelType::Http);
}

#[tokio::test]
async fn stop_closes_live_subscribers() {
    let adapter = Arc::new(HttpAdapter::new());
    let mut rx = adapter.subscribe("sess-1").await;

    adapter.stop().await.expect("stop");

    // After stop, the broadcast sender is dropped; recv should error.
    let result = rx.recv().await;
    assert!(result.is_err(), "expected closed receiver, got {result:?}");
}
