//! Test doubles for the memory subsystem.
//!
//! Gated behind the `test-support` cargo feature so they never ship in release
//! builds. Lives next to the [`crate::Memory`] trait so downstream crates
//! (the agent loop tests, integration tests) can drive the hooks without a real
//! backend.

use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{ChatMessage, ContentBlock};
use parking_lot::Mutex;

use crate::{Memory, MemoryContext, RecalledMemory, Result};

/// A [`Memory`] that records every hook invocation and returns a configurable
/// recall result, so a downstream test can assert the agent loop drove
/// recall/write at the expected points. `recall` returns clones of the canned
/// list; the write hooks just capture their arguments.
#[derive(Default)]
pub struct RecordingMemory {
    canned_recall: Mutex<Vec<RecalledMemory>>,
    recall_queries: Mutex<Vec<Vec<ContentBlock>>>,
    turn_completions: Mutex<Vec<(Vec<ContentBlock>, Vec<ContentBlock>)>>,
    session_ends: Mutex<Vec<Vec<ChatMessage>>>,
}

impl RecordingMemory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap in an `Arc<dyn Memory>` for direct wiring into an `AgentLoopConfig`.
    pub fn arc() -> Arc<dyn Memory> {
        Arc::new(Self::new())
    }

    /// Set what every `recall` call returns.
    pub fn with_recall(self, memories: Vec<RecalledMemory>) -> Self {
        *self.canned_recall.lock() = memories;
        self
    }

    pub fn recall_count(&self) -> usize {
        self.recall_queries.lock().len()
    }

    /// The query passed to each `recall` call, in order.
    pub fn recall_queries(&self) -> Vec<Vec<ContentBlock>> {
        self.recall_queries.lock().clone()
    }

    pub fn turn_complete_count(&self) -> usize {
        self.turn_completions.lock().len()
    }

    /// The `(user_input, final_output)` pairs passed to each `on_turn_complete`.
    pub fn turn_completions(&self) -> Vec<(Vec<ContentBlock>, Vec<ContentBlock>)> {
        self.turn_completions.lock().clone()
    }

    pub fn session_end_count(&self) -> usize {
        self.session_ends.lock().len()
    }

    /// The transcript passed to each `on_session_end` call, in order.
    pub fn session_ends(&self) -> Vec<Vec<ChatMessage>> {
        self.session_ends.lock().clone()
    }
}

#[async_trait]
impl Memory for RecordingMemory {
    async fn recall(
        &self,
        _ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        self.recall_queries.lock().push(query.to_vec());
        Ok(self.canned_recall.lock().clone())
    }

    async fn on_turn_complete(
        &self,
        _ctx: &MemoryContext,
        user_input: &[ContentBlock],
        final_output: &[ContentBlock],
    ) -> Result<()> {
        self.turn_completions
            .lock()
            .push((user_input.to_vec(), final_output.to_vec()));
        Ok(())
    }

    async fn on_session_end(&self, _ctx: &MemoryContext, transcript: &[ChatMessage]) -> Result<()> {
        self.session_ends.lock().push(transcript.to_vec());
        Ok(())
    }
}
