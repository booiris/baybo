# trace - Call Chain Tracing

## 1. Module Overview

The `trace` crate records the tree of all key operations that happen during processing of a session, including:

- User input handling
- LLM calls
- Tool execution
- Skill execution
- Context compression
- Memory operations
- Rollback and branching

Trace answers "what exactly did this operation do." Therefore it records sanitized inputs, results, latency, and execution provenance. Its difference from `job` is straightforward: Job manages state, Trace manages content.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | `OperationKind`, `ContentBlock`, `ChatMessage` |
| `context` | `ContextSnapshot`, used for rollback restoration |

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `serde` | Trace tree and span serialization |
| `chrono` | Timestamps |
| `async-trait` | Async interface for `TraceStore` |

---

## 3. Public Interfaces

### 3.1 SessionTrace

```rust
pub struct SessionTrace {
    pub session_id: String,
    pub root: TraceNodeId,
    pub nodes: HashMap<TraceNodeId, TraceNode>,
    pub forks: Vec<ForkRecord>,
    pub active_leaf: TraceNodeId,
}
```

### 3.2 TraceNode

```rust
pub struct TraceNode {
    pub id: TraceNodeId,
    pub parent: Option<TraceNodeId>,
    pub children: Vec<TraceNodeId>,
    pub span: TraceSpan,
    pub context_snapshot: Option<ContextSnapshot>,
}
```

### 3.3 TraceSpan

```rust
pub struct TraceSpan {
    pub kind: OperationKind,
    pub job_id: Option<String>,
    pub provenance: ExecutionProvenance,
    pub input: SpanInput,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub result: Option<SpanResult>,
}
```

Both `input` and `result` inside `TraceSpan` are **sanitized records** and must never store real secrets, raw credentials, or full reasoning text.

### 3.4 ExecutionProvenance

```rust
pub struct ExecutionProvenance {
    pub model_id: Option<String>,
    pub provider: Option<String>,
    pub provider_config_hash: Option<String>,
    pub skill_version: Option<String>,
    pub tool_artifact_hash: Option<String>,
    pub soul_version: Option<String>,
}
```

### 3.5 SpanInput

```rust
pub enum SpanInput {
    UserInput { content: Vec<ContentBlock> },
    LLMCall { input_messages: Vec<ChatMessage>, temperature: Option<f32> },
    ToolExecution { parameters: Value },
    SkillExecution { args: Vec<String> },
    ContextCompression { before_tokens: usize },
    MemoryOperation { query: Option<String> },
    None,
}
```

### 3.6 SpanResult

```rust
pub enum SpanResult {
    LLMResponse {
        output_preview: String,
        input_tokens: usize,
        output_tokens: usize,
        reasoning_redacted: bool,
        latency: Duration,
    },
    ToolResult { output: Value, success: bool, latency: Duration },
    SkillResult { output: String },
    ContextCompressionResult { after_tokens: usize, summary: Option<String> },
    FinalResponse { content: String },
    Error { error: String },
}
```

### 3.7 ForkRecord

```rust
pub struct ForkRecord {
    pub id: String,
    pub from_node: TraceNodeId,
    pub fork_root: TraceNodeId,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}
```

### 3.8 TraceStore Trait

```rust
#[async_trait]
pub trait TraceStore: Send + Sync {
    async fn save_trace(&self, trace: &SessionTrace) -> Result<()>;
    async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>>;
    async fn query_traces(&self, filter: TraceFilter) -> Result<Vec<SessionTrace>>;
    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> Result<Option<TraceNode>>;
}
```

### 3.9 TraceCollector

```rust
pub struct TraceCollector {
    session_trace: SessionTrace,
    store: Arc<dyn TraceStore>,
    auto_snapshot: bool,
    snapshot_interval: usize,
}

impl TraceCollector {
    pub fn begin_span(
        &mut self,
        kind: OperationKind,
        job_id: Option<&str>,
        provenance: ExecutionProvenance,
        input: SpanInput,
    ) -> SpanHandle;

    pub fn end_span(&mut self, handle: SpanHandle, result: SpanResult);
    pub fn fork_from(&mut self, node_id: TraceNodeId) -> Result<String>;
    pub fn get_snapshot_at(&self, node_id: TraceNodeId) -> Result<ContextSnapshot>;
    pub async fn flush(&self) -> Result<()>;
}
```

---

## 4. Implementation Details

### 4.1 Tree Structure

Trace uses a tree rather than a list because one LLM call may spawn multiple child operations:

```text
UserMessageHandling
  ├── LlmCall
  ├── ToolExecution
  │    └── LlmCall
  └── MemoryOperation
```

`active_leaf` points to the current active leaf node. New spans are attached below that leaf by default.

### 4.2 begin / end Lifecycle

Recommended flow:

1. `begin_span()` creates a `TraceNode`
2. Fill in `kind`, `job_id`, `provenance`, and `input`
3. Execute the operation
4. `end_span()` fills in `ended_at` and `result`

Upper layers should usually use `ObservabilityRecorder` to create Job and Trace records together, rather than calling them separately in business code.

### 4.3 Sanitization Constraints

Trace must follow these rules by default:

- Record only sanitized payloads
- Secrets in inputs may appear only as placeholders
- Outputs keep only previews or summaries
- `reasoning_redacted = true` means the provider may have returned reasoning, but the system did not persist the plaintext

That is why `SpanResult::LLMResponse` uses `output_preview` instead of full output.

### 4.4 Provenance and Replayability

If the system supports:

- Skill hot reload
- WASM tool hot replacement
- Soul configuration updates
- Provider configuration changes

Then input/output alone is not enough; version source must be recorded. Otherwise historical replay turns into "rerun yesterday's conversation with today's code and config," which is not auditable.

Therefore:

- `skill_version` records the skill version
- `tool_artifact_hash` records the `.wasm` artifact hash
- `provider_config_hash` records a summary hash of model configuration
- `soul_version` records the persona prompt version

### 4.5 Snapshots and Rollback

`ContextSnapshot` comes from the `context` crate and is used to restore session state.

Recommended strategy:

- Save a snapshot automatically every `snapshot_interval` spans
- `ContextSnapshot` stores only logical messages and blob references, never raw media bytes
- If `get_snapshot_at(node_id)` finds no snapshot on the target node, it should walk up the parent chain to find the nearest snapshot

Rollback path:

```text
get_snapshot_at(target_node)
    │
    ├── fork_from(target_node)
    ├── session.messages = snapshot.messages
    └── context_manager.restore_state(snapshot)
```

### 4.6 Branch Semantics

Branching does not overwrite the original chain. It creates a new branch below the target node.

```text
Original branch: ToolCall_A -> LLM_2 -> Response_1
New branch:      ToolCall_A -> LLM_2' -> Tool_B -> Response_2
```

Both branches should be preserved for audit and comparison.

### 4.7 Collaboration with Job

| Dimension | Job | Trace |
|------|-----|-------|
| Focus | State | Content |
| Key fields | `status` | `input/result/provenance` |
| Sensitive-data strategy | Sanitized JSON | Sanitized payloads / summaries |

It is recommended that `ObservabilityRecorder::begin()` simultaneously:

- Creates a Job
- Creates a Trace span
- Cross-links them through `job_id` and `trace_span_id`

### 4.8 Storage Design Recommendations

SQLite tables can be split into:

- `session_traces`
- `trace_nodes`
- `trace_forks`

`save_trace()` should save the whole tree in one transaction to avoid partial writes.

---

## 5. File Structure

```text
crates/trace/src/
├── lib.rs
├── collector.rs
├── tree.rs
├── fork.rs
└── snapshot.rs
```

Responsibility split:

- `lib.rs`: type definitions and `TraceStore`
- `collector.rs`: span lifecycle and persistence
- `tree.rs`: tree operations and `active_leaf` maintenance
- `fork.rs`: branching and rollback helper logic
- `snapshot.rs`: snapshot policy and lookup

---

## 6. Implementation Recommendations

- `TraceCollector` should lock only for short critical sections and never hold locks across `await`
- Trace data may become large in production; prefer paginated node loading rather than materializing the whole tree at once
- Apply uniform sanitization to `SpanResult::Error` to prevent sensitive input from leaking through exception paths
