//! The agent loop's side of durable asks: what a cancelled batch does to a
//! call parked on an approval, by cancel origin. A turn cancel (`/stop`)
//! abandons the ask before the denial is appended; an ancestor cancel
//! (shutdown) leaves the row pending and the `tool_use` dangling, exactly as
//! a crash would.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use baybo_channels::{AgentEvent, AgentOutput};
use baybo_integration_tests::AgentTestHarness;
use baybo_llm::{StreamEvent, ToolCallInfo};
use baybo_model::human_ask::AskResolution;
use baybo_model::{ApprovalDecision, ContentBlock, ResourceAccess};
use baybo_session::SessionStore;
use baybo_store::{HumanAskRow, HumanAskStore};
use baybo_tools::{Tool, ToolContext, ToolManifest, ToolOutput};
use baybo_turn::CancelReason;
use serde_json::{Value, json};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);
const PARK_TIMEOUT: Duration = Duration::from_secs(5);
const GATED_TOOL: &str = "gated_write";
const CALL_ID: &str = "call-gated";

/// Asks to write a file before it runs; counts how often it actually ran.
struct GatedWrite {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for GatedWrite {
    fn name(&self) -> &str {
        GATED_TOOL
    }
    fn description(&self) -> String {
        "writes after asking".to_string()
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        vec![ResourceAccess::WriteFile {
            path: std::path::PathBuf::from("/tmp/human-ask-e2e"),
        }]
    }
    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::Text("written".to_string()))
    }
}

fn manifest() -> ToolManifest {
    ToolManifest {
        name: GATED_TOOL.to_string(),
        description: "writes after asking".to_string(),
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: json!({"type": "object", "properties": {}}),
        capabilities: vec![baybo_tools::ToolCapability::WriteFile],
        channels: Vec::new(),
    }
}

/// A chat turn whose one tool call is parked on a live approval.
async fn parked_turn() -> (AgentTestHarness, Arc<AtomicUsize>, HumanAskRow) {
    let runs = Arc::new(AtomicUsize::new(0));
    let mut harness = AgentTestHarness::builder()
        .with_tool(
            Arc::new(GatedWrite {
                runs: Arc::clone(&runs),
            }),
            manifest(),
        )
        .with_channel_ask_gate()
        .build();
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::ToolCall(ToolCallInfo {
            id: CALL_ID.to_string(),
            name: GATED_TOOL.to_string(),
            arguments: json!({}),
            signature: None,
        })]);
    harness.send_text("write it").await.unwrap();

    let queue = harness.ask_queues.get_or_create(&harness.session.channel);
    tokio::time::timeout(PARK_TIMEOUT, async {
        while queue.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the call should park on an approval");
    let rows = harness.ask_store.rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let row = rows[0].clone();
    assert_eq!(row.tool_call_id, CALL_ID);
    assert!(row.is_pending());
    (harness, runs, row)
}

async fn stop_turn(harness: &AgentTestHarness) {
    let turns = harness
        .turn_lifecycle
        .list_active_chat_turns_by_session(&harness.session.id)
        .await
        .expect("list active turns");
    assert!(!turns.is_empty(), "a turn must be in flight to cancel");
    for turn in &turns {
        harness
            .turn_lifecycle
            .cancel(&turn.id, CancelReason::UserStopped, vec![])
            .await
            .expect("cancel the in-flight turn");
    }
}

/// Tool results on the transcript for [`CALL_ID`].
async fn results_for_call(harness: &AgentTestHarness) -> Vec<String> {
    harness
        .memory_session_store
        .load_session_messages_with_supersede(&harness.session.id)
        .await
        .expect("load transcript")
        .iter()
        .flat_map(|row| row.message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == CALL_ID => Some(content.to_string()),
            _ => None,
        })
        .collect()
}

fn completed_for_call(outputs: &[AgentOutput]) -> usize {
    outputs
        .iter()
        .filter(
            |o| matches!(&o.event, AgentEvent::ToolCompleted { call_id, .. } if call_id == CALL_ID),
        )
        .count()
}

#[tokio::test]
async fn a_turn_cancel_abandons_the_parked_ask_and_appends_the_denial() {
    let (mut harness, runs, row) = parked_turn().await;

    stop_turn(&harness).await;
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after = harness.ask_store.row(&row.ask_id).expect("row kept");
    assert_eq!(after.resolution, Some(AskResolution::Abandoned));
    assert!(
        after.applied_at.is_some(),
        "the loop's claim stamps the row applied"
    );
    let results = results_for_call(&harness).await;
    assert_eq!(results.len(), 1, "exactly one result: {results:?}");
    assert!(results[0].contains("torn down"), "{results:?}");
    assert_eq!(completed_for_call(&outputs), 1);
    assert!(
        harness
            .ask_queues
            .get_or_create(&harness.session.channel)
            .is_empty()
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    tokio::time::timeout(PARK_TIMEOUT, harness.shutdown())
        .await
        .expect("actor stops");
}

#[tokio::test]
async fn a_turn_cancel_that_loses_the_claim_to_an_answer_appends_nothing() {
    let (mut harness, runs, row) = parked_turn().await;

    // The answer is claimed on the ledger in the same instant the user
    // stops the turn: the waiter never sees it.
    let now = chrono::Utc::now();
    assert!(
        harness
            .ask_store
            .resolve(
                &row.ask_id,
                &AskResolution::Approval(ApprovalDecision::Approve),
                now
            )
            .await
            .expect("resolve")
    );
    stop_turn(&harness).await;
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after = harness.ask_store.row(&row.ask_id).expect("row kept");
    assert_eq!(
        after.resolution,
        Some(AskResolution::Approval(ApprovalDecision::Approve))
    );
    assert!(
        after.applied_at.is_none(),
        "the answer waits for the next settlement"
    );
    assert!(
        results_for_call(&harness).await.is_empty(),
        "a lost claim appends nothing"
    );
    assert_eq!(completed_for_call(&outputs), 0);
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    tokio::time::timeout(PARK_TIMEOUT, harness.shutdown())
        .await
        .expect("actor stops");
}

#[tokio::test]
async fn shutdown_leaves_the_parked_ask_pending_and_the_tool_use_dangling() {
    let (mut harness, runs, row) = parked_turn().await;

    harness.actor_parent_token.cancel();
    let outputs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after = harness.ask_store.row(&row.ask_id).expect("row kept");
    assert!(after.is_pending(), "{after:?}");
    assert!(after.applied_at.is_none());
    assert!(
        results_for_call(&harness).await.is_empty(),
        "nothing is appended for the parked call"
    );
    assert_eq!(completed_for_call(&outputs), 0, "nothing is emitted either");
    let queue = harness.ask_queues.get_or_create(&harness.session.channel);
    assert_eq!(
        queue.liveness(&row.ask_id),
        Some(false),
        "the entry stays on the queue, dead"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 0);

    tokio::time::timeout(PARK_TIMEOUT, harness.shutdown())
        .await
        .expect("actor stops");
}
