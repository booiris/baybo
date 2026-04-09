# llm - LLM Client Layer

## Overview

The `llm` crate is Aura's infrastructure layer for LLM calls, wrapping the **rig** framework into a unified `LlmClient` interface.

Core responsibilities:

- Provide a unified invocation interface — upper layers call `LlmClient::chat()` without caring about the backend provider
- Hide provider differences behind registry-style extension via `LlmProviderRegistry`
- Support dual-mode response parsing: native function calling and prompt-guided JSON extraction (for local models without tool-use support)

**Design constraint**: this crate is pure infrastructure with no business logic. It does not depend on `security`, `tools`, or `skills`.

## Design Decisions

### Dual response parsing modes

- **NativeFunctionCalling**: for providers with built-in tool support (OpenAI, Anthropic). Tool definitions are included in the request; structured tool calls are extracted from the response.
- **PromptGuided**: for models without native function calling (e.g. Ollama). Tool schemas are appended to the system prompt, and a `JsonExtractor` pulls tool-call JSON from free text. If extraction fails, the response is treated as plain text.

### Provider registry pattern

`LlmProviderRegistry` holds factory functions keyed by provider name. Built-in providers (OpenAI, Anthropic, Ollama) are registered by the crate itself. New providers are added by implementing a factory and registering it — no external factory trait needed.

### rig integration

`LlmClient` internally holds `Box<dyn rig::completion::Chat>`, using dynamic dispatch to uniformly call different providers. A `rig_adapter` module converts Aura's tool-definition schema into rig's format without depending on the `tools` crate.

### Observability constraints

`LlmResponse` may carry provider reasoning/thinking, but upper layers decide whether it is recorded. Trace records only `output_preview`, token usage, and `reasoning_redacted` — full reasoning is not persisted by default in production.

### Error handling

Rate-limit retries are not handled in `llm`. They are managed by `AgentLoop` through `ErrorHandler`. Timeout is configurable at the HTTP client level; upper-layer Job monitoring can mark long-running calls as `Stuck`.

## Constraints

- Depends only on `model` (plus `rig`, `serde`, `async-trait`)
- Does not depend on `cost` — instead, `cost` consumes `TokenUsage` produced by `llm`, assembled by `agent`
- API keys should use environment-variable placeholders and must not be stored directly in config files

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `LlmClient::chat()` and handles retries |
| `cost` | Consumes `TokenUsage` and `ModelPricing` to calculate per-call cost |
| `context` | Provides compressed message history for `ChatRequest` |
