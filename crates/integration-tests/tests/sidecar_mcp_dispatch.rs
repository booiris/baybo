//! Regression coverage for the sidecar MCP dispatch path in
//! `AgentLoop::dispatch_sidecar_tool`.
//!
//! Codex adversarial review (slice 2E) flagged that sidecar dispatch
//! bypassed the local `ToolExecutor` pipeline and therefore skipped
//! `sanitize_tool_output`, the per-call timeout, observability spans,
//! and `reveal_in_value` on params. The fix wraps the dispatch in a
//! dedicated helper that mirrors that pipeline (minus
//! approval/sandbox/trust gates, which don't apply to remote tenant
//! API calls).
//!
//! Each test wires a stub `SidecarMcpProvider` into the agent test
//! harness so the agent loop sees a tool whose name carries the
//! session's channel prefix; the LLM stub then "calls" that tool and
//! we observe what landed in the secret store / output stream.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_integration_tests::AgentTestHarness;
use aura_llm::{StreamEvent, ToolCallInfo};
use aura_model::Session;
use aura_tools::ToolDefinition;
use aura_tools::ToolOutput;
use aura_tools::mcp::SidecarMcpProvider;
use serde_json::json;
use serde_json::Value;

const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const DRAIN_TIMEOUT: Duration = Duration::from_millis(2000);

/// Stub provider that always claims a single tool name and returns a
/// caller-supplied `ToolOutput`. Mimics the `SidecarMcpManager`
/// contract: `claims_tool` is the cheap preflight, `execute_for_session`
/// produces the result.
struct StubProvider {
    tool_name: String,
    schema: Value,
    response: ToolOutput,
}

#[async_trait]
impl SidecarMcpProvider for StubProvider {
    async fn tool_definitions_for_session(&self, _session: &Session) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.tool_name.clone(),
            description: "stub sidecar tool".into(),
            parameters_schema: self.schema.clone(),
        }]
    }

    async fn claims_tool(&self, _session: &Session, name: &str) -> bool {
        name == self.tool_name
    }

    async fn execute_for_session(
        &self,
        _session: &Session,
        name: &str,
        _params: Value,
    ) -> Option<Result<ToolOutput, String>> {
        if name == self.tool_name {
            Some(Ok(self.response.clone()))
        } else {
            None
        }
    }
}

fn tool_call_event(name: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCallInfo {
        id: "call-1".into(),
        name: name.to_string(),
        arguments: json!({}),
        signature: None,
    })
}

#[tokio::test]
async fn sanitize_tool_output_runs_on_sidecar_response() {
    // The default harness session uses `ChannelType::tui`. A sidecar
    // tool advertised as `tui/leaky_tool` matches the prefix the
    // dispatch path keys off `session.user.channel`.
    let provider = Arc::new(StubProvider {
        tool_name: "tui/leaky_tool".into(),
        schema: json!({"type":"object"}),
        response: ToolOutput::Text(format!("here is your key: {AWS_KEY}")),
    });

    let mut harness = AgentTestHarness::builder()
        .with_sidecar_mcp(provider)
        .build();

    // Iter 1: LLM emits a tool call. AgentLoop dispatches via the
    // sidecar provider, sanitizes the result, then loops back into
    // call_llm. Iter 2: LLM emits a final text reply so the actor
    // exits cleanly.
    harness
        .stub_llm
        .push_stream(vec![tool_call_event("tui/leaky_tool")]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("done".into())]);

    harness.send_text("call the tool").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // The leaked AWS key in the tool result must have been minted
    // into a placeholder by `sanitize_tool_output`. Without the fix,
    // the secret_store would still be empty (the executor's
    // sanitize was bypassed for sidecar dispatch).
    assert_eq!(
        harness.secret_store.len(),
        1,
        "sanitize_tool_output should have minted exactly one placeholder for the leaked AWS key",
    );

    harness.shutdown().await;
}

// NOTE: a true timeout regression test would need `tokio::time::pause`
// (gated behind tokio's `test-util` feature, not currently enabled in
// `aura-integration-tests`) or a configurable `SIDECAR_MCP_TIMEOUT`
// for tests. The 30s default makes a real-clock test impractical.
// Tracked as a follow-up — the sanitize test above is the more
// critical regression Codex flagged.
