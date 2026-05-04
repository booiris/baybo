# hook - Lifecycle Hook System

## Overview

The `hook` crate provides uniform lifecycle extension points for auditing, rewriting, interception, and alerting — without intruding into core execution flow.

Core responsibilities:

- Trigger extension logic at 31 lifecycle points across session, turn, LLM, tool, response, subagent, task/job, context, cost, and async events
- Allow extensions to inspect, modify, block, or abort execution flows
- Filter hook execution via matchers (exact, pattern, regex)
- Support multiple hook handler types: trait (Rust), command (shell), HTTP (webhook)
- Keep security, auditing, and operations logic decoupled from the `agent` main loop

## Hook Events

31 lifecycle events organized by firing cadence:

### Per-Session Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `SessionStart` | Session begins or resumes | Start method: `new`, `resume`, `clear` | No |
| `SessionEnd` | Session terminates | Exit reason: `normal`, `timeout`, `error`, `clear` | No |

### Per-Turn Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `UserPromptSubmit` | User submits prompt, before any processing | — (always fires) | Yes — blocks prompt, message discarded |
| `PreMessage` | Before message enters agent loop | — (always fires) | No |
| `PostMessage` | After message fully processed | — (always fires) | No |
| `Stop` | Agent finishes responding normally | — (always fires) | Yes — prevents stop, agent loop continues |
| `StopFailure` | Turn ends due to error | Error type: `rate_limit`, `timeout`, `llm_error`, `cost_limit`, `unknown` | No |

### LLM Lifecycle Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `PreLLMCall` | Before LLM invocation | Provider name | No |
| `PostLLMCall` | After LLM response received | Provider name | No |

### Agentic Loop Events (per tool call)

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `PreToolUse` | Before tool executes (can block/modify input) | Tool name | Yes — blocks tool call, agent receives denial |
| `PostToolUse` | Tool executes successfully | Tool name | No |
| `PostToolUseFailure` | Tool execution fails | Tool name | No |
| `PermissionRequest` | Permission check before tool execution | Tool name | Yes — blocks tool call |
| `PermissionDenied` | Tool execution denied by permission system | Tool name | No |

### Response Delivery Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `PreResponse` | Before sending response to channel | Channel type | No |
| `PostResponse` | After response delivered to channel | Channel type | No |

### Subagent Lifecycle Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `SubagentStart` | Child agent actor spawned | Agent type/name | No |
| `SubagentStop` | Child agent finishes | Agent type/name | Yes — prevents stop, agent continues |

### Task and Job Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `TaskCreated` | Task created | — (always fires) | Yes — prevents task creation |
| `TaskCompleted` | Task marked complete | — (always fires) | Yes — prevents task completion |
| `TeammateIdle` | Agent actor about to go idle | Agent type/name | Yes — prevents idle transition |
| `JobStatusChanged` | Job state machine transitions | Target status: `in_progress`, `completed`, `failed`, `stuck` | No |

### Context Management Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `PreCompact` | Before context compaction/compression | Trigger: `auto`, `manual` | No |
| `PostCompact` | After context compaction/compression | Trigger: `auto`, `manual` | No |

### Cost Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `CostLimitReached` | Spending limit hit | — (always fires) | No |

### Async / Standalone Events

| Event | Description | Matcher Target | Can Block |
|-------|-------------|----------------|-----------|
| `Notification` | Notification sent to user/channel | Notification type | Yes — suppresses delivery |
| `InstructionsLoaded` | Identity files (SOUL.md, AGENTS.md, etc.) loaded | Load reason: `startup`, `reload`, `hot_swap` | No |
| `ConfigChange` | Configuration or workspace settings change | Config source | Yes — prevents config reload |
| `FileChanged` | Watched workspace file changes | — (always fires) | No |
| `SkillReloaded` | Skill hot-reloaded, loaded, or unloaded | Skill name | Yes — prevents skill reload |
| `ChannelStatusChanged` | Channel connects or disconnects (connect/disconnect, not start/stop — WS sidecars own their own lifecycle) | Channel type | No |

### Hook Lifecycle Diagram

```
SessionStart
  │
  ┌────────────────────────────────────────────────┐
  │  Per-Turn Loop                                 │
  │  ├─ UserPromptSubmit                           │
  │  ├─ PreMessage                                 │
  │  │  ┌──────────────────────────────────────┐   │
  │  │  │  Agent Loop                          │   │
  │  │  │  ├─ PreLLMCall                       │   │
  │  │  │  ├─ PostLLMCall                      │   │
  │  │  │  ├─ PreToolUse                       │   │
  │  │  │  ├─ PermissionRequest                │   │
  │  │  │  ├─ PermissionDenied                 │   │
  │  │  │  ├─ PostToolUse / PostToolUseFailure │   │
  │  │  │  ├─ SubagentStart                    │   │
  │  │  │  ├─ SubagentStop                     │   │
  │  │  │  ├─ TaskCreated                      │   │
  │  │  │  └─ TaskCompleted                    │   │
  │  │  └──────────────────────────────────────┘   │
  │  ├─ PostMessage                                │
  │  ├─ PreResponse                                │
  │  ├─ PostResponse                               │
  │  ├─ Stop / StopFailure                         │
  │  └─ TeammateIdle                               │
  └────────────────────────────────────────────────┘
  │
  PreCompact / PostCompact
  │
  Async Events (fire independently):
  ├─ InstructionsLoaded
  ├─ ConfigChange
  ├─ FileChanged
  ├─ SkillReloaded
  ├─ ChannelStatusChanged
  ├─ Notification
  ├─ CostLimitReached
  └─ JobStatusChanged
  │
SessionEnd
```

## Design Decisions

### Why 31 hook points

The hook point set covers the full execution lifecycle — not just the happy path:

- **Session boundary** (`SessionStart`, `SessionEnd`): initialization and cleanup
- **User input boundary** (`UserPromptSubmit`): prompt filtering and rewriting before it enters the agent loop, distinct from `PreMessage` which fires after routing
- **Message lifecycle** (`PreMessage`, `PostMessage`): metadata injection and post-processing
- **Turn termination** (`Stop`, `StopFailure`): extensions need to react to (or prevent) agent stop transitions, and observe failures
- **LLM lifecycle** (`PreLLMCall`, `PostLLMCall`): context injection, metrics, quality checks
- **Tool lifecycle** (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`): extensions must observe both successes and failures
- **Permission lifecycle** (`PermissionRequest`, `PermissionDenied`): security hooks need pre-authorization interception, not just pre-execution
- **Response delivery** (`PreResponse`, `PostResponse`): response wrapping, leak detection, delivery metrics
- **Subagent lifecycle** (`SubagentStart`, `SubagentStop`): child actors need the same observability as the parent
- **Task/job lifecycle** (`TaskCreated`, `TaskCompleted`, `TeammateIdle`, `JobStatusChanged`): external dashboards and workflow engines need task-level hooks
- **Context management** (`PreCompact`, `PostCompact`): observe or influence compression behavior
- **Cost** (`CostLimitReached`): operational alerting
- **System events** (`Notification`, `InstructionsLoaded`, `ConfigChange`, `FileChanged`, `SkillReloaded`, `ChannelStatusChanged`): async events beyond the agent loop

### Matcher system

Hooks fire conditionally based on a matcher. Each event defines a matcher target (see tables above). Events marked "—" always fire regardless of matcher.

**Matcher evaluation rules:**

| Pattern Type | Evaluation | Example |
|:--|:--|:--|
| `*`, `""`, or omitted | Match all | Fires on every occurrence of the event |
| Only alphanumeric + `_` + `\|` | Exact string or `\|`-separated list | `Bash` or `Edit\|Write` |
| Any other character | Regex | `^ext_.*` or `builtin__file.*` |

**Tool name matching**: Built-in tools use their registered name (e.g. `file_read`, `bash`).

**Why matchers live on the hook, not on the manager**: Each hook knows its own scope. The manager triggers all hooks for a `HookPoint`, and each hook's matcher decides whether it actually fires. This keeps the manager simple and the filtering logic co-located with the hook definition.

### Three hook handler types

| Type | Description | Isolation | Use Case |
|------|-------------|-----------|----------|
| **Trait** | Rust `Hook` trait implementation | In-process | Built-in extensions, maximum performance |
| **Command** | Shell command execution | Process-level | External scripts, workspace-local hooks |
| **HTTP** | Webhook POST to URL | Network | External services, dashboards, audit systems |

Trait hooks are the primary handler type and the only type implemented directly. Command and HTTP handlers are adapter structs that implement the `Hook` trait internally:

- **Command adapter**: Spawns shell process, writes `HookInput` JSON to stdin, reads `HookOutput` JSON from stdout, maps exit code to `HookAction`
- **HTTP adapter**: POSTs `HookInput` JSON to URL, parses response body as `HookOutput`, maps HTTP status to `HookAction`

### Four-action model

| Action | Effect |
|--------|--------|
| `Continue` | No changes, proceed to the next hook |
| `ContinueWith(HookModification)` | Apply field-level modifications to the context, then continue |
| `Block(String)` | Prevent the current action with a reason (event-specific, see tables above) |
| `Abort(String)` | Stop the entire hook chain and escalate error to caller |

**Block vs Abort**: `Block` prevents only the specific action (e.g. blocks a single tool call; the agent loop continues with a denial). `Abort` halts the entire execution flow. Events that do not support blocking treat `Block` as `Continue` with a warning logged.

**Merge semantics**: `ContinueWith` modifications are shallow-merged by field. Only `Some` fields overwrite; `extra` map entries are merged. This prevents one hook from accidentally erasing context written by another.

### Hook input protocol

All hooks receive a `HookInput` structure. Common fields present for every event:

```
session_id: String
user_id: Option<String>
hook_event_name: String        // e.g. "PreToolUse"
matcher_target: Option<String> // the value the matcher was evaluated against
extra: HashMap<String, Value>
```

Event-specific fields are carried in an `event_data` enum. Each variant contains only the fields relevant to that event:

| Event Group | Key Fields |
|-------------|-----------|
| Tool events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `PermissionDenied`) | `tool_name`, `tool_input` (tool-specific JSON), `tool_use_id` |
| `PostToolUse` additionally | `tool_response` |
| `PostToolUseFailure` additionally | `error`, `is_interrupt` |
| `PermissionDenied` additionally | `reason` |
| `UserPromptSubmit` | `prompt` (raw prompt text) |
| `StopFailure` | `error_type`, `error_message` |
| `PreLLMCall` / `PostLLMCall` | `provider_name`, `model_id`, `token_usage` (PostLLMCall only) |
| `SubagentStart` / `SubagentStop` | `agent_id`, `agent_type` |
| `SubagentStop` additionally | `last_message` |
| `TaskCreated` / `TaskCompleted` | `task_id`, `task_subject`, `task_description` |
| `TeammateIdle` | `agent_id`, `agent_type` |
| `PreCompact` / `PostCompact` | `trigger` (`auto`/`manual`), `token_stats` |
| `InstructionsLoaded` | `file_path`, `load_reason` |
| `ConfigChange` | `config_source`, `changed_keys` |
| `FileChanged` | `file_path`, `change_type` (`create`/`modify`/`delete`) |
| `SkillReloaded` | `skill_name`, `skill_version`, `action` (`load`/`unload`/`update`) |
| `ChannelStatusChanged` | `channel_type`, `old_status`, `new_status` |
| `Notification` | `message`, `notification_type` |
| `JobStatusChanged` | `job_id`, `old_status`, `new_status`, `operation_kind` |
| `CostLimitReached` | `current_cost`, `limit`, `session_id` |
| `SessionStart` | `start_method` (`new`/`resume`/`clear`) |
| `SessionEnd` | `exit_reason` (`normal`/`timeout`/`error`/`clear`) |

### Hook output protocol (command / HTTP handlers)

For external (non-trait) handlers, the exit code or HTTP status determines the base action:

| Exit Code / HTTP Status | Meaning | JSON Parsed | Effect |
|:--|:--|:--|:--|
| 0 / 2xx | Success | Yes | Action proceeds; JSON fields control behavior |
| 2 / 422 | Blocking error | No | Block action (event-specific); stderr/body shown to agent |
| Other / 5xx | Non-blocking error | No | Execution continues; error logged |

JSON output fields (`HookOutput`):

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `continue` | bool | `true` | If `false`, abort entire agent loop |
| `stop_reason` | Option\<String\> | — | Reason when stopping (for logging, not shown to agent) |
| `decision` | Option\<String\> | — | `"allow"`, `"block"`, `"deny"` — event-specific action |
| `reason` | Option\<String\> | — | Explanation for decision |
| `additional_context` | Option\<String\> | — | Extra context injected into agent conversation |
| `system_message` | Option\<String\> | — | Warning shown to user |
| `suppress_output` | bool | `false` | Omit stdout from debug log |
| `hook_specific_output` | Option\<Value\> | — | Event-specific structured output (see below) |

**Event-specific output fields** (via `hook_specific_output`):

| Event | Fields |
|-------|--------|
| `PreToolUse` | `permission_decision` (`allow`/`deny`), `permission_decision_reason`, `updated_input` (modify tool args before execution) |
| `PermissionRequest` | `decision` (`allow`/`deny`), `updated_permissions` |
| `PermissionDenied` | `retry: bool` |
| `PostToolUse` | `additional_context` |
| `PostToolUseFailure` | `additional_context` |
| `UserPromptSubmit` | `additional_context` |
| `SessionStart` | `additional_context` |
| `SubagentStart` | `additional_context` |
| `PreCompact` | `strategy_override` (override compression strategy name) |
| `Stop`, `SubagentStop` | (no specific fields; use top-level `decision`) |

Output injected into context (`additional_context`, `system_message`) is capped at **10,000 characters**. Exceeding content is truncated with a notice.

### Serial execution model

Hooks for the same point execute serially in registration order (not parallel) because:

- A later hook may depend on changes from an earlier hook
- Hooks are often used for auditing/interception where order carries meaning
- Parallel execution introduces merge conflicts between modifications

### Decision precedence

When multiple hooks return decisions for the same event, the most restrictive decision wins:

`Abort` > `Block` > `ContinueWith` > `Continue`

Once a hook returns `Abort`, no further hooks execute. `Block` stops the action but remaining hooks still fire (for observability). Multiple `ContinueWith` modifications are merged in order.

### Critical vs non-critical hooks

Critical hook failure aborts the main flow; non-critical hook failure is logged but does not affect execution. Determined by metadata at registration time.

### Async hooks

Command and HTTP hooks can be marked `async: true`. Async hooks:

- Run in a background tokio task without blocking the main flow
- Cannot return `Block`, `ContinueWith`, or `Abort` actions (only side effects)
- Suitable for logging, notifications, metrics, and external service calls
- Failures are logged but never affect execution

### Hook timeout

Every hook handler has a configurable timeout:

| Handler Type | Default Timeout |
|-------------|----------------|
| Trait | 500 ms |
| Command | 30 seconds |
| HTTP | 30 seconds |

Timeout triggers the failure path — non-critical hooks log a warning and continue; critical hooks abort the flow.

Trait hooks default tight (500 ms) so the per-hook degrade counter (3 consecutive timeouts → auto-disable) can fire before the agent-loop step-boundary chain timeout (3 s) masks it. Hooks that genuinely need longer should override `Hook::timeout()`.

### Hook configuration sources

Hooks can be registered from multiple sources with different scopes:

| Source | Scope | Priority | Example |
|--------|-------|----------|---------|
| Built-in (Rust trait) | Always active | Highest | Security gateway, leak detection |
| Admin policy | Organization-wide | High | Compliance audit hooks |
| Workspace config | Per-workspace | Medium | AGENTS.md hook declarations |
| Skill frontmatter | While skill is active | Medium | Skill-specific pre/post hooks |
Higher-priority sources can override or disable lower-priority hooks. Admin policy hooks cannot be overridden by workspace hooks.

## Typical Use Cases

| Event | Use Case |
|-------|----------|
| `SessionStart` | Initialize session state, inject environment |
| `SessionEnd` | Cleanup, final audit record |
| `UserPromptSubmit` | Input filtering, prompt rewriting, content policy |
| `PreMessage` | Attach audit labels, inject metadata |
| `PostMessage` | Post-processing analytics |
| `Stop` | Final response validation |
| `StopFailure` | Error alerting, recovery decision |
| `PreLLMCall` | Inject extra context, request metrics |
| `PostLLMCall` | Response quality checks, cost tracking |
| `PreToolUse` | Permission checks, argument sanitization |
| `PostToolUse` | Business audit logs, result validation |
| `PostToolUseFailure` | Error classification, retry decisions |
| `PermissionRequest` | Auto-approve/deny based on policy |
| `PermissionDenied` | Alert on denied operations |
| `PreResponse` | Uniform response wrapping, leak detection |
| `PostResponse` | Delivery confirmation, channel metrics |
| `SubagentStart` / `SubagentStop` | Child agent observability |
| `TaskCreated` / `TaskCompleted` | External task tracker sync |
| `TeammateIdle` | Agent pool management |
| `JobStatusChanged` | External dashboard sync |
| `PreCompact` / `PostCompact` | Compression observability, strategy override |
| `CostLimitReached` | Operational alerting, graceful degradation |
| `Notification` | Notification routing, suppression |
| `InstructionsLoaded` | Identity change auditing |
| `ConfigChange` | Config change auditing, validation |
| `FileChanged` | Workspace file watch triggers |
| `SkillReloaded` | Skill governance, version auditing |
| `ChannelStatusChanged` | Channel health monitoring |

## Constraints

- Depends only on `channels`
- `HookContext.extra` must not contain sensitive plaintext
- Hook execution must have timeout protection to prevent external extensions from blocking the main flow
- Command/HTTP hooks run in a separate process/request
- Hook output injected into context is capped at 10,000 characters
- Async hooks cannot modify context or block actions

## Collaboration

| Module | Role | Status |
|--------|------|--------|
| `agent` | `AgentActor` triggers `PreMessage` and `PreResponse` hooks | Implemented |
| `agent` | `ToolExecutor` triggers `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `PermissionDenied` | Hook points defined, not yet triggered in code |
| `agent` | `Router` triggers `UserPromptSubmit` before message enters actor | Hook point defined, not yet triggered in code |
| `context` | `ContextManager` triggers `PreCompact` / `PostCompact` around compression | Hook points defined, not yet triggered in code |
| `job` | `JobManager` triggers `JobStatusChanged` after state transitions | Hook point defined, not yet triggered in code |
| `cost` | `CostGuard` triggers `CostLimitReached` on spending limit hits | Hook point defined, not yet triggered in code |
| `channels` | `ChannelRegistry` triggers `ChannelStatusChanged` on channel register/unregister (connect/disconnect); channel delivery triggers `PreResponse` / `PostResponse` | Hook points defined, not yet triggered in code |
| `skills` | `SkillRegistry` triggers `SkillReloaded` on hot reload | Hook point defined, not yet triggered in code |
| `workspace` | `WorkspaceManager` triggers `InstructionsLoaded` on identity file load | Hook point defined, not yet triggered in code |
