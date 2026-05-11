//! Canonical per-entry LLM pricing override.
//!
//! Operators can pin individual price components on top of whatever
//! the OpenRouter snapshot or per-provider factory default produces.
//! Unset (`None`) fields keep the underlying default; set fields
//! replace it. Lives in `aura-model` so `aura-config` (persistence)
//! and `aura-llm` (consumption) can share one struct without taking a
//! dependency on each other.

use serde::{Deserialize, Serialize};

use crate::MicroUsd;

/// Per-token pricing override. Each field is independently optional —
/// `None` keeps the factory / OpenRouter default for that field; `Some`
/// pins it. Wire shape mirrors the corresponding fields of
/// `aura_llm::ModelPricing`: integer micro-USD per **1 million tokens**.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmPricingOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_1m_tokens: Option<MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_1m_tokens: Option<MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_1m_tokens: Option<MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_1m_tokens: Option<MicroUsd>,
}
