# hook - Lifecycle Hook System

## 1. Module Overview

The `hook` crate provides Aura with uniform lifecycle extension points, allowing auditing, rewriting, interception, alerting, and other cross-cutting capabilities to be inserted without intruding into the core execution flow.

Its purpose is not business-process orchestration, but:

- Trigger extension logic at key lifecycle points
- Allow extensions to modify context or abort the flow
- Keep security, auditing, and operations logic decoupled from the `agent` main loop

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | Message, session, and error types |

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `async-trait` | Async methods for `Hook` |
| `serde_json` | Structured data for carrying modifications |

---

## 3. Public Interfaces

### 3.1 HookPoint

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

These enum values define the places in the system where extensions are allowed to attach.

### 3.2 Hook Trait

```rust
#[async_trait]
pub trait Hook: Send + Sync {
    fn name(&self) -> &str;
    fn hook_point(&self) -> HookPoint;
    async fn execute(&self, ctx: &mut HookContext) -> Result<HookAction>;
}
```

### 3.3 HookAction

```rust
pub enum HookAction {
    Continue,
    ContinueWith(HookModification),
    Abort(String),
}
```

Semantics:

- `Continue`: make no changes and continue
- `ContinueWith`: modify context and continue
- `Abort`: stop the current flow and let the upper layer handle the error string

### 3.4 HookManager

```rust
pub struct HookManager {
    hooks: HashMap<HookPoint, Vec<Box<dyn Hook>>>,
}

impl HookManager {
    pub fn register(&mut self, hook: impl Hook + 'static);
    pub async fn trigger(&self, point: HookPoint, ctx: &mut HookContext) -> Result<HookAction>;
}
```

### 3.5 HookContext / HookModification

Recommended context types:

```rust
pub struct HookContext {
    pub session_id: String,
    pub user_id: Option<String>,
    pub message: Option<Message>,
    pub response: Option<OutgoingMessage>,
    pub job_id: Option<String>,
    pub trace_span_id: Option<String>,
    pub extra: HashMap<String, Value>,
}

pub struct HookModification {
    pub message: Option<Message>,
    pub response: Option<OutgoingMessage>,
    pub extra: HashMap<String, Value>,
}
```

---

## 4. Implementation Details

### 4.1 Execution Model

It is recommended to execute hooks serially in registration order for the same `HookPoint`:

```text
HookManager::trigger(point, ctx)
    │
    ├── for hook in hooks[point]
    │      ├── execute(ctx)
    │      ├── Continue        -> continue
    │      ├── ContinueWith    -> apply modification and continue
    │      └── Abort           -> stop immediately
    └── return final HookAction
```

Why serial instead of parallel:

- A later hook may depend on changes made by an earlier hook
- Hooks are often used for auditing and interception, where order carries meaning
- Parallel execution introduces merge conflicts between modifications

### 4.2 Modification Propagation

`ContinueWith(HookModification)` should not replace the whole context. It should merge by field:

- Replace `ctx.message` if `message` is present
- Replace `ctx.response` if `response` is present
- Shallow-merge `extra`

This avoids one hook accidentally erasing context written by another.

### 4.3 Failure Handling Strategy

It is recommended to distinguish between two kinds of hooks:

- Critical hooks: failure aborts the main flow
- Non-critical hooks: failure is logged but does not affect the main flow

If this capability is needed, add metadata at registration time:

```rust
pub struct RegisteredHook {
    pub critical: bool,
    pub hook: Box<dyn Hook>,
}
```

### 4.4 Typical Use Cases

- `PreMessage`: attach audit labels to user input
- `PreLLMCall`: inject extra context or metrics
- `PostToolExecution`: record business audit logs
- `PreResponse`: apply uniform response wrapping
- `CostLimitReached`: notify an operational alerting system
- `JobStatusChanged`: sync an external task dashboard

---

## 5. Collaboration with Other Modules

| Module | Collaboration |
|------|---------|
| `agent` | `AgentActor` / `AgentLoop` trigger hooks at key points |
| `job` | `JobStatusChanged` fires after job state changes |
| `cost` | `CostLimitReached` fires when spending limits are exceeded |
| `channels` | `PreResponse` / `PostResponse` can add logic before and after delivery |

---

## 6. Implementation Recommendations

- Maintain a stable semantic contract for each `HookPoint` so modules interpret timing consistently
- `HookContext.extra` must not contain sensitive plaintext and should continue to follow sanitization rules
- Add timeout protection around hook execution to keep external extensions from slowing down the main flow
