# Background Subagent → SubagentNotification Turns + Message-Priority Mailbox

> Status: **implemented** on branch `aura-subagent-notify` (2026-05-23); pending review/commit.
> Replaced the prior "piggyback the result onto the next user turn" model. Designed via a
> review on 2026-05-22.

## Non-obvious decisions (do not "fix" these)

Each of these looks wrong at a glance and has a load-bearing reason — don't revert on intuition:

1. **`ActorStop` is the LOWEST priority, not the highest.** Highest would skip cron's
   back-to-back `CronTrigger`→stop (FIFO) and let a reaper stop jump ahead of a just-delivered
   `SubagentFinished`. "Stop now" is `/stop`'s cancel token (§7), never a mailbox tier.
2. **Automatic priority is queue-ordering only — NON-preemptive.** A running turn is never
   interrupted by a higher-priority arrival. The *only* explicit preemption is `/stop`.
3. **The notification framing (and any silence behaviour) lives in per-turn content, NEVER the
   system prompt.** Moving it into the system prompt changes the cached prefix and breaks the
   prompt cache. This is why the turn reuses the exact main-path system prompt + toolset.
4. **There is no `<no_output/>` sentinel.** The model is not told it may stay silent; we only
   suppress an **empty** final message. Don't re-introduce a silence sentinel.
5. **UserInput coalescing has NO debounce timer** (drain already-queued only). It deliberately
   does not batch rapid sends to an idle actor — accepted.
6. **Every leading-slash message is a hard merge boundary, not just `/compact`.** Needed so
   skill invocation (`/skill`) and future actor-level commands keep the slash at position 0.
7. **No cron-vs-`SubagentFinished` priority rule.** They can't coexist — a cron session is
   one-shot and unregistered, so `SubagentFinished` never reaches it.

## Problem

Today a `background: true` subagent's terminal result is delivered cheaply but passively:

- The wait task routes `AgentMessage::SubagentFinished` to the parent actor's mailbox
  (`crates/agent/src/actor/router/system_spawn/subagent.rs::deliver_background_result`).
- `handle_subagent_finished` buffers it into `session.state.pending_subagent_results`
  (dedup by `handle_id`, cap 64 drop-oldest) and persists.
- It is **never** acted on by itself. The **next** `UserInput` drains the buffer via
  `drain_pending_subagent_notice`, renders a bracket-text preamble
  (`BACKGROUND_NOTIFICATIONS_PREAMBLE`/`POSTAMBLE`), and injects it as a side context
  message on that user turn (`crates/agent/src/actor/mod.rs`).

So if no user message ever arrives, the parent never reacts to the finished work. We want
the parent to **autonomously react** when a background subagent finishes — run its own
agent-loop turn and proactively report to the user — while keeping a live user responsive
(user messages must not wait behind subagent reactions).

The actor's mailbox is a plain FIFO `mpsc::Receiver<AgentMessage>` processed one message at
a time (`crates/agent/src/actor/mod.rs::run`), with no notion of priority. Realising the
above needs a priority mailbox plus a new turn kind.

## Scope

- **Background subagents only.** Foreground subagents are synchronous (the parent blocks
  inside the `spawn_subagent` tool awaiting a oneshot) and never touch the mailbox /
  `SubagentFinished`.
- **Cron sessions are out of scope.** Each cron fire mints a fresh, one-shot session that
  is **not registered** with the supervisor (`crates/agent/src/actor/router/cron.rs`), so
  `supervisor.route(SubagentFinished)` can't reach it and the actor is gone by the time a
  background subagent finishes. (Pre-existing: a cron task that spawns a background subagent
  gets no follow-up — result lives only in the trace / child session. Not fixed here.)

## Design

### 1. Priority mailbox (replaces the plain `mpsc`)

Replace `mpsc::Receiver<AgentMessage>` with a custom priority queue (e.g.
`Arc<Mutex<BinaryHeap<…>>>` + `tokio::sync::Notify`). Priority is **intrinsic to the message
kind**, so senders (`supervisor.route`, the spawner, the cron path) don't pass a priority —
call sites barely change.

```
Tier 1 (FIFO within tier):  UserInput · CronTrigger · SubagentSpawned
Tier 2:                     SubagentFinished
Tier 3 (lowest):            ActorStop
```

The custom mailbox must re-implement three things `mpsc` gave for free:

1. **FIFO-within-tier** via a monotonic insertion sequence number as the heap tiebreaker.
2. **Bounded capacity + backpressure** (match the current configured mailbox capacity).
3. **Close-on-all-senders-dropped** → `recv` resolves to `None`. The cron one-shot relies on
   sender-drop to exit (`cron.rs`), so this must be correct or that actor hangs.

Within any single actor only one Tier-1 kind ever appears (a user session sees only
`UserInput`; a cron session only `CronTrigger`; a subagent child only `SubagentSpawned`),
so the cross-type priority decision that actually happens is always **trigger work vs
`SubagentFinished`**, and the responsiveness reason for it only exists on user sessions.

### 2. `Shutdown` → `ActorStop`, placed **lowest**

Rename `AgentMessage::Shutdown` → `ActorStop` — the session row never dies (it is core
data); only the in-memory actor stops. Place it at the **lowest** tier ("drain all real work,
then stop"). This is deliberate:

- It dissolves a FIFO dependency the priority queue would otherwise break: the cron path
  enqueues `CronTrigger` then `Shutdown` back-to-back and relies on FIFO to run the trigger
  first (`cron.rs`). With `ActorStop` lowest, `CronTrigger` (Tier 1) still runs first — no
  cron change needed.
- It removes the "a reaper `Shutdown` jumps ahead of a just-delivered `SubagentFinished`"
  drop window by ordering: Tier-2 always drains before Tier-3.
- Real process shutdown is driven by `actor_parent_token` cancellation, not this message, so
  fast shutdown is unaffected.

### 3. Scheduling logic

- **Tier 1 / UserInput** — on dequeue, `try_pop` the other **already-queued** consecutive
  non-slash `UserInput`s and **coalesce** them (see §6). Does **not** drain the subagent
  buffer (no piggyback).
- **Tier 1 / CronTrigger · SubagentSpawned** — unchanged (never co-resident with `UserInput`).
- **Tier 2 / SubagentFinished** — on dequeue: append to `pending_subagent_results` + persist;
  `try_pop` any other queued Tier-2 (append each); then fold the **whole buffer** into **one**
  merged `SubagentNotification` turn (§4); clear buffer + persist. Stale wakes that find an
  empty buffer are no-ops.
- **Tier 3 / ActorStop** — cancel `actor_token`, break (only reached once higher tiers drain).
- **Drain-on-exit** — the two **preemptive** exit paths that bypass the `ActorStop` message
  (mailbox close; `actor_parent_token` cancel) must drain remaining Tier-2 into the buffer +
  persist before exiting, so nothing delivered-but-unhandled is lost.
- **Hydration** — on actor start, if `pending_subagent_results` is non-empty, synthesize a
  Tier-2 wake so the merged turn runs (after any higher-priority trigger that hydrated it).

### 4. The `SubagentNotification` turn

- New `JobKind::SubagentNotification` (with `allowed_for(*) == true`, like `Spawned`) +
  `JobInput::SubagentNotification` (`crates/job/src/kind.rs`). A distinct kind keeps trace /
  cost classification honest. (`JobKind::System` is **not** allowed on a `User`-rooted
  session, so it can't be reused here.)
- Runs the **same main path** `run_agent_loop`: **identical system prompt + full toolset →
  the prompt-cache prefix is unchanged.** This is a hard requirement.
- Content is nested **XML** delivered as a `Role::User` message with **`from_user = false`**
  (synthetic). `AgentLoop::run` derives this from `JobInput::SubagentNotification` (every other
  job kind stays `from_user = true`), so the chat REST/WS surfaces — which hide `Role::User`
  rows with `from_user = false` as agent-injected context — never render it as a fake
  user-authored bubble. The framing rides in this **per-turn content**, never the system prompt
  (touching the system prompt would break the cache). Per-result text reuses the existing
  1024-char truncation + "full text in child session transcript" pointer. **Text-only** (images
  dropped, matching today).

  ```
  [background tasks finished since your last turn — report the outcome to the
  user as a fresh, proactive message]

  <subagent_results>
    <result handle="bg-7f3a" type="planner" status="completed">
      <task>…task_summary…</task>
      <output>…final_text (truncated)…</output>
      <child_session>…child_session_id…</child_session>
    </result>
    …
  </subagent_results>
  ```

- **Proactive, empty-suppressed**: the reply is sent to the session's channel as a proactive
  message (cron-style framing — "report to the user"; channels handle out-of-turn sends). There
  is **no `<no_output/>` sentinel and no explicit "stay silent" instruction**, but if the final
  assistant message is **empty / whitespace-only it is not sent** — the model's only implicit way
  to stay quiet. Empty-reply policy is asymmetric: **non-user turns** (this notification turn,
  cron) silently suppress a blank reply; a **user turn** instead surfaces a fallback `Notice`
  (`send_user_reply`), since the user is waiting and a blank bubble would leave them hanging.
- **Persistence**: the synthetic XML turn + the assistant reply persist into the transcript
  normally — same as any main-path turn (the XML turn at `from_user = false`, hidden from chat).
- **Failure-safe drain (crash- and error-safe)**: the drained (empty) buffer is persisted to the
  row **before** the fallible turn, so a crash mid-turn can't leave the results in the row to be
  replayed as a duplicate notification on restart. On an in-process turn failure (provider error,
  cost rejection, cancellation) the results are **restored** to `pending_subagent_results` and
  re-persisted so the next drain retries them. So: a transient failure never loses a completion; a
  crash mid-turn drops it from the parent row but it survives in the child session's trace. (The
  actor is single-threaded, so nothing is buffered while the turn runs.) After a failure the actor
  **retries on a capped backoff** (`NOTIFY_RETRY_*` — 60s, ×2, cap 5 min, ≤5 attempts), via a
  biased `select!` over `mailbox.recv()` vs a sleep in the run loop, so a quiet fire-and-forget
  session is still notified during the idle window. A real inbound message wins the race and resets
  the schedule; after the attempt cap the actor falls back to delivering on the next message /
  hydration (so the `last_active` bumped by each retry can no longer keep it from being reaped).

### 5. Buffer (unchanged shape)

`pending_subagent_results` stays the durable merge source: dedup by `handle_id`, cap 64
drop-oldest, persisted inside the `sessions.data` JSON blob (`SessionState`).

### 6. UserInput coalescing (high-fidelity)

When the actor is free, drain the **already-queued** consecutive **non-slash** `UserInput`s
and merge them into one turn (no debounce timer — this only batches the "busy pile-up" case,
not rapid sends to an idle actor; accepted).

- **High-fidelity storage (built):** each message in the batch is kept as its **own**
  `Role::User` transcript row. `handle_merged_user_turn` appends the leading messages via the
  new `AgentLoop::append_user_message` (which writes both the in-memory `ContextManager` and the
  persisted `session_messages` log, same as the normal user-turn append), then runs the turn
  with the last message as `user_content`; the job record's `JobInput::UserChat` carries the
  combined content for provenance. The existing `merge_for_llm`
  (`crates/agent/src/runtime/agent_loop.rs`) collapses the consecutive rows into one message for
  the provider call. One reply answers the batch.
- **Slash is a hard boundary** (not just `/compact`). Generalize `is_compact_command` →
  `is_slash_command` (syntactic: first text block, trimmed, leading `/` + non-empty token —
  the same shape as `agent_loop.rs::detect_slash_invocation`). Any slash message is a
  boundary: flush the batch before it; `/compact` runs `handle_compact`; any other slash
  (skill command or unknown) runs as its **own single-message turn** so the leading slash sits
  at position 0 and in-loop skill detection still fires. Only non-slash runs coalesce.

### 7. Explicit cancel: `/stop` (planned, separate from this feature)

A `/stop` user command that **stops all of this session's activity**:

1. cancel the parent's in-flight turn **and** every in-flight **background subagent** it spawned;
2. clear queued subagent notifications — both `pending_subagent_results` and any queued Tier-2
   `SubagentFinished` still in the mailbox.

Key point: a mailbox tier — even the highest — **cannot** preempt a running loop, because a
busy actor is inside `run_agent_loop` and isn't reading its mailbox; a queued message is only
seen between turns. So `/stop` is **not** a priority tier — it's an **out-of-band** control
command (intercept like `/new`, server-side).

Mechanism + traps for whoever builds it:

- Cancel in-flight jobs via `JobCancellationRegistry` / `JobLifecycle::cancel` (add
  `CancelReason::UserStopped`). The parent turn is one job; the background subagents are jobs on
  **child** sessions (`parent_job_id` links them) that deliberately anchor to the **process**
  token, not the parent's actor token — so cancelling the parent turn does **not** touch them;
  they must be found (by `parent_job_id` / lineage) and cancelled explicitly.
- A cancelled background subagent's wait task still escorts a `SubagentFinished` (Cancelled)
  back to the parent — so `/stop` must **suppress** those deliveries (or have the parent drop
  `UserStopped`-cancelled results) or the buffer repopulates right after you clear it.
- Clearing the **in-memory** queued Tier-2 must be done by the actor (out-of-band code can't
  touch the mailbox) — e.g. a dedicated `ClearPendingSubagents` control message the actor
  honours after the cancelled loop returns, which also clears + persists
  `pending_subagent_results`.
- Parts that have nothing to act on are no-ops (idle actor / empty buffer).

This is why `ActorStop` can sit at the **lowest** tier (§2): "stop now" is `/stop`'s cancel
path, not a mailbox priority. Automatic scheduling stays non-preemptive; `/stop` is the only
explicit preemption.

## Implementation Plan

Three phases; each compiles + lands with tests and is independently reviewable.

1. **Priority mailbox + `ActorStop` rename** — introduce the priority-queue mailbox type,
   route every `AgentMessage` through it with intrinsic priorities, rename `Shutdown`. No
   behaviour change yet beyond ordering. Tests: tier ordering, FIFO-within-tier, close →
   `None`, backpressure, cron `CronTrigger` precedes `ActorStop`.
2. **`SubagentNotification` turns** — add `JobKind`/`JobInput` variant, the XML framing
   (proactive, no silence sentinel; empty model output suppressed), the Tier-2 drain/merge → one
   turn, hydration wake, drain-on-exit. Remove `drain_pending_subagent_notice` + the
   `BACKGROUND_NOTIFICATIONS_*` preamble and the `handle_user_input` background-notice injection.
   Tests: merge of N completions, empty-output suppression, transcript persistence, reaper/exit
   durability, allowed-for on a user session.
3. **UserInput coalescing** — `is_slash_command` boundary, high-fidelity multi-append, rely on
   `merge_for_llm` for provider coalescing, `reply_to` = last. Tests: busy-pile-up merge, slash
   boundary (compact + skill + unknown), transcript fidelity, single reply.

## Risks

- **The priority mailbox is the highest-risk piece** — it re-implements `mpsc`'s close /
  backpressure / FIFO-within-tier semantics. Needs thorough concurrency tests.
- **Proactive messaging** — the agent now sends out-of-turn messages to user channels. Cron is
  the precedent; confirm each channel tolerates it.
- **Cost** — every background completion now costs an LLM turn (vs the near-free piggyback);
  simultaneous completions are merged to mitigate.

## Related

- `crates/agent/src/actor/mod.rs` — `AgentActor::run` loop, `AgentMessage`, `handle_subagent_finished`, `handle_user_input`, `is_compact_command`, `drain_pending_subagent_notice` (to be removed)
- `crates/agent/src/actor/router/system_spawn/subagent.rs` — `deliver_background_result`, `await_subagent_terminal` escort
- `crates/agent/src/actor/supervisor.rs` — idle reaper + `in_flight_background_subagents` pin
- `crates/job/src/cancellation_registry.rs` + `JobLifecycle::cancel` — the cancel-token path `/stop` (§7) reuses
- `crates/agent/src/actor/router/cron.rs` — one-shot cron session (out of scope, but its FIFO `CronTrigger`→stop dependency drives the `ActorStop` placement)
- `crates/agent/src/actor/cron_prompt.rs` — `frame_cron_prompt`, the precedent for synthetic-user-role autonomous turns
- `crates/agent/src/runtime/agent_loop.rs` — `merge_for_llm` (provider-side coalescing), `detect_slash_invocation`
- `crates/job/src/kind.rs` — `JobKind` / `JobInput` / `allowed_for` (add `SubagentNotification`)
- `crates/model/src/spawn_protocol.rs` — `PendingSubagentResult`, `BACKGROUND_NOTIFICATIONS_*` (to be removed)
- `crates/model/src/session.rs` — `SessionState::pending_subagent_results`
- `docs/modules/agent.md` — current-reality module doc; carries a forward pointer to this file
