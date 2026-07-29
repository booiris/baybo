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

`LlmClient` wraps `AnyCompletionModel`, an enum with one variant per provider — `OpenAI`, `Anthropic`, `Gemini`, `DeepSeek`, `Minimax`, a set of rig-backed hosts added via the `rig_provider_factory!` macro (xAI, Mistral, Cohere, Perplexity, Moonshot, Z.ai, XiaomiMiMo, Groq, Together, Ollama, llamafile, Hyperbolic, HuggingFace), and `OpenAiSubscription` (the ChatGPT/Codex OAuth path, documented in [`llm-openai-subscription.md`](llm-openai-subscription.md)). The MiniMax provider uses rig's dedicated MiniMax client on its Anthropic-compatible surface (default base URL `https://api.minimaxi.com/anthropic`), sharing the Anthropic variant's cache-bucket folding and stream path; DeepSeek uses rig's dedicated `deepseek` provider (default `https://api.deepseek.com`) rather than the generic OpenAI-compatible path, because thinking mode requires `reasoning_content` round-tripped on assistant tool-call turns. This uses compile-time enum dispatch instead of trait objects — rig's `CompletionModel` trait is not object-safe (`Clone` + `impl Future`), and the deprecated `CompletionModelDyn` has been removed. Adding a new provider means adding an enum variant and a match arm.

`OpenAiSubscription` bypasses the rig adapter: it speaks the Codex Responses API directly over HTTP with its own OAuth dance; it plugs into the same enum dispatch as the rig providers — `LlmClient` builds the rig `CompletionRequest` normally, and `OpenAiSubscriptionCompletionModel` converts it into a Codex Responses API request with custom auth and 401-refresh handling.

Subprocess-driven agents (the `claude` binary) are **not** LLM providers and live outside this crate. See [`external-agents.md`](../external-agents.md).

### Streaming

`LlmClient::chat_stream()` returns `LlmStream`, a type-erased `futures::Stream<Item = Result<StreamEvent>>`. `StreamEvent` has five variants: `Text`, `ToolCall`, `Reasoning` (incremental delta), `ThinkingBlock(baybo_model::ContentBlock)` (complete structured reasoning block, preserved for providers that require thinking to be echoed back), and `Usage`. The stream maps rig's `StreamedAssistantContent` to these unified events, hiding provider-specific response types.

### Provider registry pattern

`LlmProviderRegistry` holds factory functions keyed by provider name. Built-in providers (OpenAI, Anthropic, Gemini, MiniMax, DeepSeek, xAI, Mistral, Cohere, Perplexity, Moonshot, Z.ai, XiaomiMiMo, Groq, Together, Ollama, llamafile, Hyperbolic, HuggingFace, OpenAI-subscription) are registered by the crate itself. New providers are added by implementing `LlmProviderFactory` and registering it.

### Multimodal support

When a `BlobFetcher` is attached (`LlmClient::with_blob_fetcher`) and the model reports `supports_vision`, `ContentBlock::Image` / `Audio` / `File` user blocks are materialised into real rig `Image` / `Audio` / `Document` content (base64-encoded blob bytes). Otherwise — no fetcher, text-only model, unsupported MIME type, or blob fetch failure — the block degrades to a descriptive text placeholder via the `multimodal` module (`[image: …]`-style stubs). `extract_text` joins text blocks for system/assistant message conversion.

### Observability constraints

`LlmResponse` carries provider reasoning/thinking, tool calls, and full output content. The trace layer records all of these: `output_content`, `thinking`, `tool_calls`, and token usage.

### Error handling

Rate-limit retries are not handled in `llm`. They are managed by `AgentLoop` through `ErrorHandler`. Timeout is configurable at the HTTP client level; upper-layer Turn monitoring can mark long-running calls as `Stuck`.

## Constraints

- Depends on `model` and `baybo-security` (the latter for the `openai-subscription` OAuth token vault), plus external crates `rig-core`, `reqwest`, `futures`, `serde`, `tokio`, `chrono`, `url`, and similar HTTP/serialization utilities
- Does not depend on `cost` — the dependency is one-directional (`cost` → `llm`): `cost` injects opaque `CostHooks` (admission guard + usage recorder) that `llm`'s `BoundBilledLlm` runs around every call, so a successful return guarantees the spend was recorded
- Does not depend on `baybo-storage` / `baybo-session`
- API keys should use environment-variable placeholders and must not be stored directly in config files

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `chat()` / `chat_stream()` through the `BillableLlm` / `BoundBilledLlm` billing wrapper and handles retries via `ErrorHandler` |
| `cost` | Consumes `TokenUsage` and `ModelPricing` to calculate per-call cost |
| `context` | Provides compressed message history for `ChatRequest` |
