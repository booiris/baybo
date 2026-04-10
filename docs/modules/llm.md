# llm - LLM Client Layer

## Overview

The `llm` crate is Aura's infrastructure layer for LLM calls, wrapping the **rig** framework into a unified `LlmClient` interface.

Core responsibilities:

- Provide a unified invocation interface — upper layers call `LlmClient::chat()` without caring about the backend provider
- Hide provider differences behind registry-style extension via `LlmProviderRegistry`
- Leverage rig's native function calling for structured tool-use responses

**Design constraint**: this crate is pure infrastructure with no business logic. It does not depend on `security`, `tools`, or `skills`.

## Design Decisions

### rig-based completion

`LlmClient` wraps `Arc<dyn CompletionModelDyn>` from rig, using dynamic dispatch to uniformly call any provider. Provider factories create rig-native completion models (OpenAI, Anthropic) and hand them to `LlmClient::new()`.

### Provider registry pattern

`LlmProviderRegistry` holds factory functions keyed by provider name. Built-in providers (OpenAI, Anthropic) are registered by the crate itself. New providers are added by implementing `LlmProviderFactory` and registering it.

### Multimodal support

A `multimodal` module converts Aura's `ContentBlock` types into text representations for the LLM. Non-text blocks (images, audio, files) are rendered as descriptive placeholders. `extract_text` joins text blocks for system/assistant message conversion.

### Observability constraints

`LlmResponse` may carry provider reasoning/thinking, but upper layers decide whether it is recorded. Trace records only `output_preview`, token usage, and `reasoning_redacted` — full reasoning is not persisted by default in production.

### Error handling

Rate-limit retries are not handled in `llm`. They are managed by `AgentLoop` through `ErrorHandler`. Timeout is configurable at the HTTP client level; upper-layer Job monitoring can mark long-running calls as `Stuck`.

## Constraints

- Depends only on `model` (plus `rig-core`, `serde`, `async-trait`)
- Does not depend on `cost` — instead, `cost` consumes `TokenUsage` produced by `llm`, assembled by `agent`
- API keys should use environment-variable placeholders and must not be stored directly in config files

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `LlmClient::chat()` and handles retries |
| `cost` | Consumes `TokenUsage` and `ModelPricing` to calculate per-call cost |
| `context` | Provides compressed message history for `ChatRequest` |
