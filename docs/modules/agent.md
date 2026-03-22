# agent - Assembly Layer and Execution Engine

## 1. Module Overview

The `agent` crate is Aura's top-level assembly layer. It connects modules such as `llm`, `context`, `tools`, `skills`, `memory`, `workspace`, `trace`, `job`, `cost`, `security`, and `sandbox` into an executable engine.

It does not own low-level storage or backend implementation details. Instead, it is responsible for:

- Message dispatch and session isolation under the Actor model
- The agent main loop: LLM calls, tool execution, skill execution, and final reply generation
- Long-running execution: heartbeat, routine, cron, and background notifications
- Unified observability wrapping for Job, Trace, and Cost
- Runtime logic such as error recovery, timeout control, and rollback

---

## 2. Dependencies

### 2.1 Internal Dependencies

`agent` is the top-level assembly module and depends on the following business crates:

- `core`
- `llm`
- `tools`
- `skills`
- `memory`
- `workspace`
- `context`
- `session`
- `trace`
- `job`
- `security`
- `cost`
- `hook`
- `sandbox`

### 2.2 Design Positioning

`agent` does not define low-level Store traits and does not implement SQLite or container backends. Those are provided by other crates. `agent` consumes them through dependency injection and composes them into one observable execution chain.

---

## 3. Public Interfaces

### 3.1 AgentActor

```rust
pub struct AgentActor {
    session: Session,
    agent_loop: AgentLoop,
    response_tx: mpsc::Sender<OutgoingMessage>,
    hooks: HookManager,
}
```

`AgentActor` does only two things:

- Receive mailbox messages
- Delegate business logic to `AgentLoop`

### 3.2 AgentMessage

```rust
pub enum AgentMessage {
    UserInput(IncomingMessage),
    CronTrigger { job_id: String },
    HeartbeatTick,
    RoutineTrigger { routine_id: String },
    Rollback { target_node_id: TraceNodeId },
    SessionTimeout,
    System(SystemCommand),
}
```

### 3.3 AgentLoop

```rust
pub struct AgentLoop {
    llm: Arc<LlmClient>,
    context_manager: Box<dyn ContextManager>,
    tool_executor: ToolExecutor,
    skill_registry: Arc<SkillRegistry>,
    memory: MemoryManager,
    workspace: Arc<WorkspaceManager>,
    soul: Soul,
    error_handler: ErrorHandler,
    policy: ExecutionPolicy,
    recorder: Arc<ObservabilityRecorder>,
}
```

Core interface:

```rust
impl AgentLoop {
    pub async fn run(&mut self, session: &mut Session, input: IncomingMessage) -> Result<OutgoingMessage>;
    pub async fn run_cron(&mut self, session: &mut Session, job_id: &str) -> Result<()>;
    pub async fn run_heartbeat(&mut self, session: &mut Session) -> Result<()>;
    pub async fn run_routine(&mut self, session: &mut Session, routine_id: &str) -> Result<()>;
    pub async fn rollback(&mut self, session: &mut Session, target_node_id: TraceNodeId) -> Result<()>;
}
```

### 3.4 ToolExecutor

```rust
pub struct ToolExecutor {
    tool_registry: Arc<ToolRegistry>,
    secret_vault: Arc<SecretVault>,
    recorder: Arc<ObservabilityRecorder>,
}

impl ToolExecutor {
    pub async fn execute_calls(
        &self,
        calls: &[ToolCall],
        session: &Session,
        parent_job_id: &str,
    ) -> Vec<Result<ToolOutput>>;
}
```

### 3.5 ObservabilityRecorder

```rust
pub struct ObservabilityRecorder {
    job_manager: Arc<JobManager>,
    trace_collector: Arc<Mutex<TraceCollector>>,
    cost_tracker: Arc<CostTracker>,
}
```

### 3.6 Long-Running Collaborators

```rust
pub struct HeartbeatRunner {
    interval: Duration,
    workspace: Arc<WorkspaceManager>,
    scheduler: Arc<RoutineScheduler>,
}

pub struct RoutineScheduler {
    routines: Vec<RoutineDefinition>,
    agent_supervisor: Arc<AgentSupervisor>,
}

pub struct RoutineDefinition {
    pub id: String,
    pub schedule: RoutineSchedule,
    pub prompt: String,
    pub notify_channel: Option<String>,
}
```

### 3.7 Other Runtime Collaborators

```rust
pub struct Soul {
    pub name: String,
    pub base_prompt: String,
    pub guidelines: Vec<String>,
    pub style: InteractionStyle,
    pub restrictions: Vec<String>,
}

pub struct ErrorHandler {
    retry_policy: RetryPolicy,
    fallback_chain: Vec<FallbackStrategy>,
}

pub struct Router {
    session_manager: SessionManager,
    rate_limiter: RateLimiter,
    cost_guard: CostGuard,
    agent_supervisor: AgentSupervisor,
}

pub struct AgentSupervisor {
    actors: HashMap<SessionId, ActorHandle<AgentMessage>>,
    config: AgentConfig,
}
```

---

## 4. Implementation Details

### 4.1 Actor Isolation Model

The system adopts the convention of "one Actor per session":

```text
Router
  │
  ├── session_a -> AgentActor A
  ├── session_b -> AgentActor B
  └── session_c -> AgentActor C
```

Benefits:

- Natural serialization inside the same session, avoiding context races
- Natural concurrency across sessions
- Control messages such as rollback, timeout, cron, heartbeat, and routine can all be routed to the same actor

### 4.2 Main Execution Path of AgentLoop

The standard path:

1. Create the top-level Job and Trace span
2. Build the system prompt, Soul, and identity injection from `workspace`
3. Recall long-term memory
4. Append the current user message to Context
5. Loop:
   - `ContextManager::maybe_compress()`
   - Build `ChatRequest`
   - Call `LlmClient`
   - Parse the response into text, tool calls, or skill invocation
   - Dispatch tool or skill execution
6. Produce the final `OutgoingMessage`
7. Persist final Job, Trace, and Cost state

### 4.3 Lock Strategy of ObservabilityRecorder

It is critical to avoid holding the recorder lock across long waits in business flow:

- `ObservabilityRecorder` should expose short-lived `begin/succeed/fail`
- `TraceCollector` can use `Arc<Mutex<_>>` internally for short critical updates
- `AgentLoop` and `ToolExecutor` must not hold locks while waiting for LLM calls or tool execution

### 4.4 Responsibility Boundary of ToolExecutor

`ToolExecutor` is responsible for:

- Looking up tools by `ToolCall`
- Reading declared secrets for tools
- Determining `SandboxPolicy` and `NetworkPolicy` under governance rules
- Constructing `ToolContext`
- Creating Job and Trace child nodes for tool execution
- Executing tools and writing back results

It is not responsible for deciding whether a tool should be called at all. That remains the responsibility of `AgentLoop`.

### 4.5 Long-Running Model

Long-running tasks are uniformly incorporated into the Actor model and observability chain:

```text
HeartbeatRunner / RoutineScheduler
    │
    ▼
AgentSupervisor
    │
    ▼
AgentMessage::HeartbeatTick / RoutineTrigger
    │
    ▼
AgentLoop::run_heartbeat() / run_routine()
```

Key constraints:

- Heartbeat, routine, and cron all create Job and Trace records
- Background results should be delivered asynchronously by default and must not pollute the active foreground conversation
- Stuck-job recovery uses the same state machine as normal tasks

### 4.6 Rollback Mechanism

Rollback flow:

```text
AgentMessage::Rollback
    │
    ▼
AgentLoop::rollback()
    │
    ├── read the snapshot for target_node_id from TraceCollector
    ├── fork_from(target_node_id)
    ├── session.messages = snapshot.messages
    └── context_manager.restore_state(snapshot)
```

### 4.7 Router's Upstream Responsibilities

Before a message enters an actor, `Router` should complete:

- Session identification or creation
- User-level rate limiting
- Quota checking through `CostGuard`
- Selecting or creating the target `AgentActor`
- Routing heartbeat, routine, and cron to the target session or notification session

---

## 5. Collaboration with Other Modules

| Module | Collaboration |
|------|---------|
| `llm` | `AgentLoop` initiates model calls |
| `tools` | `ToolExecutor` executes tools |
| `skills` | `AgentLoop` parses and executes skills |
| `memory` | Recall before input, decide whether to store after output |
| `workspace` | Supplies identity files, heartbeat configuration, and routine definitions |
| `context` | Maintains the conversation window and compression |
| `job` / `trace` / `cost` | Recorded uniformly through `ObservabilityRecorder` |
| `security` | `ToolExecutor` obtains secrets from `SecretVault` and consumes network-policy decisions |
| `sandbox` | Executes WASM or container-isolated runs |
| `hook` | `AgentActor` triggers hooks at lifecycle points |

---

## 6. Implementation Recommendations

- Keep `AgentActor` thin and prevent it from evolving back into a God Object
- Make the parse result of `AgentLoop` explicit as a `ParsedResponse` enum
- Keep `job_id`, `trace_node_id`, and `started_at` inside `OperationHandle`
- Set `max_iterations` on the main `run()` loop
- Heartbeat and routine should not bypass `AgentSupervisor`
- Background notification targets should be explicitly configured to avoid sending async results to the wrong session
