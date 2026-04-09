# Aura - Intelligent Assistant Architecture Design

## 1. Project Overview

Aura is an intelligent-assistant framework built on top of large language models. It supports multi-channel access, tool invocation, skill extension, and complete context management with compression and error-recovery capabilities.

**Tech stack**: Rust, [rig](https://github.com/0xPlaygrounds/rig) as the unified LLM interface layer

**Core design principles**:

- **Modularity**: each crate is an independent module, traits are defined in the crate that owns them, and crates interact through traits with high cohesion and low coupling
- **Extensibility**: Channels, Tools, and Skills all enter through registries; tools and extensions can be loaded through a WASM runtime
- **Security**: encrypted secret storage, input leak detection, layered execution isolation with WASM by default and containers as fallback, and least-privilege network and credential injection
- **Governance**: every Skill, Tool, and extension must carry source, version, hash, trust level, and capability declarations, and both selection and execution must be auditable
- **Observability**: complete call-chain tracing, a unified Job system for async operation state, and support for replay, branching, and rollback; logs and Trace store sanitized placeholders and summaries by default rather than sensitive plaintext
- **Reliability**: built-in recovery, retry, and graceful-degradation strategies
- **Actor model**: message events are decoupled from execution under an Actor-based concurrency architecture
- **Long-running execution**: supports heartbeat, background routines, workspace identity files, and daemon-style execution rather than only one-off requests

---

## 2. System Architecture Overview

```text
Channels
  Telegram / Discord / HTTP API / CLI
          │
          ▼
ChannelAdapter
          │
          ▼
Security Gateway
  (input leak detection / secret placeholder replacement /
   output re-sanitization, never restoring plaintext)
          │
          ▼
Router / Dispatcher
  (routing, session management, rate limiting, Cost Guard)
          │
          ▼
Agent Engine (Actor)
  AgentActor (one actor per session)
    ├── Mailbox
    ├── Agent Loop
    ├── Response Dispatcher
    ├── Context Manager
    ├── Error Recovery
    └── Execution Policy
          │
          ├── Soul
          ├── Memory
          ├── CronJob
          └── Hook
          │
          ▼
  ┌──────────────┬───────────────┬──────────────────┐
  │ LLM (rig)    │ Tool Registry │ Skill Registry   │
  │ OpenAI       │ Built-in      │ File-based       │
  │ Claude       │ WASM          │ Hot reload       │
  │ Ollama       │ Sandboxed     │ Templated        │
  └──────────────┴──────┬────────┴──────────────────┘
                        │
                        ▼
                   WASM Runtime

Job System
  JobManager
  Pending -> InProgress -> Completed -> Submitted -> Accepted
                     \-> Failed
                     \-> Stuck -> InProgress / Failed

Trace System
  TraceCollector
  Span Tree / Fork / Rollback / Replay

Security Layer
  SecretVault / LeakDetector / CostTracker

Storage Layer
  SessionStore / MemoryStore / CostStore / TraceStore / SecretStore / JobStore
```

---

## 3. Modular Design Principles

### 3.0 Inter-Crate Dependencies and Trait Ownership

**Core rule**: each crate defines its own traits and exposes them as interaction contracts. `core` contains only the most fundamental shared types such as `Message`, `User`, `Session`, and `AuraError`. It contains no business traits.

Trait ownership:

```text
channels  -> ChannelAdapter
llm       -> LlmClient / provider abstractions
tools     -> Tool / ToolRegistry
skills    -> SkillDefinition / SkillRegistry
memory    -> MemoryStore / MemoryManager
context   -> ContextManager / Tokenizer
session   -> SessionStore / SessionManager
trace     -> TraceStore / TraceCollector
security  -> SecretStore / SecretVault / LeakDetector
cost      -> CostStore / CostTracker / CostGuard
hook      -> Hook / HookManager
job       -> JobStore / JobManager / JobStatus
agent     -> AgentActor / Router / Supervisor
storage   -> concrete implementations of all Store traits
```

Dependency direction is acyclic:

```text
core
  ├── channels
  ├── llm
  ├── tools
  ├── skills
  ├── memory
  ├── context
  ├── session
  ├── trace
  ├── job
  ├── security
  ├── cost
  └── hook

storage -> session / memory / trace / security / cost / job
agent   -> llm / tools / skills / memory / workspace / context / session / trace / job / security / cost / hook / sandbox
```

Notes:

- `tools` does not depend on `security`; secrets are injected by the `agent` assembly layer through `ToolContext.secrets`
- `storage` and `agent` are sibling assembly layers that both depend on trait crates
- `session` was separated from `storage` to preserve the rule that a crate owns its own trait definitions

---

## 4. Detailed Core Module Design

### 4.1 core (Shared Foundational Types)

**Responsibility**: only shared data structures that flow across crates. **No business traits**.

```rust
struct Message {
    id: String,
    session_id: String,
    channel: ChannelType,
    sender: User,
    content: Vec<ContentBlock>,
    timestamp: DateTime<Utc>,
    reply_to: Option<String>,
    metadata: MessageMetadata,
}

pub struct MessageMetadata {
    pub channel_specific: Option<ChannelMetadata>,
    pub priority: Option<MessagePriority>,
    pub thread_id: Option<String>,
    pub extra: HashMap<String, Value>,
}

enum ContentBlock {
    Text(String),
    Image { blob: BlobRef, mime_type: String },
    Audio { blob: BlobRef, mime_type: String },
    File { blob: BlobRef, filename: String, mime_type: String },
}

struct BlobRef {
    blob_id: String,
    size_bytes: u64,
    sha256: String,
}
```

Key points:

- multimedia is passed by reference rather than embedding raw binary data
- metadata and session state are typed structs with an `extra` escape hatch
- `OperationKind` is shared by Job and Trace to avoid duplicate enums

### 4.2 channels (Channel Ingress)

Traits are defined in this crate and depend only on `core`.

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn channel_type(&self) -> ChannelType;
    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()>;
    async fn send_response(&self, response: OutgoingMessage) -> Result<()>;
    async fn stop(&self) -> Result<()>;
}
```

Built-in channel implementations:

- `TelegramChannel`
- `DiscordChannel`
- `HttpChannel`
- `CliChannel`

### 4.3 llm (LLM Provider Layer, Based on rig)

```rust
pub struct LlmClient {
    model: Box<dyn rig::completion::Chat>,
    model_info: ModelInfo,
    parse_mode: ResponseParseMode,
}

pub enum ResponseParseMode {
    NativeFunctionCalling,
    PromptGuided {
        tool_schema_prompt: String,
        json_extractor: JsonExtractor,
    },
}

pub trait LlmProviderFactory: Send + Sync {
    fn provider_name(&self) -> &str;
    fn create(&self, config: &LlmProviderConfig) -> Result<LlmClient>;
}
```

Built-in factories:

- `OpenAIProviderFactory`
- `AnthropicProviderFactory`
- `OllamaProviderFactory`

### 4.4 tools (Tool System, WASM Sandbox)

Traits are defined in this crate and secrets are injected by `agent`.

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn required_secrets(&self) -> Vec<String> { vec![] }
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

pub struct ToolContext {
    pub session_id: String,
    pub user: User,
    pub timeout: Duration,
    pub cancellation_token: CancellationToken,
    pub secrets: HashMap<String, SecretValue>,
    pub sandbox_policy: SandboxPolicy,
    pub network_policy: NetworkPolicy,
}
```

Execution-isolation layers:

- WASM by default for pure computation and restricted I/O
- escalate to the `sandbox` crate for high-risk tools such as browser automation, shell, package managers, file writes, and long-running work
- `ToolManifest.capabilities` is a hard declaration, not advisory metadata

### 4.5 skills (Skill System)

```rust
pub struct SkillDefinition {
    pub name: String,
    pub version: String,
    pub description: String,
    pub trigger: SkillTrigger,
    pub prompt_template: String,
    pub allowed_tools: Vec<String>,
    pub post_processing: Option<PostProcessing>,
    pub source: ArtifactSource,
    pub trust_level: TrustLevel,
    pub requirements: SkillRequirements,
    pub token_budget_hint: usize,
}

pub struct SkillRegistry {
    skills: RwLock<HashMap<String, SkillDefinition>>,
    watcher: Option<FileWatcher>,
    selector: SkillSelector,
}
```

Governance model:

- trust tiers: `Trusted`, `Installed`, `Untrusted`
- gating before execution: binaries, env vars, config, and model capabilities
- selection pipeline: `gating -> scoring -> token budget -> tool ceiling attenuation`
- Trace must record `skill_version`, `artifact_hash`, and `source`

### 4.5.1 registry (Extension Registry and Installation Governance)

`registry` handles discovery, download, verification, installation, and upgrade of Tool and Skill extensions. It does not execute them.

```rust
pub struct ExtensionManifest {
    pub name: String,
    pub version: String,
    pub artifact_hash: String,
    pub signature: Option<String>,
    pub source_url: String,
    pub kind: ExtensionKind,
    pub trust_level: TrustLevel,
}
```

### 4.6 memory (Long-Term Memory)

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn store(&self, entry: &MemoryEntry) -> Result<()>;
    async fn retrieve(&self, user_id: &str, key: &str) -> Result<Option<MemoryEntry>>;
    async fn search(&self, user_id: &str, query: &str, limit: usize) -> Result<Vec<MemoryEntry>>;
    async fn delete(&self, id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<MemoryEntry>>;
}

pub struct MemoryManager {
    store: Box<dyn MemoryStore>,
    embedder: Option<Box<dyn rig::embeddings::EmbeddingModel>>,
}
```

Long-running relation:

- `workspace` provides persistent identity and long-term policy files such as `AGENTS.md`, `SOUL.md`, `USER.md`, `IDENTITY.md`, and `HEARTBEAT.md`
- `HeartbeatRunner` periodically checks workspace rules and can trigger maintenance, summaries, and routines
- heartbeat, cron, and background tools must all go through Job and Trace

### 4.7 context (Context Management)

```rust
#[async_trait]
pub trait ContextManager: Send + Sync {
    async fn append(&self, session: &mut Session, role: Role, msg: &ChatMessage) -> Result<()>;
    async fn append_assistant(&self, session: &mut Session, text: &str) -> Result<()>;
    async fn append_tool_calls(&self, session: &mut Session, calls: &[ToolCall]) -> Result<()>;
    async fn append_tool_results(&self, session: &mut Session, results: &[Result<ToolOutput>]) -> Result<()>;
    async fn append_skill_result(&self, session: &mut Session, result: &SkillResult) -> Result<()>;
    async fn maybe_compress(&self, session: &mut Session) -> Result<CompressResult>;
    fn count_tokens(&self, messages: &[ChatMessage]) -> Result<usize>;
    fn snapshot(&self, session: &Session) -> ContextSnapshot;
    fn restore_state(&mut self, snapshot: &ContextSnapshot) -> Result<()>;
}
```

Context structure:

```text
System Prompt / Soul
Memory Context
Compressed Summary
Recent Messages
Current User Message
```

Snapshot constraints:

- `ContextSnapshot` stores only logical messages and blob references
- snapshots are for rollback and replay, not media backup

### 4.8 job (Job Management)

`job` uniformly manages lifecycle state for all asynchronous operations.

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum JobStatus {
    Pending,
    InProgress,
    Completed,
    Submitted,
    Accepted,
    Failed,
    Stuck,
}
```

State machine:

```text
Pending -> InProgress -> Completed -> Submitted -> Accepted
                     \-> Failed
                     \-> Stuck -> InProgress
                              \-> Failed
```

Job vs Trace:

- Job manages **state and lifecycle**
- Trace records **sanitized inputs, outputs, summaries, provenance, and metrics**

### 4.9 trace (Call Chain Tracing)

```rust
pub struct SessionTrace {
    pub session_id: String,
    pub root: TraceNodeId,
    pub nodes: HashMap<TraceNodeId, TraceNode>,
    pub forks: Vec<ForkRecord>,
    pub active_leaf: TraceNodeId,
}

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

Trace constraints:

- only sanitized payloads, placeholders, and summaries are persisted
- never store real secrets, raw credentials, or full reasoning
- provenance must include model, skill version, WASM hash, and Soul version when applicable

Branching and rollback:

```text
Original branch:
  UserMsg_1 -> LLM_1 -> ToolCall_A -> LLM_2 -> Response_1

Rollback after ToolCall_A:
  original branch remains
  new branch forks from ToolCall_A and proceeds independently
```

### 4.10 security (Security Module)

```rust
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, name: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<String>>;
}

pub struct SecurityGateway {
    leak_detector: LeakDetector,
    secret_vault: Arc<SecretVault>,
    policy_decider: Arc<dyn NetworkPolicyDecider>,
}
```

Security constraints:

- secrets in input are replaced with placeholders before entering Agent
- real secrets are injected only at the tool-execution boundary
- outputs, logs, Trace, and Job preserve placeholders or summaries only
- network is deny-by-default and governed jointly by tool manifests and admin policy

### 4.11 cost (Cost Management)

```rust
pub struct CostRecord {
    pub user_id: String,
    pub session_id: String,
    pub job_id: String,
    pub trace_span_id: String,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cost_usd: f64,
    pub timestamp: DateTime<Utc>,
}
```

`CostGuard` enforces per-user and global daily/monthly limits before execution.

### 4.12 hook (Lifecycle Hooks)

```rust
pub enum HookPoint {
    PreMessage,
    PostMessage,
    PreLLMCall,
    PostLLMCall,
    PreToolExecution,
    PostToolExecution,
    PreResponse,
    PostResponse,
    SessionCreated,
    SessionDestroyed,
    CostLimitReached,
    JobStatusChanged,
}
```

Hooks enable audit, interception, rewriting, and alerting without embedding them into the main loop.

### 4.13 agent (Assembly Layer)

**Responsibility**: assemble all modules and implement Agent Loop, the Actor model, Router, Soul, CronJob, and ErrorHandler.

The God Object is split into focused collaborators:

- `AgentActor`: message dispatch and actor lifecycle
- `AgentLoop`: core conversation loop
- `ToolExecutor`: tool execution plus observability
- `ObservabilityRecorder`: Job + Trace + Cost integration
- `HeartbeatRunner` / `RoutineScheduler`: long-running proactive work

```rust
pub struct ObservabilityRecorder {
    job_manager: Arc<JobManager>,
    trace_collector: Arc<Mutex<TraceCollector>>,
    cost_tracker: Arc<CostTracker>,
}

pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    secret_vault: Arc<SecretVault>,
    recorder: Arc<ObservabilityRecorder>,
}

pub struct AgentActor {
    session: Session,
    agent_loop: AgentLoop,
    response_tx: mpsc::Sender<OutgoingMessage>,
    hooks: HookManager,
}
```

Main loop responsibilities:

1. create a root Job and Trace span
2. load Soul and workspace identity
3. recall memory
4. append user input to context
5. iterate LLM -> parse -> tools/skills until final reply
6. store final observability state

Concurrency constraints:

- `ObservabilityRecorder` must not be held across long `await`s
- business code should use only short-lived `begin/succeed/fail`

### 4.14 session (Session Management)

`SessionStore` is defined in its own crate:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get(&self, session_id: &str) -> Result<Option<Session>>;
    async fn save(&self, session: &Session) -> Result<()>;
    async fn delete(&self, session_id: &str) -> Result<()>;
    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>>;
}
```

### 4.15 storage (Unified Storage Implementations)

One giant implementation struct is intentionally avoided. Each backend implementation is split per domain:

```rust
pub struct StorageSet {
    pub session: Box<dyn SessionStore>,
    pub memory: Box<dyn MemoryStore>,
    pub trace: Box<dyn TraceStore>,
    pub secret: Box<dyn SecretStore>,
    pub cost: Box<dyn CostStore>,
    pub job: Box<dyn JobStore>,
}
```

SQLite implementations:

- `SqliteSessionStore`
- `SqliteMemoryStore`
- `SqliteTraceStore`
- `SqliteSecretStore`
- `SqliteCostStore`
- `SqliteJobStore`

### 4.16 sandbox (Execution Isolation Layer)

`sandbox` is an independent crate used by `tools` and `agent`.

```rust
pub enum SandboxPolicy {
    WasmOnly,
    WorkspaceWrite,
    ContainerRestricted,
    ContainerElevated,
}
```

Isolation policies:

- `WasmOnly`: default, restricted host functions and network
- `WorkspaceWrite`: workspace reads and writes only, still no arbitrary subprocesses
- `ContainerRestricted`: container execution with proxy and allowlist controls
- `ContainerElevated`: available only to `Trusted` extensions with explicit approval and full audit records

---

## 5. Crate Layout

```text
aura/
├── Cargo.toml
├── crates/
│   ├── core/
│   ├── channels/
│   ├── llm/
│   ├── tools/
│   ├── registry/
│   ├── skills/
│   ├── memory/
│   ├── workspace/
│   ├── context/
│   ├── session/
│   ├── job/
│   ├── trace/
│   ├── security/
│   ├── cost/
│   ├── hook/
│   ├── agent/
│   ├── sandbox/
│   └── storage/
├── src/main.rs
├── config/
├── skills/
└── tools/
```

Recommended internal split:

- `core`: shared data types only
- `channels`: adapters per platform
- `llm`: client, provider registry, built-in providers, and prompt-guided parsing
- `tools`: tool trait, registry, and WASM tool wrapper
- `registry`: extension catalog loading and verification
- `skills`: skill definitions, registry, and loader
- `memory`: store trait and manager
- `workspace`: identity files and heartbeat parsing
- `context`: manager and compression strategies
- `session`: trait and manager
- `job`: records and state machine manager
- `trace`: tree, collector, fork, and snapshot
- `security`: gateway, leak detector, vault, and crypto
- `cost`: tracker and guard
- `hook`: hook points and manager
- `agent`: actor, loop, observability, supervisor, router, soul, cron, heartbeat, recovery, and policy
- `sandbox`: WASM, container, and network controls
- `storage`: in-memory and SQLite implementations of Store traits

---

## 6. Configuration Design (JSON)

```json
{
  "server": {
    "host": "0.0.0.0",
    "port": 8080
  },
  "agent": {
    "max_iterations": 20,
    "default_model": "claude-sonnet-4-6",
    "context": {
      "max_tokens": 128000,
      "compression_threshold": 0.8,
      "compression_strategy": "hybrid",
      "keep_recent_messages": 20
    },
    "error_recovery": {
      "max_retries": 3,
      "backoff": "exponential",
      "backoff_base_ms": 1000,
      "backoff_max_ms": 30000
    }
  },
  "memory": {
    "enabled": true,
    "max_entries_per_user": 1000,
    "auto_forget_days": 90
  },
  "workspace": {
    "root": "workspace/",
    "identity_files": ["AGENTS.md", "SOUL.md", "USER.md", "IDENTITY.md"],
    "heartbeat_file": "HEARTBEAT.md"
  },
  "cost": {
    "enabled": true,
    "limits": {
      "daily_per_user": 5.0,
      "monthly_per_user": 100.0,
      "daily_global": null,
      "monthly_global": null
    }
  },
  "security": {
    "leak_detection": {
      "enabled": true,
      "default_action": "replace"
    }
  },
  "sandbox": {
    "default_policy": "wasm_only",
    "network": {
      "default_action": "deny",
      "allowed_domains": ["api.openai.com", "api.anthropic.com"]
    }
  },
  "trace": {
    "enabled": true,
    "auto_snapshot": true,
    "snapshot_interval": 5,
    "capture_sensitive_payloads": false,
    "capture_reasoning": "disabled"
  },
  "llm": {
    "providers": {
      "claude": {
        "provider": "anthropic",
        "api_key": "${CLAUDE_API_KEY}",
        "model": "claude-sonnet-4-6"
      },
      "openai": {
        "provider": "openai",
        "api_key": "${OPENAI_API_KEY}",
        "base_url": "https://api.openai.com/v1",
        "model": "gpt-4o"
      },
      "ollama": {
        "provider": "ollama",
        "base_url": "http://localhost:11434",
        "model": "llama3"
      }
    }
  },
  "channels": {
    "http": { "enabled": true, "cors_origins": ["*"] },
    "telegram": { "enabled": false, "bot_token": "${TELEGRAM_BOT_TOKEN}" },
    "discord": { "enabled": false, "bot_token": "${DISCORD_BOT_TOKEN}" },
    "cli": { "enabled": true }
  },
  "storage": {
    "backend": "sqlite",
    "sqlite_path": "data/aura.db"
  },
  "skills": {
    "dir": "skills/",
    "hot_reload": true,
    "record_version_in_trace": true,
    "trusted_dirs": ["skills/"],
    "selection": {
      "max_prompt_tokens": 12000,
      "max_tools_for_installed": 3
    }
  },
  "tools": {
    "wasm_dir": "tools/",
    "default_timeout_ms": 30000,
    "record_artifact_hash_in_trace": true,
    "require_manifest_capabilities": true
  },
  "registry": {
    "enabled": true,
    "catalog_url": "https://registry.example.com/index.json",
    "verify_signature": true,
    "install_dir": "extensions/"
  }
}
```

Key configuration rules:

- secrets should come from environment placeholders, not plaintext files
- network defaults to deny
- Trace and Job must remain sanitized by default
- tool and skill governance must be configurable without rewriting business code

---

## 7. Request Handling Flow (Complete Example)

Example user message on Telegram:

> "Use `sk-abc123` to help me search for Rust async best practices"

Flow:

```text
1. Telegram channel receives the message
2. ChannelAdapter converts it into a unified Message
3. SecurityGateway detects the leaked secret and replaces "sk-abc123" with {{SECRET_x7k9}}
4. Router performs rate limiting and CostGuard checks
5. Router sends AgentMessage::UserInput to AgentActor
6. JobManager creates UserMessageHandling [Pending -> InProgress]
7. TraceCollector creates the root span
8. HookManager runs PreMessage hooks
9. Agent loads Soul and recalls memory
10. ContextManager appends the user message and checks compression
11. JobManager creates an LlmCall child job
12. TraceCollector opens an LLM span
13. LlmClient calls Anthropic through rig
14. JobManager marks the LLM job successful
15. TraceCollector stores sanitized output preview, tokens, and latency
16. CostTracker records cost linked to job_id and trace_span_id
17. Agent parses a ToolCall such as web_search
18. JobManager creates a ToolExecution child job
19. TraceCollector opens a tool span
20. WasmRuntime loads the WASM module and injects the minimal secret set
21. JobManager marks ToolExecution successful
22. TraceCollector closes the tool span
23. Agent opens the second LLM call
24. LlmClient calls the provider again
25. JobManager marks the second LLM call successful
26. Agent parses the final text response
27. MemoryManager decides no new memory is needed
28. JobManager finishes UserMessageHandling through Completed -> Submitted -> Accepted
29. TraceCollector flushes sanitized payload and provenance
30. SecurityGateway re-sanitizes output and preserves {{SECRET_x7k9}} as a placeholder
31. ChannelAdapter converts the result back into Telegram format and sends it
32. HookManager runs PostResponse hooks
```

---

## 8. Development Roadmap

### Phase 1 - Foundational Skeleton

- [ ] workspace and crate layout
- [ ] shared core types
- [ ] Actor-model foundation
- [ ] basic Agent Loop
- [ ] rig integration with OpenAI
- [ ] CLI channel
- [ ] basic context management with sliding windows
- [ ] in-memory storage backend
- [ ] basic Job system
- [ ] basic Trace spans

### Phase 2 - Security and Tools

- [ ] SecretVault encryption
- [ ] SecurityGateway leak detection
- [ ] default WASM execution path
- [ ] container sandbox and network allowlist for high-risk tools
- [ ] Tool trait and ToolRegistry
- [ ] least-privilege secret injection

### Phase 3 - Capability Expansion

- [ ] skill parsing and hot reload
- [ ] skill governance pipeline
- [ ] extension registry with hash and signature verification
- [ ] Soul system
- [ ] long-term memory
- [ ] cron jobs
- [ ] workspace identity loading and HeartbeatRunner
- [ ] lifecycle hooks
- [ ] Claude provider
- [ ] summarize-based context compression
- [ ] error recovery

### Phase 4 - Full Trace Capability

- [ ] branching and rollback
- [ ] automatic context snapshots
- [ ] SQLite TraceStore

### Phase 5 - Channel Integration

- [ ] HTTP API
- [ ] Telegram bot
- [ ] Discord bot

### Phase 6 - Production Hardening

- [ ] CostTracker and CostGuard
- [ ] SQLite storage backend
- [ ] structured logs and observability
- [ ] full multimodal support
- [ ] daemon-style service management
- [ ] background notifications and stuck-job recovery

---

## 9. Confirmed Design Decisions

| Topic | Decision |
|--------|------|
| LLM provider integration | Based on rig, connected through `LlmProviderRegistry` and registered factories |
| Models without function calling | Supported through prompt-guided JSON extraction |
| Multimodal support | v1 includes image and audio |
| Skill vs Tool boundary | Explicit: Tool = atomic operation with isolated execution, Skill = declarative orchestration with governance |
| Extension governance | Every Tool and Skill extension must carry source, version, hash, trust level, and capabilities |
| Concurrency model | Actor model with decoupled message events and execution |
| Streaming output | Not supported; every channel sends after the full response is ready |
| Plugin system | WASM runtime loading and execution |
| Execution isolation | WASM by default, container escalation for high-risk work, deny-by-default networking |
| Config format | JSON |
| Crate naming | No prefixes, such as `core`, `agent`, `llm`, `channels`, and `session` |
| Trait ownership | Every crate defines its own trait; `core` contains only shared data types and `OperationKind` |
| Call-chain recording | Full Trace tree with sanitized payload summaries, provenance, and timing, but no sensitive plaintext or full reasoning |
| Conversation rollback | Fork-based rollback driven by Trace node snapshots |
| Job system | Unified state management for all async operations with fixed transitions through Pending -> InProgress -> Completed -> Submitted -> Accepted |
| Internal Agent split | `AgentActor` for dispatch, `AgentLoop` for conversation flow, `ToolExecutor` for tools, `ObservabilityRecorder` for observability |
| Long-running execution | Supported through workspace identity files, heartbeat, routines, and background jobs |
| Storage implementation | One independent struct per Store trait, created through `StorageFactory` |
| Metadata design | Typed structs replace generic `HashMap<String, Value>`, keeping only `extra` for dynamic extension |
