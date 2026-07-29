//! Test fixtures shared by per-backend integration tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use baybo_memory::MemoryContext;
use baybo_model::{ChannelType, SessionId, TurnId, User};
use baybo_tools::ToolContext;
use baybo_trace::test_support::MemoryTraceStore;
use baybo_trace::{SpanRecorder, StepKind, TraceEventStream};
use tokio::task::JoinHandle;

pub struct TestServer {
    pub addr: SocketAddr,
    _handle: JoinHandle<()>,
}

pub async fn spawn(app: Router) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestServer {
        addr,
        _handle: handle,
    }
}

pub fn base_url(server: &TestServer) -> String {
    format!("http://{}", server.addr)
}

pub async fn memory_context(user_id: &str, session_id: &str, step_kind: StepKind) -> MemoryContext {
    let recorder = Arc::new(SpanRecorder::new(
        SessionId::from(session_id),
        user_id.to_string(),
        Arc::new(MemoryTraceStore::new()),
        TraceEventStream::new(),
    ));
    let turn_id = TurnId::new();
    let step = recorder.begin_step(turn_id, step_kind).await.unwrap();
    MemoryContext::new(
        user_id.to_string(),
        SessionId::from(session_id),
        turn_id,
        recorder,
        step,
    )
}

pub fn tool_context(user_id: &str) -> ToolContext {
    let tmp = std::env::temp_dir();
    ToolContext {
        session_id: SessionId::from("test-session"),
        turn_id: TurnId::new(),
        user: User {
            id: user_id.into(),
            name: Some("tester".into()),
            channel: ChannelType::tui(),
        },
        timeout: Duration::from_secs(5),
        workspace_root: PathBuf::from(&tmp),
        workspace_paths: baybo_workspace::WorkspacePaths::new(tmp),
        ..ToolContext::for_test()
    }
}
