# llm - LLM Client Layer

## Overview

The `llm` crate is Aura's infrastructure layer for LLM calls, wrapping the **rig** framework into a unified `LlmClient` interface.

Core responsibilities:

- Provide a unified invocation interface — upper layers call `LlmClient::chat()` or `chat_stream()` without caring about the backend provider
- Hide provider differences behind registry-style extension via `LlmProviderRegistry`
- Leverage rig's native function calling for structured tool-use responses

**Design constraint**: this crate is pure infrastructure with no business logic. It does not depend on `security`, `tools`, or `skills`.

## Design Decisions

### rig-based completion with enum dispatch

`LlmClient` wraps `AnyCompletionModel`, an enum that holds provider-specific rig completion models (OpenAI, Anthropic, Gemini). This uses compile-time enum dispatch instead of trait objects — rig's `CompletionModel` trait is not object-safe (`Clone` + `impl Future`), and the deprecated `CompletionModelDyn` has been removed. Adding a new provider means adding an enum variant and a match arm.

### Streaming

`LlmClient::chat_stream()` returns `LlmStream`, a type-erased `futures::Stream<Item = Result<StreamEvent>>`. `StreamEvent` emits `Text`, `ToolCall`, `Reasoning`, and `Usage` events. The stream maps rig's `StreamedAssistantContent` to these unified events, hiding provider-specific response types.

### Provider registry pattern

`LlmProviderRegistry` holds factory functions keyed by provider name. Built-in providers (OpenAI, Anthropic, Gemini) are registered by the crate itself. New providers are added by implementing `LlmProviderFactory` and registering it.

### Multimodal support

A `multimodal` module converts Aura's `ContentBlock` types into text representations for the LLM. Non-text blocks (images, audio, files) are rendered as descriptive placeholders. `extract_text` joins text blocks for system/assistant message conversion.

### Observability constraints

`LlmResponse` carries provider reasoning/thinking, tool calls, and full output content. The trace layer records all of these: `output_content`, `thinking`, `tool_calls`, and token usage.

### Error handling

Rate-limit retries are not handled in `llm`. They are managed by `AgentLoop` through `ErrorHandler`. Timeout is configurable at the HTTP client level; upper-layer Job monitoring can mark long-running calls as `Stuck`.

## Constraints

- Depends only on `model` (plus `rig-core`, `futures`, `serde`)
- Does not depend on `cost` — instead, `cost` consumes `TokenUsage` produced by `llm`, assembled by `agent`
- API keys should use environment-variable placeholders and must not be stored directly in config files

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `LlmClient::chat()` / `chat_stream()` and handles retries |
| `cost` | Consumes `TokenUsage` and `ModelPricing` to calculate per-call cost |
| `context` | Provides compressed message history for `ChatRequest` |
