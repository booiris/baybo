# llm - LLM Client Layer

## Overview

The `llm` crate is Baybo's infrastructure layer for LLM calls, wrapping the **rig** framework into a unified `LlmClient` interface.

Core responsibilities:

- Provide a unified invocation interface — upper layers call `LlmClient::chat()` or `chat_stream()` without caring about the backend provider
- Hide provider differences behind registry-style extension via `LlmProviderRegistry`
- Leverage rig's native function calling for structured tool-use responses

**Design constraint**: this crate is pure infrastructure with no business logic. It does not depend on `tools` or `skills`. It does depend on `baybo-security` — the `openai-subscription` provider reads its OAuth bundle from the `SecretVault`.

## Design Decisions

### rig-based completion with enum dispatch

`LlmClient` wraps `AnyCompletionModel`, an enum with four variants: `OpenAI`, `Anthropic`, `Gemini`, and `OpenAiSubscription` (the ChatGPT/Codex OAuth path, documented in [`llm-openai-subscription.md`](llm-openai-subscription.md)). The MiniMax provider also routes through the `Anthropic` variant by reusing rig's Anthropic client against MiniMax's Anthropic-compatible endpoint; symmetrically, the DeepSeek provider routes through the `OpenAI` variant against DeepSeek's OpenAI-compatible endpoint (`https://api.deepseek.com`). This uses compile-time enum dispatch instead of trait objects — rig's `CompletionModel` trait is not object-safe (`Clone` + `impl Future`), and the deprecated `CompletionModelDyn` has been removed. Adding a new provider means adding an enum variant and a match arm.

`OpenAiSubscription` bypasses the rig adapter: it speaks the Codex Responses API directly over HTTP with its own OAuth dance, intercepted by `LlmClient::chat` / `chat_stream` before the rig `CompletionRequest` is built.

Subprocess-driven agents (the `claude` binary) are **not** LLM providers and live outside this crate. See [`external-agents.md`](../external-agents.md).

### Reasoning-effort bridge

`ChatRequest` carries an optional per-request reasoning-effort ask:

```rust
pub struct ChatRequest {
    pub messages: Vec<baybo_model::ChatMessage>,
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub tools: Vec<ToolDefinitionForLlm>,
}
```

`None` means "the provider's own configured behavior" — the caller (`AgentLoop::call_llm`, populated from a bound agent profile's `reasoning_effort`; see [`agent-profiles.md`](agent-profiles.md#content-resolution-identity-snapshots-content-follows-live)) only ever sets `Some` when it has an explicit ask. rig's `CompletionRequest` has no native reasoning-effort field, so `LlmClient::chat` / `chat_stream` bridge the value through `additional_params` — a provider-opaque `serde_json::Value` bag rig already threads to every backend — under a namespaced key, `REQUEST_REASONING_EFFORT_PARAM = "baybo_reasoning_effort"` (`crate::lib`), so it can never be mistaken for a real provider API parameter.

The bridge is **gated per variant**, not blanket-injected: `AnyCompletionModel::accepts_request_reasoning_effort()` returns `true` only for `Self::OpenAiSubscription(_)`. `chat` / `chat_stream` build the rig request first, then set `additional_params` **only** when both `request.reasoning_effort.is_some()` and `self.model.accepts_request_reasoning_effort()` — so every other variant's (`OpenAI`, `Anthropic`, `Gemini`, and the `MiniMax`/`DeepSeek` providers that route through them) outbound request body never carries the key at all, not even as an ignored field.

**Provider consumption matrix:**

| Variant | Consumes `reasoning_effort`? |
|---|---|
| `OpenAiSubscription` (Codex Responses, incl. any provider routed through it) | Yes — see below |
| `OpenAI` | No — field never set on the request (gate above) |
| `Anthropic` (incl. MiniMax, routed through this variant) | No |
| `Gemini` | No |

`OpenAiSubscription`'s `effective_reasoning_effort` (`providers/openai_subscription/completion_model.rs`) reads the value back out of `additional_params` at request-build time and prefers it over the client's construction-baked effort (from `LlmEntry.reasoning_effort`): `request.additional_params.get(REQUEST_REASONING_EFFORT_PARAM).or(baked)`. Either way the result is run through the existing per-model-family allow-list, `providers/openai_subscription/reasoning.rs::resolve_effort` (e.g. `gpt-5-pro` only accepts `high`; `gpt-5.2+` adds `xhigh`) — an out-of-range ask clamps to the nearest legal value with a `tracing::warn!`, it never fails the call. So a per-request ask always wins over the baked default when present, and always yields *some* resolved value for the model family, clamped rather than rejected.

### Streaming

`LlmClient::chat_stream()` returns `LlmStream`, a type-erased `futures::Stream<Item = Result<StreamEvent>>`. `StreamEvent` has five variants: `Text`, `ToolCall`, `Reasoning` (incremental delta), `ThinkingBlock(baybo_model::ContentBlock)` (complete structured reasoning block, preserved for providers that require thinking to be echoed back), and `Usage`. The stream maps rig's `StreamedAssistantContent` to these unified events, hiding provider-specific response types.

### Provider registry pattern

`LlmProviderRegistry` holds factory functions keyed by provider name. Built-in providers (OpenAI, Anthropic, Gemini, MiniMax, DeepSeek, OpenAI-subscription) are registered by the crate itself. New providers are added by implementing `LlmProviderFactory` and registering it.

### Multimodal support

A `multimodal` module converts Baybo's `ContentBlock` types into text representations for the LLM. Non-text blocks (images, audio, files) are rendered as descriptive placeholders. `extract_text` joins text blocks for system/assistant message conversion.

### Observability constraints

`LlmResponse` carries provider reasoning/thinking, tool calls, and full output content. The trace layer records all of these: `output_content`, `thinking`, `tool_calls`, and token usage.

### Error handling

Rate-limit retries are not handled in `llm`. They are managed by `AgentLoop` through `ErrorHandler`. Timeout is configurable at the HTTP client level; upper-layer Job monitoring can mark long-running calls as `Stuck`.

## Constraints

- Depends on `model` and `baybo-security` (the latter for the `openai-subscription` OAuth token vault), plus external crates `rig-core`, `reqwest`, `futures`, `serde`, `tokio`, `chrono`, `url`, and similar HTTP/serialization utilities
- Does not depend on `cost` — instead, `cost` consumes `TokenUsage` produced by `llm`, assembled by `agent`
- Does not depend on `baybo-storage` / `baybo-session`
- API keys should use environment-variable placeholders and must not be stored directly in config files

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `LlmClient::chat()` / `chat_stream()` and handles retries; the main loop fills `ChatRequest.reasoning_effort` from `AgentLoopConfig.reasoning_effort` (sourced from a bound agent profile — see [`agent-profiles.md`](agent-profiles.md#content-resolution-identity-snapshots-content-follows-live)); title/observer/compression side-calls leave it `None` |
| `cost` | Consumes `TokenUsage` and `ModelPricing` to calculate per-call cost |
| `context` | Provides compressed message history for `ChatRequest` |
