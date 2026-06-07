//! Test fixtures shared by per-backend integration tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aura_memory::MemoryContext;
use aura_model::{ChannelType, JobId, SessionId, User};
use aura_tools::ToolContext;
use aura_trace::test_support::MemoryTraceStore;
use aura_trace::{SpanRecorder, StepKind, TraceEventStream};
use axum::Router;
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
    let job_id = JobId::new();
    let step = recorder.begin_step(job_id, step_kind).await.unwrap();
    MemoryContext::new(
        user_id.to_string(),
        SessionId::from(session_id),
        job_id,
        recorder,
        step,
    )
}

pub fn tool_context(user_id: &str) -> ToolContext {
    let tmp = std::env::temp_dir();
    ToolContext {
        session_id: SessionId::from("test-session"),
        job_id: JobId::new(),
        span_id: aura_model::SpanId::default(),
        user: User {
            id: user_id.into(),
            name: Some("tester".into()),
            channel: ChannelType::tui(),
        },
        timeout: Duration::from_secs(5),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        workspace_root: PathBuf::from(&tmp),
        workspace_paths: aura_workspace::WorkspacePaths::new(tmp),
        sandbox: None,
        approval: None,
        notifier: None,
        events: aura_tools::noop_event_sink(),
        llm: None,
        secrets: None,
        virtual_reads: None,
        background_jobs: None,
    }
}
