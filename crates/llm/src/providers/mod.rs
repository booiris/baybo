// Internal: rig-backed providers stay crate-private — they're consumed
// only through `LlmProviderRegistry::with_default_providers()`.
pub(crate) mod anthropic;
pub(crate) mod gemini;
pub(crate) mod minimax;
pub(crate) mod openai;

// Public: the subscription provider's OAuth surface (PKCE / device-code
// flows, vault token store) is consumed by `aura-cli` for the OAuth
// branch of `aura llm add` / `aura llm edit` / `aura llm remove`, so
// the module needs to be reachable as
// `aura_llm::providers::openai_subscription`.
pub mod openai_subscription;
