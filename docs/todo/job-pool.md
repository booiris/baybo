# Execution Pool — Unified Job Admission for Subagents & Detached Commands

**Status:** Phases 1–3 **built** on branch `job-pool-design` (commits
`7628a35f`, `5b1104dc`, `627aed27`), atop merged background-jobs (#80).
Only the explicitly-deferred design extensions remain (see Deferred / open).

## As built (vs the proposal below)

The shipped design is leaner than the original "single `JobPool` front door"
sketch — the same value with less churn:

- **`SubagentSpawner` is a trait in `aura-subagent`** (leaf crate, no cycle);
  the actor-backed `ActorSubagentSpawner` impl is in `aura-agent`. The tool
  holds an `Arc<OnceLock<Arc<dyn SubagentSpawner>>>` slot, filled in
  `wire_router`. `SystemSpawnRequest` + the router arm are deleted. **One
  divergence:** `spawn()` is a single method (not split fg/bg) and keeps a
  *private* oneshot internally to bridge the convertible-foreground wait —
  the cross-actor channel/oneshot are gone, but it didn't fully vanish.
- **No unified `JobPool` god-component / `ctx.jobs`.** Instead a focused
  `JobBudget` (`runtime/job_budget.rs`, an `Arc<Semaphore>`) is shared
  between the spawner (acquires for background subagents) and the existing
  `BackgroundJobManager` (reports `(running, total)` in `JobList`). The
  subagent tool keeps its spawner slot; bash keeps `BackgroundJobSink`. The
  `SubagentDispatchLimiter` (foreground reject-cap) is *not* absorbed — it
  stays as-is.
- **Budget gates by holding the prompt, not deferring the build.** A
  background child's actor is built but parks on its mailbox doing no LLM
  work until it's fed `SubagentSpawned`; the spawner holds that prompt behind
  `budget.acquire()`. The `Semaphore`'s FIFO wait *is* the queue (no separate
  structure / admit worker).
- **Config knob is `agent.max_concurrent_background_jobs`** (default 8,
  validated, restart-only) — not `jobs.max_concurrent`.
- **Deferred:** the `priority: i8` hook (queue is plain FIFO), per-root
  fairness, durable queue, true command queueing — see Deferred / open.

The original proposal follows for rationale.

## Motivation

After background-jobs, Aura has two different "tool → runtime async work"
mechanisms:

- **Subagent** spawn rides `system_spawn_tx: mpsc<SystemSpawnRequest>` — an
  envelope enum consumed by the Router's `select!` loop, replying via a
  `oneshot`.
- **Detached bash** rides `ctx.background_jobs: Arc<dyn BackgroundJobSink>` — a
  ToolContext capability whose impl (`BackgroundJobManager`) owns an escort task.

The unified *registry* already exists (`AgentSupervisor::in_flight_background_subagents`
holds both kinds; `JobList`/`JobStop` query it uniformly). What's split is the
*spawn entry*, and neither mechanism has a real concurrency budget: subagents
have only a per-root fan-out reject-cap (`SubagentDispatchLimiter`), detached
commands have nothing.

This proposal makes the pool a real admission controller — a global concurrency
budget + a queue + scheduling — and collapses the two spawn entries into one,
**deleting `SystemSpawnRequest`** by extracting the Router's subagent-spawn
logic into a standalone `SubagentSpawner` service.

## Resolved decisions (grill outcomes)

| Axis | Decision |
|---|---|
| Goal | A real pool: global concurrency cap + queue + scheduling + (later) a jobs dashboard. Not cosmetic unification. |
| Full-budget behavior | **Queue, background jobs only.** Fresh background dispatches wait for a slot. Foreground subagents keep the synchronous per-root **reject**-cap (never queued — the caller is blocking). |
| Budget scope | **Global** single budget (`jobs.max_concurrent`). Per-root fairness deferred. |
| Spawn entry | **Delete `SystemSpawnRequest`.** Extract `SubagentSpawner`; foreground calls it directly, background goes through the pool. |
| Queue durability | **In-memory.** A queued-not-started job lost on restart == never dispatched (matches best-effort background results; the group barrier has its 30-min fallback). |
| Scheduling | **v1 FIFO** + a `priority: i8` hook (default 0; queue ordered by `(priority desc, enqueue seq)`). |
| Dashboard | `JobList`/`JobStop` already give programmatic visibility. WebUI budget gauge + cross-session list = fast-follow (2nd PR). |

### Correctness nuance — commands can't be queued

A detached bash command is *already running* by the time it detaches: bash runs
foreground and only converts to background **on timeout** (`on_timeout:
background`). There is no "dispatch a command straight to the background" mode.
So a command can't wait in a queue before starting — it has already been
consuming resources for `timeout` seconds.

Therefore the **queue/budget gates fresh background *subagent* dispatches**
(the thing that actually fans out unboundedly — an LLM can fire 20 subagents).
**Commands are tracked + counted in the pool for `JobList`/`JobStop`/visibility
but admitted-on-detach** (never queued). If the budget is full when a command
detaches, it still proceeds (killing an already-running process loses work);
it's counted toward the displayed total but not hard-gated.

True command queueing would require a new "start in background from the start"
bash mode — deferred (see Open).

## Why deleting the channel is safe (code findings)

Earlier worry: subagent spawn is deeply Router-entangled. Investigation says
otherwise:

- Actor construction is **already** behind `ActorSpawner` (`router/mod.rs:127`),
  a `Box<dyn Fn(Session, …) -> MailboxSender>`. The Router just holds one; it
  doesn't inline the actor build.
- The closure (`src/runtime.rs:768`) ends with `tokio::spawn(actor.run(mailbox))`
  and returns only the mailbox sender. **The subagent's agent loop self-drives
  on its own task — the Router never drove it.** Removing the Router changes
  nothing about how a subagent runs.
- The spawn path mutates **no** Router-exclusive state — the `&mut self` on
  `handle_subagent_spawn` / `resolve_child_session` is vestigial (they only call
  `Arc<SessionManager>`).
- Every spawn dep is already `Arc<…>` or `#[derive(Clone)]`: `session_manager`,
  `job_lifecycle`, `llm_pool`, `dispatch_limiter`, `channels`, `external_agents`,
  `workspace_paths`, `AgentSupervisor`.
- `escort_background_terminal` / `deliver_background_result` are already free
  functions (no `self`).

So the spawn logic lifts cleanly into a service holding the same handles.

## Architecture

Three pieces.

### 1. `SubagentSpawner` (new — `crates/agent/src/runtime/subagent_spawner.rs`)

Holds: `Arc<ActorSpawner>` (was `Box`; see Wiring), `Arc<SessionManager>`,
`Arc<JobLifecycle>`, `LlmPoolHandle`, `Arc<dyn SubagentDispatchLimiter>`,
`Arc<ExternalAgentRegistry>`, `Arc<WorkspacePaths>`, the supervisor,
`actor_parent_token`.

Absorbs (lifted verbatim from `router/system_spawn/subagent.rs`):
`resolve_child_session`, `spawn_aura_subagent`, `spawn_external_subagent`,
`await_subagent_terminal`, `escort_background_terminal`,
`deliver_background_result`, `release_fan_out_slot`.

```rust
// Foreground: awaits the child terminal, returns the result directly.
// The oneshot disappears — it only existed to bridge the channel.
async fn spawn_foreground(&self, parent: SubagentParentContext, req: SubagentSpawnRequest) -> SubagentResult;

// Background: spawns, returns the bg handle immediately; the wait+escort
// runs on a detached task that routes the terminal to the parent mailbox
// as a BackgroundJobFinished (today's path).
async fn spawn_background(&self, parent: SubagentParentContext, req: SubagentSpawnRequest) -> BackgroundHandle;
```

### 2. `JobPool` (grow `BackgroundJobManager` — `runtime/background_jobs.rs`)

Adds a global `tokio::sync::Semaphore` (the budget) + an in-memory priority
queue + an admit worker. Holds the `SubagentSpawner` and absorbs
`SubagentDispatchLimiter` (one concurrency home).

- **Background subagent**: enqueue → admit (permit) → `spawner.spawn_background(…)`
  → release on terminal.
- **Foreground subagent**: pass-through — check the per-root reject-cap, then
  `spawner.spawn_foreground(…)`; no permit (not queued).
- **Detached command**: register the already-running `DetachedCommand` (today's
  `detach_command`), escort + count; no permit (see nuance above).

Single tool-facing capability: **`ctx.jobs: Arc<dyn JobPool>`** replacing
today's `system_spawn_tx` (subagent tool) and
`background_jobs`/`background_control` (bash + `JobList`/`JobStop`).

### 3. Registry (extend `AgentSupervisor::in_flight_background_subagents`)

Add `state: Queued | Running` to the in-flight entry. `JobList` reports state +
queue position + live budget usage; `JobStop` drops a queued job from the queue
(it never spawns) or cancels a running one (today's path).

## The three flows

```
foreground subagent:  tool ─▶ ctx.jobs.spawn_fg ─▶ [reject-cap] ─▶ spawner.spawn_foreground ─▶ SubagentResult
background subagent:   tool ─▶ ctx.jobs.dispatch_bg ─▶ enqueue ─▶ [permit] ─▶ spawner.spawn_background ─▶ ack(handle); escort↴
detached command:      bash ─▶ ctx.jobs.register_cmd ─▶ (already running) escort + count ─▶ ack(handle); escort↴
                                                                 (release permit on subagent terminal)
```

## What gets deleted

- `aura_model::SystemSpawnRequest` (the channel envelope enum). `SubagentParentContext`
  stays — it becomes the spawner's argument.
- `Router::system_trigger_rx` field + the `select!` arm + `handle_system_spawn` +
  the whole `router/system_spawn/` module (bodies move to `SubagentSpawner`).
- `RouterConfig::system_trigger_rx` + the `system_spawn_rx` plumbing in
  `runtime.rs`.
- The subagent tool's `system_spawn_tx` constructor field + the `oneshot` dance.

The Router keeps `actor_spawner` for top-level user-session + cron spawns (still
routed through it).

## Wiring / bootstrap

- `ActorSpawner`: `Box<dyn Fn>` → **`Arc<dyn Fn>`** so both the Router
  (user/cron) and the `SubagentSpawner` can hold the one closure.
- `SubagentSpawner` + `JobPool` are built in `runtime.rs` where the
  `spawn_actor_for` closure + supervisor already are (after them). The tool
  executor is wired earlier (`build_managers`), so the pool is injected with
  **late-set `OnceLock` slots** for `{ supervisor, actor_spawner }` — the same
  pattern `BackgroundJobManager` already uses for the supervisor. Slots are
  filled once the closure + supervisor exist.

## Knobs & defaults

- `jobs.max_concurrent` (config): global background-subagent queue budget.
  Default **8**. A background subagent is LLM-heavy (own agent loop → cost +
  provider rate limits); tune down if subagent fan-out hits provider limits.
- `priority: i8` on a job spec, default 0. v1 ordering `(priority desc, enqueue seq)`.

## Phases

1. **Extract `SubagentSpawner`** — **DONE** (`7628a35f`): `Box`→`Arc`
   ActorSpawner; spawn bodies lifted into the service; tool calls it via an
   `Arc<OnceLock>` slot; `SystemSpawnRequest` + the router arm deleted. (The
   foreground oneshot became a private impl detail rather than vanishing.)
2. **Concurrency budget** — **DONE** (`5b1104dc`): `JobBudget` (`Semaphore`)
   gates background subagent dispatches (Aura + external) by holding the
   prompt; `max_concurrent_background_jobs` config; `Queued|Running` registry
   state + `mark_background_subagent_running`; `JobList` shows per-job state +
   `background_budget {running, total}`. Commands tracked-not-queued; the
   `SubagentDispatchLimiter` stays separate (not absorbed).
3. **WebUI dashboard** — **DONE** (`627aed27`): `GET /v1/background-jobs`
   (cross-session list + budget, backed by `list_all_in_flight_background`)
   and a `JobsPage` (budget gauge + auto-polling table, `/jobs` route +
   sidebar nav). Read-only — `JobStop` stays the agent-driven per-session
   tool; no admin cancel endpoint.

## Risks & test plan

- **Concurrency model**: spawns move off the Router's serialized loop onto
  tool/pool tasks. Safe — spawn touches only concurrent-safe state (DashMap
  supervisor, async store, atomic limiter). Add a test firing N concurrent
  foreground spawns.
- **Permit leak / starvation**: a permit must be released on every terminal path
  (success/fail/cancel/`/stop`). Reuse the existing release-on-terminal
  discipline (`note_*_finished`). Test: full budget → queued job → on terminal
  the queued job admits.
- **`JobStop` on a queued job**: drop from the queue (never spawns) AND ack the
  parent (no escorted result is coming). Test.
- **Bindings**: deleting `SystemSpawnRequest` ripples openapi→schema.d.ts→web TS
  if any variant leaked to bindings (subagent spawn is internal; verify
  `scripts/check-ts-bindings.sh`).

## Deferred / open

- Per-root fairness (sub-budgets or round-robin queue) — global-only for v1.
- Durable queue (survive restart) — in-memory for v1.
- True **command queueing** — needs a "dispatch to background from the start"
  bash mode; today's commands are already-running on detach.
- Weighted slots (subagent = N, command = 1) — flat 1-slot for v1.
- Priority policy beyond FIFO + hook (e.g. foreground-conversion or group
  members auto-bump priority).
