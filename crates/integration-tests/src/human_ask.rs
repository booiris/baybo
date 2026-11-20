use crate::AgentTestHarness;
use baybo_llm::StreamEvent;
use baybo_model::{ChatMessage, ContentBlock, ResourceAccess};
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolManifest, ToolOutput};
use baybo_turn::TurnInputKind;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const CALL_ID: &str = "call-parked";
pub const GATED_TOOL: &str = "gated_write";
pub const WRITTEN: &str = "written";

/// Asks to write a file before it runs; counts how often it actually ran.
pub struct GatedWrite {
    runs: Arc<AtomicUsize>,
    on_execute: Option<Box<dyn Fn() + Send + Sync>>,
}

impl GatedWrite {
    pub fn new(runs: Arc<AtomicUsize>) -> Self {
        Self {
            runs,
            on_execute: None,
        }
    }

    pub fn on_execute(mut self, probe: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_execute = Some(Box::new(probe));
        self
    }

    pub fn manifest(&self) -> ToolManifest {
        ToolManifest {
            name: self.name().into(),
            description: self.description(),
            trust_level: baybo_model::TrustLevel::Trusted,
            parameters_schema: self.parameters_schema(),
            capabilities: vec![baybo_tools::ToolCapability::WriteFile],
            channels: Vec::new(),
        }
    }
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
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }
    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        vec![ResourceAccess::WriteFile {
            path: PathBuf::from("/tmp/human-ask-e2e"),
        }]
    }
    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if let Some(probe) = &self.on_execute {
            probe();
        }
        Ok(ToolOutput::Text(WRITTEN.to_string()))
    }
}

pub async fn transcript(harness: &AgentTestHarness) -> Vec<ChatMessage> {
    harness
        .session_store
        .load_session_messages_with_supersede(&harness.session.id)
        .await
        .expect("load transcript")
        .into_iter()
        .map(|row| row.message)
        .collect()
}

pub fn result_text(message: &ChatMessage) -> Option<String> {
    message.content.iter().find_map(|block| match block {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } if tool_use_id == CALL_ID => Some(content.to_string()),
        _ => None,
    })
}

pub fn tool_use_at(rows: &[ChatMessage]) -> Option<usize> {
    rows.iter().position(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == CALL_ID))
    })
}

/// The call's single result, which must sit right after its `tool_use`.
pub async fn adjacent_result(harness: &AgentTestHarness) -> String {
    let rows = transcript(harness).await;
    let results: Vec<String> = rows.iter().filter_map(result_text).collect();
    assert_eq!(results.len(), 1, "exactly one result: {rows:#?}");
    let at = tool_use_at(&rows).expect("the tool_use row is kept");
    let result = result_text(&rows[at + 1])
        .unwrap_or_else(|| panic!("the result must be adjacent to its tool_use: {rows:#?}"));
    assert!(
        !result.contains("interrupted"),
        "a parked call is never crash-filled: {result}"
    );
    result
}

async fn turns_of(harness: &AgentTestHarness, kind: TurnInputKind) -> usize {
    harness
        .turn_lifecycle
        .list_by_session(&harness.session.id, None)
        .await
        .expect("list turns")
        .iter()
        .filter(|turn| turn.input.input_kind() == kind)
        .count()
}

/// Turns of the chat Resume door: an answer carried on into a reply.
pub async fn resume_turns(harness: &AgentTestHarness) -> usize {
    turns_of(harness, TurnInputKind::Resume).await
}

/// Turns of a door's prelude: asks settled ahead of the door's own turn.
pub async fn settlement_turns(harness: &AgentTestHarness) -> usize {
    turns_of(harness, TurnInputKind::AskSettlement).await
}

pub fn reply(harness: &AgentTestHarness, text: &str) {
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text(text.to_string())]);
}
