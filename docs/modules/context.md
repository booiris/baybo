# context - Context Management

## Overview

The `context` crate manages Aura's session context window, a core component inside Agent Loop.

Core responsibilities:

- **Context appending**: append user messages, assistant replies, tool results, and skill results
- **Token counting**: accurately count tokens for multimodal messages (text, images, tool calls)
- **Context compression**: trigger compression automatically when token usage approaches the model limit
- **Snapshots and rollback**: create context snapshots for session replay and branch rollback with the Trace system

**Goal**: ensure the context sent to the LLM never exceeds the model's context window while preserving the most valuable information.

## Design Decisions

### Two compression strategies

- **SlidingWindow**: keep only the most recent N messages, always preserving the system prompt. Simple, zero latency, predictable — but may discard important early context.
- **HybridContext** (recommended for production): combines sliding windows with LLM summarization. When `current_tokens > max_tokens * compression_threshold`, early messages (excluding system prompt and memory context) are summarized by an LLM callback and replaced with the summary.

The `SummarizeCallback` trait keeps `context` independent from `llm` — the callback is injected externally.

### Context priority structure

The context sent to the LLM is organized in descending priority:

1. **System Prompt / Soul** — fixed, never compressed
2. **Memory Context** — fixed, injected by `agent`
3. **Compressed Summary** — elastic, grows as compression happens
4. **Recent Messages** — elastic, main recent history
5. **Current User Message** — fixed, always preserved

### Tokenizer abstraction

`Tokenizer` trait is defined in this crate and implemented externally, keeping `context` free from provider-specific dependencies. Implementations must account for structural overhead (roles, separators) and provider-specific image counting rules.

### Dependency boundaries

- Does **not** depend on `llm` (Tokenizer trait defined locally)
- Does **not** depend on `memory` (memory context injected by `agent`)
- Does **not** depend on `trace` (snapshots are consumed by `trace`, not the reverse)

## Constraints

- Keep `max_tokens` slightly below the real model limit to reserve output space
- `ContextSnapshot` stores only logical messages and blob references, never raw media bytes
- Compression threshold around 0.7–0.85 is usually reasonable
- Tool-heavy conversations often need a larger `keep_recent_messages`

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` holds `Box<dyn ContextManager>` and drives context management |
| `trace` | Uses `ContextSnapshot` for rollback and replay |
| `memory` | Memory context is injected into the context window by `agent`, not by `context` itself |
