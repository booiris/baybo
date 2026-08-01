//! Test fixtures shared by per-backend integration tests.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use baybo_memory::{MemoryContext, MemoryScope};
use baybo_model::{AgentProfileId, ChannelType, SessionId, TurnId, User};
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
    memory_context_for_agent(user_id, &AgentProfileId::builtin(), session_id, step_kind).await
}

/// [`memory_context`] bound to a specific agent partition, for the tests that
/// assert what a backend sends on the wire for a non-built-in agent.
pub async fn memory_context_for_agent(
    user_id: &str,
    agent_id: &AgentProfileId,
    session_id: &str,
    step_kind: StepKind,
) -> MemoryContext {
    let recorder = Arc::new(SpanRecorder::new(
        SessionId::from(session_id),
        user_id.to_string(),
        Arc::new(MemoryTraceStore::new()),
        TraceEventStream::new(),
    ));
    let turn_id = TurnId::new();
    let step = recorder.begin_step(turn_id, step_kind).await.unwrap();
    MemoryContext::new(
        MemoryScope {
            user_id: user_id.to_string(),
            session_id: SessionId::from(session_id),
            turn_id,
            agent_id: agent_id.clone(),
        },
        recorder,
        step,
    )
}

pub fn tool_context(user_id: &str) -> ToolContext {
    tool_context_for_agent(user_id, &AgentProfileId::builtin())
}

/// [`tool_context`] bound to a specific agent partition.
pub fn tool_context_for_agent(user_id: &str, agent_id: &AgentProfileId) -> ToolContext {
    let tmp = std::env::temp_dir();
    ToolContext {
        session_id: SessionId::from("test-session"),
        turn_id: TurnId::new(),
        user: User {
            id: user_id.into(),
            name: Some("tester".into()),
            channel: ChannelType::tui(),
        },
        agent_id: agent_id.clone(),
        timeout: Duration::from_secs(5),
        workspace_root: PathBuf::from(&tmp),
        workspace_paths: baybo_workspace::WorkspacePaths::new(tmp),
        ..ToolContext::for_test()
    }
}
