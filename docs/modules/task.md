# task — The Session Planning Checklist (`Task*`)

## Overview

The `task` crate (`baybo-task`) owns the planning-checklist tool family the LLM
uses to lay out and track a multi-step plan: **`TaskCreate`**, **`TaskGet`**,
**`TaskList`**, and **`TaskUpdate`**. It mirrors
`baybo-cron` / `baybo-skills` / `baybo-subagent` — a domain crate that hosts its
own `Tool` impls and depends on `baybo-tools` for the trait; `baybo-tools` never
depends back.

Modeled on Claude Code's `Task*` tools and Codex's `update_plan`: a small status
enum (`pending → in_progress → completed`; abandoning a task is a deletion via
`TaskUpdate(status: "deleted")`, not a status), a brief `subject` + a
`description` body per task, an at-most-one-`in_progress` convention surfaced in
the descriptions, and the list re-injected into the model's context as a
throttled periodic reminder (a nudge once the model has ignored task management
for a while) rather than on every turn.

The layering follows the store/storage split the rest of the schema uses:

- **`baybo-model`** — the value types (`Task`, `TaskStatus`, `TaskId`) plus the
  tool-name consts (`TASK_CREATE_TOOL_NAME`, …, `TASK_MUTATING_TOOL_NAMES`).
  Pure data, shared one-way like `PendingBackgroundResult`.
- **`baybo-store`** — the `TaskStore` trait + `TaskPatch` (the ports contract).
- **`baybo-storage`** — `SqliteTaskStore` over the dedicated `session_tasks`
  table.
- **`baybo-task`** — the `Tool` impls + the `tools::agent_tools` factory + a
  `MemoryTaskStore` test fixture behind `#[cfg(any(test, feature = "test-support"))]`.

## Design Decisions

### A dedicated `session_tasks` table, not a `SessionState` field

A live checklist is mutated on every `TaskUpdate`, concurrently with the
full-blob writers on the `sessions` row (`SessionManager::touch` runs
`get → mutate → save` on every inbound message; the actor's `save` persists
`SessionState`). A blob `Vec<Task>` would lose updates to that clobber race. A
single flat column (the `last_llm` / `hidden` trick) has the right
anti-clobber discipline but the wrong shape for a per-row-updated collection.
The dedicated table gives the same property at **row** granularity: each
`TaskUpdate` is `UPDATE session_tasks SET … WHERE session_id=? AND task_id=?`,
so concurrent updates to different tasks (or `touch` racing a task write) never
collide. Rows are reaped by `ON DELETE CASCADE` from `sessions`; the runtime
never sweeps them (session data is core data).

### Tools write directly through `TaskStore` — no actor round-trip

Unlike `SessionState`, the `session_tasks` table is shared state, not actor-owned
`&mut`. Per-row writes are atomic in sqlite, so a tool writes straight from
`execute()` even though tool calls run concurrently under `join_all` and a REST
handler could write while the actor is mid-turn. This sidesteps the
single-writer `mpsc` dance `spawn_subagent` needs (that exists only because
spawning *creates* an actor + session, which only the router can do). Each tool
holds an `Arc<dyn TaskStore>` as a constructor field; `tools::agent_tools` is
the factory the runtime calls (mirrors `baybo_cron::tools::agent_tools`). All
tools are `TrustLevel::Trusted` with **no** capabilities — they touch
agent-internal state, not the host FS/network, so the approval gate is a no-op.

Deletion mirrors Claude Code: there is no separate delete tool and no
`cancelled` status — `TaskUpdate(status: "deleted")` is an action that issues a
single-row `DELETE` (the only per-task `DELETE` path) instead of setting a
stored status. The three stored statuses are `pending` / `in_progress` /
`completed`. Rows are otherwise reaped only by the `sessions` cascade — never a
session or transcript row.

`TaskCreate` resolves dependencies inside one batch through caller-chosen
`key` / `depends_on_keys` names. Real `depends_on` values are only task ULIDs
returned by an earlier tool result. The implementation mints every new ULID,
builds the batch-key map, then resolves dependencies before writing any row;
the previous claim that `depends_on` could name another new task was impossible
because the caller could not know a random id that had not been returned yet.
The response includes `created_by_key` so subsequent calls can cross the batch
boundary using real ids.

Optional update fields carry explicit strict-schema no-op values. `unchanged`
means no status write, blank subject/description strings are ignored, and an
empty `depends_on` array is filler rather than a destructive clear.
`clear_depends_on: true` is the sole explicit clear operation. `TaskList`
similarly accepts `status: "all"` as its unfiltered spelling.

### Re-injection (throttled) + the live web surface

The agent loop owns the model-facing and user-facing surfacing (see
[`agent.md`](agent.md)):

- It loads the list at the start of each turn (and after any iteration that ran
  a mutating `Task*` tool, `TASK_MUTATING_TOOL_NAMES`) and calls
  `ContextManager::refresh_task_reminder`, which renders a transient
  `<system-reminder>` (`baybo_context::prompts::tasks::render_task_list`) kept
  **out** of the stored transcript. When injected it rides the tail of the LLM
  request and survives compaction for free.
- The model-facing reminder is **throttled** (mirrors Claude Code's
  `TODO_REMINDER_CONFIG`): it injects only once the model has gone
  `TURNS_SINCE_WRITE` (10) turns without managing tasks AND it has been
  `TURNS_BETWEEN_REMINDERS` (10) turns since the last reminder — a periodic nudge
  rather than riding every request. The throttle counters live on `AgentLoop`
  (`should_inject_task_reminder`); the model still sees the list through tool
  results between nudges.
- It emits `AgentEvent::TaskList(Vec<Task>)` on the same trigger; the gateway
  adapter maps it to `Frame::TaskList { tasks: Vec<TaskView> }`, which the web
  dashboard renders as a live checklist. Surfaces without a checklist (TUI, the
  one-shot CLI) drop it.

### The background half stays stubbed

`TaskStop` / `TaskOutput` operate on already-running background `spawn_subagent`
work, which has no durable model yet. They remain `NotImplemented` stubs in
`baybo_tools::builtin::todo`; only the planning half is implemented here.

## Constraints

- Internal deps: `baybo-model` (value types + tool-name consts), `baybo-store`
  (the `TaskStore` trait), `baybo-tools` (the `Tool` trait). **No** dependency on
  `baybo-agent` / `baybo-context` / `baybo-storage` — those depend on the contracts,
  never the reverse.
- The crate is pure tool logic; it persists nothing itself. `MemoryTaskStore`
  is `test-support`-gated so it never ships in release builds.
- `created_at` is stamped `base + index µs` within a `TaskCreate` batch so the
  list view preserves the order the model wrote (the ULID tie-break alone is
  random within a millisecond).

## Collaboration

| Module | Role |
|--------|------|
| `model` | Owns `Task` / `TaskStatus` / `TaskId` + the `TASK_*_TOOL_NAME` consts |
| `store` | Owns the `TaskStore` trait + `TaskPatch` |
| `storage` | `SqliteTaskStore` + the `session_tasks` DDL; `task` field on the `Store` bundle |
| `agent` | `crates/baybo/src/runtime.rs` registers `baybo_task::tools::agent_tools(stores.task)`; `AgentLoop` holds the `Arc<dyn TaskStore>`, refreshes the per-turn reminder, and emits `AgentEvent::TaskList` |
| `context` | `ContextManager::refresh_task_reminder` + `prompts::tasks::render_task_list` render the transient reminder |
| `channels` | `AgentEvent::TaskList` + the `Frame::TaskList` / `TaskView` wire types |
| `gateway` | The channel adapter maps `AgentEvent::TaskList → Frame::TaskList`; the web dashboard renders the checklist |
