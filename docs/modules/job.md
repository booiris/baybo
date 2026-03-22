# job - Job Management System

## 1. Module Overview

The `job` crate is responsible for uniformly managing the lifecycle state of all asynchronous operations in Aura, including:

- User message handling
- LLM calls
- Tool execution
- Skill execution
- Context compression
- Memory reads and writes
- Cron-triggered tasks

The role of Job is to answer "what step is this operation currently at," not "what exactly did this operation do." Detailed input and output are recorded by `trace`.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | Shared types such as `OperationKind` and `AuraError` |

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `serde` / `serde_json` | Job records and JSON input/output |
| `chrono` | Timestamp fields |
| `async-trait` | Async interface for `JobStore` |

### 2.3 Boundaries with Other Modules

- Does not depend on `trace`; it uses `trace_span_id` for loose coupling
- Does not depend on `llm`, `tools`, or `agent`; it is called by upper-layer assembly

---

## 3. Public Interfaces

### 3.1 JobStatus

The Job state machine matches the main architecture:

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

State semantics:

| Status | Meaning |
|------|------|
| `Pending` | Created and waiting to execute |
| `InProgress` | Currently executing |
| `Completed` | Execution finished and waiting to enter the final confirmation chain |
| `Submitted` | Submitted and waiting for final confirmation |
| `Accepted` | Successful terminal state |
| `Failed` | Failed terminal state |
| `Stuck` | Execution is hung and waiting for recovery or failure judgment |

Fixed state transitions:

```text
Pending -> InProgress -> Completed -> Submitted -> Accepted
                     \-> Failed
                     \-> Stuck -> InProgress
                              \-> Failed
```

### 3.2 Job

```rust
pub struct Job {
    pub id: String,
    pub session_id: String,
    pub parent_job_id: Option<String>,
    pub kind: OperationKind,
    pub status: JobStatus,
    pub input: Option<Value>,
    pub output: Option<Value>,
    pub error: Option<String>,
    pub trace_span_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}
```

Field constraints:

- `input/output` store sanitized JSON only; sensitive values must already have been replaced with placeholders
- `completed_at` must be set when entering `Accepted` or `Failed`; it may be filled earlier when entering `Completed`
- `trace_span_id` links to `trace::TraceSpan`

### 3.3 JobTransition

```rust
pub struct JobTransition {
    pub job_id: String,
    pub from: JobStatus,
    pub to: JobStatus,
    pub reason: Option<String>,
    pub timestamp: DateTime<Utc>,
}
```

### 3.4 JobStore Trait

```rust
#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, job: &Job) -> Result<()>;
    async fn get(&self, job_id: &str) -> Result<Option<Job>>;
    async fn update_status(
        &self,
        job_id: &str,
        status: JobStatus,
        output: Option<Value>,
        error: Option<String>,
    ) -> Result<()>;
    async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>>;
    async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>>;
    async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>>;
    async fn record_transition(&self, transition: &JobTransition) -> Result<()>;
    async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>>;
}
```

### 3.5 JobManager

```rust
pub struct JobManager {
    store: Box<dyn JobStore>,
}

impl JobManager {
    pub async fn create_job(
        &self,
        session_id: &str,
        kind: OperationKind,
        parent: Option<&str>,
    ) -> Result<Job>;

    pub async fn start(&self, job_id: &str) -> Result<()>;
    pub async fn complete(&self, job_id: &str, output: Value) -> Result<()>;
    pub async fn submit(&self, job_id: &str) -> Result<()>;
    pub async fn accept(&self, job_id: &str) -> Result<()>;
    pub async fn fail(&self, job_id: &str, error: &str) -> Result<()>;
    pub async fn stuck(&self, job_id: &str, reason: &str) -> Result<()>;
    pub async fn recover(&self, job_id: &str, reason: &str) -> Result<()>;
    pub async fn get_history(&self, job_id: &str) -> Result<Vec<JobTransition>>;
}
```

---

## 4. Implementation Details

### 4.1 State Machine Validation

`JobManager` must read the current state and strictly validate every transition before updating:

| Current State | Allowed Transitions |
|---------|---------|
| `Pending` | `InProgress` |
| `InProgress` | `Completed`, `Failed`, `Stuck` |
| `Completed` | `Submitted` |
| `Submitted` | `Accepted` |
| `Stuck` | `InProgress`, `Failed` |

Any illegal transition should return an error rather than silently overwriting state.

### 4.2 Unified Success Path

Under the current architecture, every successfully completed Job follows the same path:

```text
Pending -> InProgress -> Completed -> Submitted -> Accepted
```

This means:

- There is no longer a special case where some Jobs end at `Completed`
- `Completed` is an internal intermediate state, not the final success state
- Top-level `UserMessageHandling` and child `LlmCall` / `ToolExecution` / `MemoryOperation` jobs can all use the same model

### 4.3 Stuck and Recovery

`Stuck` means "execution state is unknown or hung," for example:

- An LLM call has not returned for a long time
- A WASM tool execution is stuck
- An external dependency has not responded but the task has not definitively failed

Recovery flow:

1. A watchdog scans `InProgress`
2. On timeout it calls `stuck(job_id, reason)`
3. If the system decides recovery is possible, call `recover(job_id, reason)` to move back to `InProgress`
4. If recovery fails or is not allowed, call `fail(job_id, error)`

### 4.4 Job Hierarchy

Typical structure:

```text
Job: UserMessageHandling [Accepted]
  ├── Job: LlmCall [Accepted]
  ├── Job: ToolExecution [Accepted]
  ├── Job: LlmCall [Accepted]
  └── Job: MemoryOperation [Accepted]
```

Constraints:

- `parent_job_id = None` means a top-level Job
- Success or failure of a child job does not automatically rewrite the parent job; the parent state is decided by upper-layer business logic
- `list_children()` returns only direct children; recursive traversal is the caller's responsibility

### 4.5 Collaboration with Trace

The responsibility split between Job and Trace:

| Dimension | Job | Trace |
|------|-----|-------|
| Goal | Manage state | Manage content |
| Granularity | State, timestamps, hierarchy | Input, result, latency, version source |
| Sensitive data | Sanitized JSON only | Sanitized payloads and summaries only |

`ObservabilityRecorder` will usually call both:

1. `JobManager::create_job()` / `start()`
2. `TraceCollector::begin_span()`

And at the end:

1. `complete()` / `submit()` / `accept()` or `fail()`
2. `TraceCollector::end_span()`

### 4.6 Storage Design Recommendations

Use two tables:

- `jobs`
- `job_transitions`

Key indexes:

- `session_id`
- `status`
- `parent_job_id`
- `completed_at`

`update_status()` and `record_transition()` should run in the same transaction to avoid inconsistency between the main record and transition history.

---

## 5. File Structure

```text
crates/job/src/
├── lib.rs
└── manager.rs
```

Responsibility split:

- `lib.rs`: type definitions, state-machine interface, `JobStore`
- `manager.rs`: transition validation, timestamp maintenance, transition recording

---

## 6. Implementation Recommendations

- Do not add `Cancelled` back into `JobStatus`
- `recover()` must return to `InProgress`, not `Pending`
- All externally exposed `input/output/error` must already be sanitized before entering the `job` module
- Write table-driven tests for the state machine that cover all legal and illegal transitions
