//! Test fixtures shared by per-backend integration tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use baybo_memory::MemoryContext;
use baybo_model::{BUILTIN_AGENT_PROFILE_ID, ChannelType, JobId, SessionId, User};
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

/// Unbound-agent fixture: builds a [`MemoryContext`] scoped to the builtin
/// agent id (`"baybo"`), matching an unbound session's `agent_id_or_builtin()`.
pub async fn memory_context(user_id: &str, session_id: &str, step_kind: StepKind) -> MemoryContext {
    memory_context_for_agent(BUILTIN_AGENT_PROFILE_ID, user_id, session_id, step_kind).await
}

/// Bound-agent fixture: builds a [`MemoryContext`] scoped to `agent_id`, for
/// tests asserting a backend partitions recall/writes by the session's bound
/// agent rather than the builtin default.
pub async fn memory_context_for_agent(
    agent_id: &str,
    user_id: &str,
    session_id: &str,
    step_kind: StepKind,
) -> MemoryContext {
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
        agent_id.to_string(),
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
