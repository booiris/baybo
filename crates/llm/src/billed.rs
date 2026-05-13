//! In-flow LLM call abstraction with built-in cost accounting.
//!
//! [`GuardedLlm`](crate::guard::GuardedLlm) only runs the pre-call admission
//! closure; recording the spend after a successful response is the
//! caller's responsibility. [`BilledChat`] codifies the next step up:
//! one `chat()` performs the full sequence (guard → sanitize → record
//! cost), so a successful return guarantees the spend already landed
//! in the budget ledger. Implementations live downstream (in
//! `aura-agent`, where the budget ledger and security gateway are
//! known); this crate only owns the trait so any caller that holds an
//! `Arc<dyn BilledChat>` can make a billed call without knowing about
//! agent-layer types.

use async_trait::async_trait;
use aura_model::MicroUsd;

use crate::{ChatRequest, LlmResponse, ModelInfo};

/// Result returned by [`BilledChat::chat`]: the provider response paired
/// with the billed cost in micro-USD. `cost_micros == 0` is normal for
/// models the [`MicroUsd`] pricing table hasn't yet learned.
#[derive(Debug, Clone)]
pub struct BilledChatResponse {
    pub response: LlmResponse,
    pub cost_micros: MicroUsd,
}

/// In-flow LLM call with built-in cost accounting. Errors are
/// returned as a sanitized string — the implementation's security
/// gateway has already scrubbed any leaked secrets from the provider
/// message.
#[async_trait]
pub trait BilledChat: Send + Sync {
    fn model_info(&self) -> &ModelInfo;
    async fn chat(
        &self,
        request: &ChatRequest,
    ) -> Result<BilledChatResponse, String>;
}
