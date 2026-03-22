# context - Context Management

## 1. Module Overview

The `context` crate manages Aura's session context window and is one of the core components inside Agent Loop.

**Core responsibilities:**

- **Context appending**: append user messages, assistant replies, tool results, and skill results into the session context
- **Token counting**: accurately count tokens for multimodal messages, including text, images, and tool calls
- **Context compression**: trigger compression automatically when token usage approaches the model limit, reducing context while preserving useful information
- **Snapshots and rollback**: create context snapshots to support session replay and branch rollback together with the Trace system

**Goal**: make sure the context sent to the LLM never exceeds the model's context window while preserving the most valuable information for the current conversation.

---

## 2. Dependencies

### 2.1 Internal Dependencies

- `core`: `Session`, `ChatMessage`, `ContentBlock`, `Role`, `OperationKind`

### 2.2 External Dependencies

| Crate | Purpose |
|-------|------|
| `serde` | Serialize and deserialize `CompressResult`, `ContextSnapshot`, and related types |
| `async-trait` | Async methods for `ContextManager` |
| `tiktoken-rs` | Text token counting compatible with OpenAI tokenizers |

### 2.3 Dependency Boundaries

- Does **not** depend on `llm`. The `Tokenizer` trait is defined here and implemented externally
- Does **not** depend on `memory`. Memory context is injected by the `agent` layer
- Does **not** depend on `trace`. Snapshots are consumed by `trace`, but `context` does not call Trace interfaces itself

### 2.4 Reverse Dependencies

- `trace`: uses `ContextSnapshot` to persist context snapshots
- `agent`: `AgentLoop` holds `Box<dyn ContextManager>` and drives context management during the core conversation loop

---

## 3. Public Interfaces

### 3.1 ContextManager Trait

```rust
#[async_trait]
pub trait ContextManager: Send + Sync {
    async fn append(&self, session: &mut Session, role: Role, msg: &ChatMessage) -> Result<()>;
    async fn maybe_compress(&self, session: &mut Session) -> Result<CompressResult>;
    fn count_tokens(&self, messages: &[ChatMessage]) -> Result<usize>;
    fn snapshot(&self, session: &Session) -> ContextSnapshot;
    fn restore_state(&mut self, snapshot: &ContextSnapshot) -> Result<()>;
}
```

Convenience methods:

| Method | Description |
|------|------|
| `append_assistant(session, text)` | Append an assistant text reply |
| `append_tool_calls(session, calls)` | Append tool call requests |
| `append_tool_results(session, results)` | Append tool execution results |
| `append_skill_result(session, result)` | Append a skill execution result |

Typical call order in Agent Loop:

```text
append(user_msg)
  -> maybe_compress()
  -> LLM call
  -> append_assistant(text)
  -> append_tool_calls(calls)
  -> append_tool_results(results)
  -> append_skill_result(result)
```

### 3.2 CompressResult

```rust
pub struct CompressResult {
    pub compressed: bool,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub strategy_used: String,
    pub latency: Duration,
}
```

If `compressed == false`, then `before_tokens == after_tokens`, and `latency` contains only counting overhead.

Typical uses:

- Record to Trace as `OperationKind::ContextCompression`
- Monitor compression frequency and effectiveness
- Debug context-management behavior

### 3.3 ContextSnapshot

```rust
pub struct ContextSnapshot {
    pub messages: Vec<ChatMessage>,
    pub compressed_summary: Option<String>,
    pub token_count: usize,
}
```

Use cases:

- Trace rollback
- Session branching
- Debug replay

### 3.4 Tokenizer Trait

```rust
pub trait Tokenizer: Send + Sync {
    fn count_text(&self, text: &str) -> usize;
    fn count_image(&self, width: u32, height: u32) -> usize;
    fn count_message(&self, msg: &ChatMessage) -> usize;
}
```

Design notes:

- `Tokenizer` is defined in this crate and implemented externally
- `count_message` should account for structural overhead such as roles and separators
- Different providers may use different counting rules, especially for images

---

## 4. Implementation Details

### 4.1 SlidingWindowContext

The simplest strategy, based on a sliding window.

Implementation:

- Keep only the most recent N messages
- Always preserve the system prompt
- Drop the oldest messages when the count exceeds `keep_recent_messages`

Pros:

- Simple implementation
- Zero compression latency
- Predictable behavior

Cons:

- Early but important context may be discarded
- No notion of message importance
- Not suitable for long conversations that need to preserve key early information

### 4.2 HybridContext

The recommended production strategy combines sliding windows with LLM summarization.

Compression trigger:

```text
current_tokens > max_tokens * compression_threshold
```

Compression flow:

1. Select a batch of early messages to compress, excluding the system prompt and memory context
2. Use `SummarizeCallback` to generate a structured summary
3. Replace the original messages with that summary
4. Keep system prompt, memory context, summary, and recent messages within the total token budget

`SummarizeCallback`:

```rust
#[async_trait]
pub trait SummarizeCallback: Send + Sync {
    async fn summarize(&self, messages: &[ChatMessage]) -> Result<String>;
}
```

This keeps the `context` crate independent from `llm`.

Summary-quality strategies:

- Structured prompts
- Prefer factual information over conversational filler
- Incremental summaries that merge previous summaries with new material
- Validate that the summary is actually smaller than the original content

### 4.3 Context Structure

The context sent to the LLM is organized in descending priority:

```text
System Prompt / Soul
Memory Context
Compressed Summary
Recent Messages
Current User Message
```

Budget strategy:

| Region | Strategy | Notes |
|------|----------|------|
| System Prompt / Soul | Fixed | Never compressed |
| Memory Context | Fixed, injected by `agent` | Long-term relevant memory |
| Compressed Summary | Elastic | Grows as compression happens |
| Recent Messages | Elastic | Holds the main recent history |
| Current User Message | Fixed | Always preserved |

### 4.4 Multimodal Token Counting

Text:

- Use `tiktoken-rs` for OpenAI-style tokenization
- For providers without public tokenizers, use conservative approximation if needed

Images:

- Counting rules differ by provider
- OpenAI-style counting can be tile-based
- Claude-style counting can be approximated by pixel count

Tool-call messages:

- Add structural overhead for function names, parameter JSON, and call IDs

---

## 5. File Structure

```text
crates/context/src/
├── lib.rs
├── sliding_window.rs
├── summarize.rs
└── hybrid.rs
```

| File | Responsibility |
|------|------|
| `lib.rs` | Core traits and data structures |
| `sliding_window.rs` | Sliding window strategy |
| `summarize.rs` | Summary callback and summarization logic |
| `hybrid.rs` | Hybrid compression strategy |

---

## 6. Configuration

Example configuration under `agent.context`:

```json
{
  "agent": {
    "context": {
      "max_tokens": 128000,
      "compression_threshold": 0.8,
      "compression_strategy": "hybrid",
      "keep_recent_messages": 20
    }
  }
}
```

| Field | Type | Default | Description |
|--------|------|--------|------|
| `max_tokens` | `usize` | `128000` | Maximum context tokens |
| `compression_threshold` | `f32` | `0.8` | Compression trigger threshold |
| `compression_strategy` | `String` | `"hybrid"` | `sliding_window` or `hybrid` |
| `keep_recent_messages` | `usize` | `20` | Number of recent messages kept outside summary compression |

Recommended tuning:

- Keep `max_tokens` slightly below the real model limit to reserve output space
- A threshold around `0.7` to `0.85` is usually reasonable
- Tool-heavy conversations often need a larger `keep_recent_messages`
