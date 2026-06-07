# Bash & Subagent Background Jobs — Timeout-to-Background + Subagent Groups

**Status:** implemented on branch `background-jobs`. All four phases built + tested
(`66c655b8`, `e0b2c840`, `425f108d`, `6d1d495d`), plus the `JobList`/`JobStop` tools
(`6a18eca2`) and boot-time output-file retention (`7d449eb1`). The Phase-3 streaming
question was resolved with a third option (two files, format-preserving) — see below.
Not yet merged to master. **All three detached-spawn backends are implemented:**
bwrap (Linux default), Docker (`48ac2f2a`, verified live against dockerd 29.4.1 via
`--ignored` tests), and sandbox-exec (`42d250cb`, macOS — the `DetachedChild` holds
the per-call scratch tempdir so it outlives the child; verified by cross-compiling,
`cargo clippy --target aarch64-apple-darwin`, since the file is `#![cfg(macos)]`).

## Goal

Two related capabilities that share one substrate:

1. **Timeout-to-background.** When a `Bash` command or a foreground
   `spawn_subagent` exceeds its foreground budget, don't kill it — detach it,
   keep it running, stream its output, and wake the parent agent with a
   notification when it finishes.
2. **Subagent groups.** A `group` parameter on `spawn_subagent` so a batch of
   parallel subagents delivers **one merged notification, only once every
   member has finished** (a barrier/join).

## Today

- **Bash** kills the child on timeout: internal `tokio::select!` over
  `child.wait()` vs `sleep(timeout)` → `start_kill()` → returns
  `ToolError::Timeout` (`crates/tools/src/builtin/bash.rs`, kill path ~1281-1290,
  `if out.timed_out` ~526-528). Default foreground budget is `max_timeout = 600s`
  (bash.rs:354-361), overridable per call via `timeout_ms`. Sandboxed is the
  **default** path (`sandbox.spawn_command`, bash.rs ~490-504); unsandboxed is
  only `aura` CLI calls + the bwrap-setup-failure retry.
- **Foreground subagent** has no real timeout: `max_timeout = TOOL_WAIT_BACKSTOP
  = 30 days` (`crates/subagent/src/tool.rs:131,479`); the tool just
  `result_rx.await`s until the child terminates (tool.rs ~558).
- **Subagent already has a background path**: `background: bool` (SpawnParams,
  tool.rs ~136-153) → `await_subagent_terminal` → `escort_background_terminal` →
  `AgentMessage::SubagentFinished(PendingSubagentResult)` → buffered on
  `session.state.pending_subagent_results` (dedup by `handle_id`, cap 64) →
  drained into one autonomous `SubagentNotification` turn
  (`crates/agent/src/actor/mod.rs`, `crates/agent/src/actor/router/system_spawn/subagent.rs`,
  `docs/modules/agent.md` §"Background subagent results").
- **Bash has no background path**; `MonitorTool` / `TaskStopTool` /
  `TaskOutputTool` are `todo_tool!` stubs (`crates/tools/src/builtin/todo.rs`).

So "timeout-to-background" means *"don't kill, detach"* for bash, but means
*"invent a foreground-wait threshold that doesn't exist yet"* for subagent.

## Scope gate — user-facing sessions only

Background execution is enabled **only in user-facing (UserChat) sessions**. In
cron sessions and nested subagent sessions, bash/subagent timeouts keep today's
behaviour (bash kills; subagent waits to backstop). This matches the existing
notification path, which is already UserChat-only (`docs/modules/agent.md`: cron
is out of scope because it's one-shot + unregistered, so `SubagentFinished` can't
reach it). The gate is realized for bash **by injection**: the runtime only
populates `ctx.background_jobs` (below) for user-facing sessions; for subagent the
router checks the session is user-facing before converting.

## Shared substrate

A background job is "something the agent spawned in the background; when it
finishes the agent should react." Subagent completion and command completion are
two kinds of the same event. We **generalize the existing subagent notification
machinery** rather than duplicate it.

- **`aura-model/spawn_protocol.rs`** — `PendingSubagentResult` →
  `PendingBackgroundResult { handle_id, label, summary_text, status, kind }`,
  with `enum BackgroundJobKind { Subagent { child_session_id, subagent_type },
  Command { command, exit_code, output_path } }`. `AgentMessage::SubagentFinished`
  → `BackgroundJobFinished(Box<PendingBackgroundResult>)`.
- **`aura-model/session.rs`** — `pending_subagent_results` →
  `pending_background_results` (add `#[serde(alias = "pending_subagent_results")]`
  so an in-flight buffer from an older binary still deserializes). New
  `background_groups: HashMap<String, GroupState>` (see Feature 2).
- **`aura-tools/lib.rs`** — new capability traits, injected via `ToolContext`,
  mirroring `ExecSandbox` / `SecretAccess` / `ToolEventSink`:
  - `trait BackgroundJobSink` — the tool hands a still-running child + output
    file + metadata to the runtime, which owns the escort and routing.
  - `trait RunningChild` — a live detached child exposing `take_stdout()`,
    `take_stderr()` (async readers), `wait()`, `start_kill()`.
  - `ToolContext.background_jobs: Option<Arc<dyn BackgroundJobSink>>` — `Some`
    only for user-facing sessions (this is the gate).
  - `ExecSandbox` gains a detached-spawn method returning a `RunningChild`; the
    run layer returns `Completed(SandboxedOutput) | StillRunning(RunningChild)`.
    One `RunningChild` impl wraps `tokio::process::Child` (unsandboxed), one wraps
    the bwrap child (sandboxed). Without this, backgrounding would only cover the
    rare unsandboxed path.
- **`aura-agent`** — runtime-side `BackgroundJobSink` impl + a command escort
  (analogue of `escort_background_terminal`): tee the child's readers to the
  output file, `wait()`, route `BackgroundJobFinished`. Supervisor in-flight
  tracking (`in_flight_background_subagents`) generalized to cover command jobs.
  `handle_subagent_finished` → `handle_background_finished`. The notification
  drain (`maybe_run_subagent_notification` / `run_subagent_notification`) gains a
  **group-readiness filter** (Feature 2). `ctx.background_jobs` populated in
  `runtime/tool_executor.rs` (where `ToolContext` is built ~495-515) only for
  user-facing sessions.
- **`aura-context/prompts`** — `build_notification_content` switches framing on
  `kind` (subagent report vs command exit + output tail/path).
- **`aura-sandbox`** — the detached-spawn implementation; adapted into
  `ExecSandbox` by aura-agent (so aura-tools keeps no sandbox dep).

Lifecycle for all background jobs mirrors the shipped subagent rules: anchor the
process to the **process-wide token** (not the tool's per-job
`ctx.cancellation_token`) so it outlives the dispatching turn; **pin the parent**
against the idle reaper while in flight; **`/stop` kills** in-flight jobs and
**suppresses** their notification (drain-doubles-as-suppress, as today); process
shutdown kills all; a concurrency cap bounds fan-out; the durable buffer survives
actor eviction within the process.

## Feature 1 — timeout-to-background

Trigger is **automatic on timeout, with a per-call opt-out**: a new enum
`on_timeout: "background" | "kill"` (default `background`) on both tools.
`kill` reproduces today's kill-on-timeout.

- **Bash.** Foreground budget = existing `timeout` (`timeout_ms` or 600s
  default). On expiry with `on_timeout=background` **and** a sink present (user
  session): the run layer returns `StillRunning`; bash hands `{ child, output
  file, command, parent ctx }` to the sink and returns text like *"running in
  background as `<job_id>` — partial output (tail): … — full output streaming to
  `<path>` — you'll be notified on completion."* With `on_timeout=kill` or no sink
  → today's `start_kill` + `ToolError::Timeout`. Output streams to a **real file**
  `logs/background/<job_id>.log` (capped); the agent fetches full output with
  `Read` (the file is live-growing, so Read works during and after). The internal
  budget (≤600s) fires before the executor's outer deadline
  (`ctx.timeout + APPROVAL_HEADROOM`, tool_executor.rs ~530), so conversion
  returns the handle well within the outer timeout.
- **Subagent.** Foreground wait = **fixed 2 minutes** (a const, no per-call
  knob). On expiry with `on_timeout=background`: the router converts — acks
  *"converted to background as `<handle>`"* on the foreground oneshot, then
  escorts the terminal via `BackgroundJobFinished`. `on_timeout=kill` → cancel the
  child + return an error. `background: true` is unchanged (immediate
  fire-and-forget). Mechanics: the router's terminal-watch future must be
  **pinned and resumed across the 2-minute `select!` boundary** so it is not
  consumed twice — `let mut term = await_subagent_terminal(...); select! { r =
  &mut term => foreground(r); _ = sleep(2min) => { ack_converted(); escort(term.await) } }`.

## Feature 2 — subagent groups (subagent-only, v1)

New `group: Option<String>` on `spawn_subagent`. A non-empty `group`:

- **forces background-from-start** (ignore `on_timeout` / the 2-min wait), so
  group members never deliver inline — every member's result is **barrier-held**
  and delivered together. (This also keeps groups orthogonal to timeout-conversion.)
- tags the member's pending result with `group`.

Barrier is **push** with **turn-end auto-sealing** (the agent only passes the
`group` string; no count, no close call):

- On a grouped spawn, add the member's `handle_id` to
  `background_groups[group]` (`sealed = false`).
- **At the end of the dispatching UserChat job** (a turn-end hook in the actor),
  seal every group that received members during that job and start a **30-minute**
  timer per sealed group. (Sealing at job-end precludes building a group across
  jobs — acceptable for "parallel" semantics.)
- A grouped member's `BackgroundJobFinished` records its terminal state; a member
  reaching **any** terminal state (success / failure / killed) counts as done. If
  the group is sealed **and** all members are terminal → drain that group → one
  merged notification (per-member status reported).
- **Group timeout (30 min from seal) → partial fire + dissolve.** If the timer
  fires before completion: emit one partial notification for the already-terminal
  members (only if ≥1 is terminal), then **dissolve** the group — each remaining
  member reverts to a normal individual background job and notifies on its own
  completion. Net: at most one partial notification + individual stragglers; no
  second group batch, no re-fire.

**Drain filter** (the key change to `run_subagent_notification`): hold
grouped-but-not-ready results; drain non-grouped results and ready/dissolved-group
results as today. Buffered grouped results live in `pending_background_results`
tagged by group until their group is ready or dissolves.

Why groups are background-only: a pure-foreground batch already returns all
results in the same turn (the loop `join_all`s the response's tool calls,
agent_loop.rs ~1106), so a barrier is meaningful only once members go background.

## Tooling

`JobList` (list/status) + `JobStop` (per-job kill) — **named `Job*`, not `Task*`**,
to avoid colliding with the planning-checklist `TaskCreate/Get/List/Update`
implemented in `aura-task`. `JobList` shows the **current session's** in-flight +
recently-completed background jobs (id / command / output path / group / state).
`JobStop` reuses the `/stop` suppression path (killed = terminal → unblocks a
group barrier; killed result is suppressed from notification). The `Monitor` /
`TaskOutput` stubs stay stubs — push notification + `Read` on the output file make
polling/streaming tools unnecessary for v1.

## Decisions (grill outcomes)

| Topic | Decision |
|------|----------|
| Notification | Generalize the subagent notification path into one background-job substrate; bash completion wakes the parent + output to file |
| Scope | UserChat sessions only (gate via `ctx.background_jobs` injection / router check) |
| Trigger | Auto-convert on timeout + per-call opt-out |
| Threshold | bash = existing `timeout` (600s default); subagent = fixed 2 min |
| Bash output | Real file `logs/background/<job_id>.log`, fetched via `Read` |
| Hand-off | `BackgroundJobSink` trait in aura-tools, `ToolContext.background_jobs: Option`, injected only for user sessions |
| Sandbox | Extend `ExecSandbox` with detached spawn → `RunningChild`; run layer `Completed | StillRunning` |
| Data model | Honest enum: `BackgroundJobFinished(PendingBackgroundResult{ kind: Subagent | Command })`; one buffer/drain/builder |
| `/stop` | Kills in-flight jobs + suppresses (mirror subagent). Token anchoring / reaper-pin / shutdown-kill / cap all mirror subagent |
| Tools | Add `JobList` + `JobStop` |
| Group delivery | All members background-from-start + barrier-held + one merged notification |
| Group sealing | Push barrier + turn-end auto-seal |
| Hung member | Group timeout fixed 30 min from seal → partial fire; barrier dissolves, stragglers → individual |
| Group scope | subagent-only (v1) |
| Opt-out flag | enum `on_timeout: background | kill` (default `background`) |

## Derived defaults (low-ceremony; revisit if needed)

- **Concurrency cap.** A separate per-session cap for background command jobs
  (start at 8), distinct from the subagent fan-out cap (8/root). A converted
  foreground subagent keeps its original fan-out slot.
- **Job id.** Reuse the protocol `handle_id` (UUID); name the output file by it.
- **Output retention.** Output files are not auto-deleted (the agent may `Read`
  them after the notification). A retention/cleanup policy is a later TODO, not v1.
- **Restart.** Background OS processes + escort tasks are **process-lifetime**;
  an aura restart orphans them (not resumed). The durable buffer only guarantees
  re-delivery after actor eviction *within* the same process. Acceptable.

## Implementation surface

`aura-tools` (lib.rs traits + ToolContext, builtin/bash.rs, builtin/todo.rs →
real `JobList`/`JobStop`), `aura-subagent` (tool.rs: `group`, `on_timeout`, 2-min
wait), `aura-model` (spawn_protocol.rs, session.rs), `aura-agent`
(actor/mod.rs drain + group-readiness + turn-end seal + `handle_background_finished`,
router/system_spawn/subagent.rs conversion, actor/subagent.rs terminal watch,
supervisor.rs in-flight tracking, runtime/tool_executor.rs injection, new
runtime/background_jobs.rs sink + command escort), `aura-context`
(prompts/subagent.rs notification framing), `aura-sandbox` (detached spawn) + the
`ExecSandbox` adapter in aura-agent.

## Phasing

1. ✅ **Protocol generalization** — DONE (commit `66c655b8`). `PendingSubagentResult`
   → `PendingBackgroundResult { handle_id, label, summary_text, status, kind, group }`,
   `SubagentFinished` → `BackgroundJobFinished`, buffer rename + serde alias,
   kind-aware `build_notification_content`. Pure refactor, no TS ripple.
2. ✅ **Subagent timeout-to-background** — DONE (commit `e0b2c840`). `on_timeout`
   enum param (background|kill), router-side conversion (pinned terminal future
   across a 2-min `select!`), process-token anchoring for convertible spawns,
   gated to user-facing parents via `parent_supports_background`. Tested.
3. ✅ **Bash timeout-to-background** — DONE (commit `425f108d`). `BackgroundJobSink`
   + `RunningChild` + `TokioRunningChild`, `ExecSandbox`/`SandboxRunner` detached
   spawn (bwrap; docker/sandbox-exec fall back to kill), `BackgroundJobManager`
   escort, per-job two-file output streaming, `on_timeout` param, user-facing gate
   via `Session::supports_background_jobs()`. `JobList`/`JobStop` deferred.
4. ✅ **Group barrier** — DONE (commit `6d1d495d`). `group` param (forces background),
   `SessionState.background_groups` + `GroupState`, agent-loop member counting,
   turn-end seal, `check_groups` release of complete/timed-out cohorts, 30-min
   partial-fire + dissolve, idle-loop timeout enforcement.

## Resolved: output streaming model — chose two files (format-preserving)

The dilemma below was resolved with a third option not in the original list:

**Two files (chosen).** The detached path streams stdout and stderr to *separate*
capped files (`logs/background/<id>.{out,err}`) via `tokio::io::copy`. A command that
completes in the foreground window reads both back → its result keeps the normal
separate `{exit_code, stdout, stderr}` shape (**no behaviour change**); one that
overruns hands the live child + the two copy-task handles to the escort. This avoids
both the merged-log behaviour change (A) and the fiddly reader-ownership transfer (B),
and streaming-to-disk (capped at 10 MiB/stream) keeps a long job's output unbounded by
memory while a runaway producer still gets EPIPE.

For the record, the rejected options were: (A) merged interleaved log — simplest but
changed the foreground result shape; (B) dual-stream in-flight handoff — preserved the
shape but needed reader-ownership transfer across the boundary; (C) detached only as a
fallback — impossible (the sandbox kills internally on timeout).

## Open items

- ✅ Output-file retention — DONE (`7d449eb1`): boot-time prune of
  `logs/background/*` older than 7 days.
- ✅ `JobList` / `JobStop` — DONE (`6a18eca2`).
- ✅ Docker detached backend — DONE (`48ac2f2a`, live-verified).
- ✅ sandbox-exec (macOS) detached backend — DONE (`42d250cb`, cross-compile-verified;
  runtime behaviour to confirm on a real macOS host).
- `JobStop` only reaches user-facing sessions (its `background_control` is gated
  like the sink); non-user sessions rely on `/stop`/shutdown. Likely leave as-is.
- A per-session concurrency cap on detached commands (currently unbounded beyond
  the OS) — revisit once real fan-out is observed.
- e2e coverage for the group barrier is at the `GroupState` predicate level
  (unit-tested); a full spawn→barrier→one-notification harness test is a nice add.
