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
    recall_agent_ids: Mutex<Vec<String>>,
    job_completions: Mutex<Vec<(Vec<ContentBlock>, Vec<ContentBlock>)>>,
    job_complete_agent_ids: Mutex<Vec<String>>,
    session_ends: Mutex<Vec<Vec<ChatMessage>>>,
    session_end_agent_ids: Mutex<Vec<String>>,
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

    /// `ctx.agent_id()` for each `recall` call, in order.
    pub fn recall_agent_ids(&self) -> Vec<String> {
        self.recall_agent_ids.lock().clone()
    }

    pub fn job_complete_count(&self) -> usize {
        self.job_completions.lock().len()
    }

    /// The `(user_input, final_output)` pairs passed to each `on_job_complete`.
    pub fn job_completions(&self) -> Vec<(Vec<ContentBlock>, Vec<ContentBlock>)> {
        self.job_completions.lock().clone()
    }

    /// `ctx.agent_id()` for each `on_job_complete` call, in order.
    pub fn job_complete_agent_ids(&self) -> Vec<String> {
        self.job_complete_agent_ids.lock().clone()
    }

    pub fn session_end_count(&self) -> usize {
        self.session_ends.lock().len()
    }

    /// The transcript passed to each `on_session_end` call, in order.
    pub fn session_ends(&self) -> Vec<Vec<ChatMessage>> {
        self.session_ends.lock().clone()
    }

    /// `ctx.agent_id()` for each `on_session_end` call, in order.
    pub fn session_end_agent_ids(&self) -> Vec<String> {
        self.session_end_agent_ids.lock().clone()
    }
}

#[async_trait]
impl Memory for RecordingMemory {
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        self.recall_queries.lock().push(query.to_vec());
        self.recall_agent_ids.lock().push(ctx.agent_id().to_owned());
        Ok(self.canned_recall.lock().clone())
    }

    async fn on_job_complete(
        &self,
        ctx: &MemoryContext,
        user_input: &[ContentBlock],
        final_output: &[ContentBlock],
    ) -> Result<()> {
        self.job_completions
            .lock()
            .push((user_input.to_vec(), final_output.to_vec()));
        self.job_complete_agent_ids
            .lock()
            .push(ctx.agent_id().to_owned());
        Ok(())
    }

    async fn on_session_end(&self, ctx: &MemoryContext, transcript: &[ChatMessage]) -> Result<()> {
        self.session_ends.lock().push(transcript.to_vec());
        self.session_end_agent_ids
            .lock()
            .push(ctx.agent_id().to_owned());
        Ok(())
    }
}
