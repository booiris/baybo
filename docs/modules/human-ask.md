# human-ask — Durable human-in-the-loop prompts (`AskUser` + tool approvals)

> Status: **PR 1 in progress** — `baybo-human-ask` (queue, core, gates,
> manager), the ledger and the `tools::ask` ports exist; see
> [Implementation notes](#implementation-notes-pr-1). This document is the spec the three
> PRs described in [PR plan](#pr-plan) are built against. Line numbers cited
> for existing code are as of commit `504fe0af` and will drift; the named
> items will not.

## Overview

An **ask** is the agent stopping a turn to hand a question to a human and
waiting for the answer. Baybo has two kinds:

- **Approval** — "may this tool call touch these resources?" Today this is the
  per-channel `ApprovalQueue` + `ChannelApprovalGate` in
  `crates/tools/src/approval.rs`: in-memory, per-channel, oneshot-backed,
  300 s then self-deny.
- **Question** — new. The model calls **`AskUser`** with 1–4 multiple-choice
  questions (each a tab in the UI), the user picks, and the structured answer
  comes back as the tool result.

Both kinds share one lifecycle — raise, park the tool future, wait for a human,
resolve, continue the turn — and today that lifecycle has three properties the
design changes:

1. **It is in-memory.** A gateway restart loses every parked prompt and the
   turn behind it; hydration then persists a synthetic "interrupted" result for
   the dangling `tool_use` (`crates/context/src/transcript_repair.rs`).
2. **It times out.** `APPROVAL_TIMEOUT = 300 s` (`crates/gateway/src/channel/boot.rs`)
   denies on the user's behalf. A question that takes a human an afternoon to
   think about cannot live under a five-minute window.
3. **It is approval-shaped end to end** — payload, decision enum, wire frames,
   the chat-list "approval pending" bit.

The `baybo-human-ask` crate owns the generalized lifecycle; the kinds are
payloads. Pending asks are **written to sqlite before they are shown**, never
time out in attended turns, survive a gateway restart, and are resumed by the
user's eventual answer. Resolved rows are kept forever as an audit ledger.

Layering, following the store/storage split the rest of the schema uses:

| Crate | Owns |
| --- | --- |
| `baybo-model` (`model::human_ask`, `model::approval`) | Value types: `AskId`, `AskKind`, `AskPhase`, `AskRequest`, `AskResolution`, `AskStepSummary`, the `Question*` payloads, `ApprovalRequest` (moved here from `baybo-tools`), `ResumeRequest`, the tool-name and limit consts. Pure data. |
| `baybo-store` (`store::human_ask`) | `HumanAskRow` + the `HumanAskStore` port. No delete method. |
| `baybo-storage` (`sqlite/human_ask.rs`) | `SqliteHumanAskStore` over the `human_asks` table. DDL in `sqlite/mod.rs` like every other table. |
| `baybo-tools` (`tools::approval`, new `tools::ask`) | The **tool-facing ports** only: `ApprovalGate`, `ApprovalGateMap`, `AutoDenyGate`, `ApprovalHandle`, `ApprovalOutcome`, `refusal_reason`; new `QuestionGate`, `QuestionHandle`, `AskCancel`, `ParkGauge`, `ToolContext.question`, `Tool::sole_call()`. Never depends on `baybo-human-ask`. |
| `baybo-human-ask` | `AskQueue` (the queue/oneshot/pending-edge mechanism, moved out of `tools::approval`), `AskQueues` (one queue per `ChannelType`, `get_or_create`), `AskCore` (store + observers, shared by gates and manager), `ChannelAskGate` (implements both gates: persist → enqueue → wake → await, no timeout, origin-aware cancel), `PolicyDenyGate`, `HumanAskManager` (the verbs every other crate calls), the `ResumeSink` / `LivePendingAsks` / `AskObserver` ports, the `AskUser` tool, `ask_step_summary`, `MemoryHumanAskStore` test fixture. Never names `Channel`. |
| `baybo-channels` | `AskSurface` on `Channel`, `SessionEvent::{QuestionAsked, AskResolved}`, `RouterInbound::ResumeAsk`. |
| `baybo-storage`, `baybo` | `Store.human_ask` (the DI bundle field) and the single `stores.human_ask.clone()` read in `build_managers` that constructs `AskCore`. |
| `baybo-gateway`, `baybo-agent`, `baybo-project`, `baybo-tui` | Consumers that receive `Arc<AskCore>` / `Arc<HumanAskManager>` / `AskQueues`, never the store. `gateway`: wire arms, SubscribeState, queue watchers that dispatch frames, push, REST, the lazy channel install. `agent`: executor/loop/actor/router, the `ResumeSink` and `PendingToolCalls` impls. `project`: the timeline observer and card-close verb. `tui`: compile-only arms + TODO. |

Dependency edges, all acyclic: `human-ask → model, store, tools`;
`channels → tools, human-ask`; `project → human-ask`;
`agent, gateway, tui, baybo → human-ask`. `tools` never depends on
`human-ask` (the "`baybo-tools` carries only generic ports" rule in
`CLAUDE.md`), and `human-ask` never depends on `channels` — everything it
needs from the channel layer (which queue, how to wake a client, how to reach
the router) arrives as a port it defines.

## Vocabulary

- **Ask** — one prompt to a human. Keyed by `AskId` (UUID, today's
  `ApprovalRequest.call_id`). One tool call can raise more than one ask over
  its life, so `tool_call_id` is **not** unique; `ask_id` is.
- **`AskKind`** — `Approval | Question`.
- **`AskPhase`** — `BeforeExecute` (the executor's pre-execute gate) or
  `DuringExecute` (raised from inside `Tool::execute` through a handle).
  `AskUser`'s question is raised from inside its own `execute`, so it is
  `DuringExecute` by construction.
- **Resumable** — an ask whose answer can be applied after the original tool
  future is gone. Exactly one definition, `resumable(kind, phase)`:
  `kind == Question || phase == BeforeExecute`. A `Question` is resumable
  because `AskUser` has no state besides the wait; a `BeforeExecute` approval
  is resumable because approving means "run this exact call", which can be
  re-dispatched from the persisted `tool_use` block; a `DuringExecute`
  approval (Bash asking mid-command to go unsandboxed) is not, because the
  command's partial state died with the process.
- **Row states** — `pending` (`resolved_at IS NULL`), `resolved`
  (`resolved_at` set, `applied_at IS NULL`: a human or a policy decided, but
  no turn has consumed the decision yet), `applied`. Only `applied` rows
  are finished. **Unapplied** = pending ∪ resolved. The in-memory queue holds
  entries for `pending` rows only; `resolved` is a store-only state.
- **Live / dead** — a pending ask is *live* while a tool future is parked on
  its oneshot in this process, *dead* when the row is still pending but no
  waiter exists (after a restart, an actor panic, or a process shutdown).
  Dead asks are still shown and still answerable; answering one opens a
  **Resume turn**.
- **Attended** — a turn in which a human can see and answer asks. Defined
  once, on the turn's input kind (see [Attendedness](#attendedness)).
- **Raised / Answered / Abandoned edges** — the per-session boundary events
  the queue publishes to watchers (chat-list mark, push). Unchanged in
  mechanism; `Raised` gains `kind`. **Ask events** are the per-ask sibling
  (`AskRaised(entry) / AskClosed { session_id, ask_id, kind, outcome }`) the
  gateway watches to dispatch frames — they carry the session because
  `Channel::dispatch_event` keys delivery on it (`channel.rs:388, :408-415`)
  and the watcher may read neither the row nor the queue.

## Design Decisions

### One mechanism, two payloads

`ApprovalQueue`, `PendingApproval`, `PendingEdge`, `PendingWatcher`, the
outbox/`publish_pending` ordering machinery and `QueueCleanup` in
`crates/tools/src/approval.rs:263-728` are kind-agnostic in mechanism; what is
approval-typed is the payload surface — the oneshot and `ResolveFn`
(`:274`) carry `ApprovalDecision`, `peek_head`/`enqueue_mirror` trade in
`ApprovalRequest`, `push` reads `req.call_id`/`req.tool`. They move to
`baybo_human_ask::queue` as `AskQueue` / `PendingAsk` with the oneshot and
resolver carrying `AskResolution` and entries carrying `AskRequest`; the
approval-typed surface the TUI and `baybo prompt` use survives as thin
wrappers (see [Data model](#data-model)). What stays approval-specific stays where it
is: `ApprovalRequest` (payload, moves to `baybo-model` so the store can type
it), `ApprovalDecision`, `ApprovalOutcome`, `refusal_reason`, `ApprovalHandle`,
`ApprovalGateMap` and the `ApprovalGate` trait (ports in `baybo-tools`).

Rejected: adding `kind: Question` to `ApprovalRequest` and `Answer(..)` to
`ApprovalDecision`. `ApprovalDecision` is persisted in `ToolResultMeta`, span
events and the issue timeline; widening it would make every ledger record a
question as an approval, and `AutoDenyGate`'s fail-closed semantics would be
applied to questions on channels that have no question UI.

The invariants the queue already carries and which the move must keep
(`approval.rs:303-336, 389-455, 643-647`):

- The queue is **per channel, not per session**; the waker is handed the ask it
  is woken for and never re-reads the queue.
- Edges are appended under the lock and drained by a single drainer outside
  it, in mutation order. **Watchers run inline on the parked tool's future and
  must not block** — no sqlite write inside a watcher. The write-ahead insert
  happens in the async gate, *before* `push`.
- `PendingAsk.responder: Option<oneshot::Sender<_>>` — `None` already models a
  display-only mirror entry (TUI). Dead asks reuse this shape.

`tool_call_id` becomes a plain `String` everywhere the design touches:
`ApprovalRequest`, `ApprovalHandle::new`, `QuestionHandle`, the row, the new
frames. Today it is an `Option` only so test fixtures can skip it
(`approval.rs:48`, `lib.rs:649-651`); every production caller passes `Some`
(`tool_executor.rs:833, :965`). `CLAUDE.md`'s "no `Option<T>` to accommodate
tests" applies. The one `Option` that stays is on the wire —
`Frame::ApprovalRequested.tool_call_id` and `ApprovalCard.tool_call_id`,
pinned byte-for-byte by the TUI and iOS mirrors; the gateway fills it from
the `String` at dispatch, and the three client-side `ApprovalRequest`
constructions (`tui/src/client/transport.rs:343, :393`, `prompt_cmd.rs:621`)
`unwrap_or_default()` it back.

### `human_asks`: a write-ahead ledger that is never deleted

```sql
CREATE TABLE IF NOT EXISTS human_asks (
    ask_id        TEXT PRIMARY KEY,
    session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    channel       TEXT NOT NULL,            -- ChannelType; which queue to reload into
    tool_call_id  TEXT NOT NULL,            -- the LLM tool_use id the ask blocks
    kind          TEXT NOT NULL,            -- 'approval' | 'question'
    phase         TEXT NOT NULL,            -- 'before_execute' | 'during_execute'
    request       TEXT NOT NULL,            -- AskRequest JSON (ApprovalRequest | QuestionRequest)
    created_at    INTEGER NOT NULL,         -- unix µs, sqlite/time.rs
    resolved_at   INTEGER,                  -- NULL = pending
    resolution    TEXT,                     -- AskResolution JSON, NULL while pending
    applied_at    INTEGER                   -- NULL until the turn consumed the resolution
);
CREATE INDEX IF NOT EXISTS idx_human_asks_unapplied
    ON human_asks(session_id, created_at) WHERE applied_at IS NULL;
```

- `REFERENCES … ON DELETE CASCADE` is **enforced**, not documentary: the
  bundled sqlite (`rusqlite` `bundled`) builds with foreign keys on, so
  `create` fails for a `session_id` with no `sessions` row — the ask's session
  must be persisted first. Sessions are never deleted, so the cascade never
  fires in practice.
- **No `delete` on `HumanAskStore`, no janitor sweep.** Resolved rows are the
  audit trail ("Governable: selection and execution are auditable"). The
  transcript's `tool_result` is the authoritative answer; the row adds who
  answered, when, and how (answered / declined / abandoned / policy).
- **Three states, two claims.** `resolve` and `apply` are both claim-style
  single-statement `UPDATE`s (`… WHERE ask_id = ? AND resolved_at IS NULL`,
  `… WHERE ask_id = ? AND applied_at IS NULL`) returning `bool`, so
  `interact_write`'s BUSY re-run is idempotent (`sqlite/mod.rs:396-430`,
  `write_closures_are_retry_safe`) and two concurrent claimers always have
  exactly one winner. `create` is `INSERT … ON CONFLICT(ask_id) DO NOTHING`.
  Reads: `get`, `list_unapplied()`, `list_unapplied_for_session(session_id)`.
- Timestamps are unix µs via `sqlite/time.rs`, like every other table.
- What persist-first buys, precisely: **row before queue push** — a crash
  between insert and the wire frame leaves a pending row the reload will
  show; **resolution before oneshot** — the *decision* is in the ledger
  before anything acts on it. It does not by itself put the answer on the
  transcript; the `applied_at` state and the
  [turn prelude](#the-turn-prelude-one-home-for-unapplied-asks) are what
  make a recorded decision reach the model after any interruption.
- When `applied_at` is stamped, **live path**: by `ChannelAskGate` itself,
  after the oneshot yields and before the resolution is returned to the
  caller. That is the one point common to both phases and both kinds that
  holds the `ask_id` — the executor and the loop only ever see
  `ApprovalOutcome` / `ExecutedTool.approval`, neither of which carries an id
  (`tool_executor.rs:371-377`, `approval.rs:81-84`), and a `DuringExecute`
  approval raised from inside Bash (`bash/mod.rs:1981, :2117, :2221`) passes
  through `ApprovalHandle`, which keeps nothing but `last_decision`. For a
  `BeforeExecute` approval this is literally "before the tool executes", so a
  crash mid-execution leaves an applied row and a dangling `tool_use` that
  hydration fills as "interrupted" like any other crashed call — never a
  second execution with side effects. For a question or a `DuringExecute`
  approval the window between stamp and the `tool_result` append is accepted
  on the same terms: the ledger has the decision, the transcript gets the
  interrupted fill, the model asks again. **Resume path**: by the prelude —
  before re-executing an approved call, after appending for everything else.

### Attendedness

Today's "unattended" marker is `Option<InheritedToolContext>`, minted only by
cron fires (`actor/mod.rs:757`) and inherited by their subagents; the executor
reads it at `tool_executor.rs:807` and `:960`. Two other doors answer the
same question differently: issue runs pass `None` and **do** prompt (through
`TimelineApprovalGate`, answerable from the kanban card); in-process subagents
run on channel `subagent`, which has no registered gate, so
`ApprovalGateMap::get` already falls back to `AutoDenyGate` for them
(`approval.rs:237-241`, `subagent_spawner.rs:663`). Three spellings, two
homes. One predicate, in `baybo-turn` beside `is_chat_turn`:

```rust
impl TurnInputKind {
    /// A human can see and answer asks raised by this turn.
    pub fn is_attended(self) -> bool {
        match self {
            UserChat | SubagentNotification | Compact | Resume | IssueRun => true,
            Cron | CronNotification | Spawned => false,
        }
    }
}
```

- `UserChat`, `Resume`: the user is in the conversation.
- `SubagentNotification`: a background result delivered into a live user
  session; the tab is subscribed, the ask is visible. Attended.
- `IssueRun`: attended **for approvals**, via the card timeline + board
  (see next section). It never sees `AskUser`.
- `Cron`, `CronNotification`: fires, including the dream pass and one-shots,
  on whatever channel created the job. Unattended, as today.
- `Spawned`: every in-process subagent turn. Unattended — this is what the
  channel door already does by accident; `is_attended` makes it the rule's
  home and the channel door defence in depth. **Not a behaviour change.**

`AgentLoop::run_inner` computes `attended = turn_kind.is_attended()` once,
beside `background_eligible`, and passes it to `ToolExecutor::execute`. Tools
never see the bool: the only attendedness signal a tool gets is whether
`ctx.approval` / `ctx.question` is wired to a gate that can prompt. The two
`inherited_context.is_some()` spellings collapse into one branch per phase.
`InheritedToolContext` and its plumbing are deleted
(`model/inherited_tool_context.rs`, `ToolContext.inherited_context` at
`tools/src/lib.rs:428,512`, `run_with_inherited_context`,
`SubagentParentContext.inherited_context` at `spawn_protocol.rs:179`,
`SubagentTool` forwarding at `subagent/tool.rs:616` and its tests at
`:899-929`, `SubagentSpawned.inherited_context`, `subagent_spawner.rs:199,319-366`,
`agent_loop_e2e.rs:3382-3416`, the prose at `mcp/reconciler.rs:555`).

**Unattended turns are policy-denied through `PolicyDenyGate`**, a
`baybo-human-ask` type implementing `ApprovalGate` over `AskCore`: it writes
the row already resolved `PolicyDenied` (no queue entry, no edge, no frame)
and returns `ApprovalOutcome::policy(Deny)`. The executor hands it to both
phases — it is the pre-execute gate's choice and the `ApprovalHandle`'s gate
— so "deny + ledger row" has one implementation. `ToolExecutor::new` takes it
as a required `policy_deny: Arc<dyn ApprovalGate>` built once in
`build_managers`. The reason the model sees is
`refusal_reason(ApprovalResolution::Policy)`, replacing the hand-written
"runs unattended" text. The ledger therefore records why a cron job's tool
call failed. The one deny that is *not* ledgered is `ApprovalGateMap`'s
`AutoDenyGate` fallback for a channel with no gate in the map — `subagent`,
or a sidecar type that has not connected since boot; that is a
misconfiguration guard, not a policy.

**No timeout in attended turns.** `APPROVAL_TIMEOUT` (`boot.rs:33`),
`ChannelApprovalGate.timeout`, `ApprovalOutcome::timed_out`,
`REFUSAL_TIMED_OUT` and `APPROVAL_HEADROOM` (`tool_executor.rs:121`) are
deleted. `ApprovalResolution::TimedOut` stays as a serde variant so old span
events still decode, documented as historical.

`baybo prompt` (`prompt_cmd.rs:258-264`) keeps working through the session
tier of `ApprovalGateMap` (`AutoApprove`/`AutoDeny` per session); the tier
survives the generalization.

### Issue runs stay attended for approvals

`Q11` of the design interview grouped project runs with cron as "unattended".
The code says otherwise: issue runs prompt through `TimelineApprovalGate`
(`crates/project/src/approvals.rs`), the board renders `attention().approvals`,
`POST /projects/{id}/issues/{n}/approvals/{call_id}` answers them, and
`Timeline.tsx` has the buttons. Auto-denying would retire a working feature.

Decision: `IssueRun` is attended for approvals and keeps its whole surface;
the timeout removal and persistence apply to it like any other session.
`AskUser` is hidden from issue sessions by trigger scope (below), so the board
never has to render a question.

`TimelineApprovalGate` today wraps the owner gate's `request` to bracket it
with timeline events and keeps an in-memory `open` ledger with three closers
(settle / shutdown / `AbandonGuard`). With asks durable and resolvable while
the project crate's wrapper is not on the call stack, the bracket pattern
cannot see every resolution. It becomes an **async observer**:
`AskCore::add_observer(Arc<dyn AskObserver>)` with
`#[async_trait] async fn on_event(&self, ask: &HumanAskRow, event: AskEvent)`,
invoked from the gates' and manager's **async verbs** (after `store.create`
on raise, after the claim on resolve/abandon/policy-deny) — never from the
sync queue-watcher drain. The project crate records
`IssueEventBody::ApprovalRequested / ApprovalResolved` from there for
`TriggerSource::Issue` sessions (the `attempt` it needs comes from
`list_runs` at `Raised` time; a `Resolved` after a restart carries none,
which the store's `attempt: Option<i64>` already allows) and drops its own
open-ledger. Boot reload publishes queue edges only, never observer events,
so a restart does not append a duplicate `approval_requested` timeline row.

The card-close cancel (`close_card_prompts`, keyed by `(project, number)`
and reached from `settle_run` and `stop_live_run` through the existing
`CardPromptCloser` / `set_prompt_closer` seam, `approvals.rs:122-138`,
`manager.rs:446-450`) calls `HumanAskManager::abandon_for_session(run.session_id)`
— live and dead asks alike; a run with `session_id: None` has no asks. What
makes a raced *Allow* return `AskClosed` is the store claim losing, replacing
the open-ledger arbitration `project.md:1875-1883` describes.

`install_timeline_approval_gate` (`boot.rs:92-107`) is replaced by one
wiring call in `baybo-project`, run after both managers exist:
`baybo_project::install_ask_bridge(core, project, asks, sessions)` adds the
`TimelineAskObserver { manager: Weak<ProjectManager>, sessions }` to
`AskCore` and sets `project`'s prompt closer to an `AskCloser { asks }`. The
observer holds a `Weak` for the same reason today's `ParkedPrompts` does
(`approvals.rs:104-108`): `ProjectManager → closer → HumanAskManager → AskCore
→ observers` would otherwise be a strong `Arc` cycle. The gate map holds the
channel's own gate again, so `gate_map.get(owner)` and `channel.ask_gate()`
are the same object (today they are not). `ManagerGraph.timeline_approvals`
goes away; nothing awaits it at shutdown.

Issue sessions never use the chat Resume door
([below](#answering-a-dead-ask-the-resume-turn)): `resolve` on a dead ask
in a `TriggerSource::Issue` session persists the decision and requests
nothing, because the run's own re-drive — `resume_unsettled_runs` at boot
(`gateway_cmd.rs:710`, `manager.rs:2168`) — already re-dispatches the
interrupted run into the same session as a real `IssueRun` turn with its
`IssueRunWaiter`, and that turn's [prelude](#the-turn-prelude-one-home-for-unapplied-asks)
consumes whatever the row says: a decision the user already made is applied
under the real run; a still-pending ask is abandoned and the model simply
asks again, live, on the card. One run attempt executes the approved call;
`finish_run` fires once.

> **Owner decision needed** — this keeps issue-run approvals; the interview
> wording would have dropped them. See [Decisions for the owner](#decisions-for-the-owner).

### Verbs resolve asks; drops never do

Every ask leaves a state through one of these verbs, each of which persists
first and only then touches memory. The manager and the gates share
`AskCore { store, observers }`, so the gate can abandon its own ask on cancel
without a back-edge to the manager.

| Verb | Persists | Then |
| --- | --- | --- |
| `resolve(ask_id, resolution)` | `resolution`, `resolved_at` (claim) | remove the entry (live or dead), `Answered` edge, `AskClosed` event; live: fire the oneshot; dead (or a waiter that vanished in the same instant), resumable, and `resume_sink.resumes_on_answer(session)`: hand `ResumeRequest { session_id, ask_id }` to the resume sink. `Err(AskClosed)` if the claim affected 0 rows, `Err(NotFound)` for an unknown id, `Err(KindMismatch)` for an approval decision on a question or answers on an approval. A row whose `channel` has no queue still gets the claim, with a `warn!`. |
| `abandon(ask_id)` / `abandon_for_session(session_id)` / `abandon_for_tool_call(session_id, tool_call_id)` | `Abandoned` (claim, pending rows only) | remove entry, `Abandoned` edge, `AskClosed(abandoned)` event, fire the oneshot with `Abandoned` (a live waiter sees `abandoned()`, and the gate's stamp point marks the row applied). `abandon_for_tool_call` also stamps `applied` on what it abandoned — its caller appends the result next — and returns `ToolCallAbandon::{NoAsk, Abandoned, Answered}`; `Answered` (a claim lost, or a row already resolved) means append nothing. |
| `policy_deny(request)` | row created already `resolved = PolicyDenied`, `applied` | no queue entry, no edge; observer event only. |
| `settle_unapplied(session) -> Vec<Settled>` | per unapplied row: pending → `Abandoned` (claim) + `applied` (the caller appends next); resolved non-resumable → `applied` | returns `Settled::Abandoned { tool_call_id, kind }` for rows it abandoned and `Settled::Apply { ask_id, tool_call_id, kind, resolution }` for `resolved` **resumable** rows, so the caller can put the right `tool_result` on the transcript. A resolved non-resumable row is stamped applied and reported as nothing — its result is either already on the transcript or was crash-filled by hydration. Removes the queue entries it abandoned. |
| `mark_applied(ask_id)` | `applied_at` (claim) | — |
| `reload_pending()` | `Abandoned` for non-resumable pending rows; `applied` for resolved non-resumable rows | re-enqueue every **pending** resumable row on `queues.get_or_create(row.channel)` as a dead entry, `Raised` edge; request a resume for `resolved` resumable non-Issue rows **without** enqueueing or publishing anything — the user already answered, and re-showing the card, re-marking the session and re-pushing for it would be wrong. |

A dropped waiter is **not** a resolution. `QueueCleanup::drop` (today:
`drop_call` → `Abandoned` edge) becomes `queue.orphan(ask_id)`: a resumable
entry loses its responder and becomes dead, no edge is published, the row
stays pending. This is what makes a parked ask survive an actor panic
(`recover_panicked_actor_session` cancels the turn; the ask is still on the
queue, dead, and the Resume door spawns a fresh actor when it is answered)
and a process shutdown (next section). A non-resumable entry is removed by
`orphan` with an `Abandoned` edge and an `AskClosed(Abandoned)` event (it is useless without its tool future),
but Drop cannot write the row; `reload_pending` and `settle_unapplied`
reconcile it.

Two claimers can race on one row — a WS task answering a dead ask while the
actor's prelude abandons it. The claim `UPDATE` is the arbiter: the loser
gets `false` and does nothing to the transcript. Every path that then has to
put something on the transcript re-reads the row's state through
`settle_unapplied` rather than remembering what it believed a moment ago.
Because a dead `resolve` removes the entry, every queue-enumerating read
surface (`SubscribeState`, the chat-list mark, `cards_awaiting_approval`, the
push recheck, `note_pending_edge`'s pending predicate) stays correct without
learning a new state.

### Shutdown is not `/stop`

Today a graceful shutdown (`runtime.rs:846` `actor_parent_token.cancel()`) is
indistinguishable from `/stop` inside the loop: both trip the turn token
(`run_agent_loop` mints `turn_token = actor_token.child_token()`,
`actor/mod.rs:609`, and passes only that down), the pre-execute `select!`
(`tool_executor.rs:849-853`) yields `abandoned()`, the executor returns a real
`Denied` result, and the cancelled-batch path (`agent_loop.rs:1601-1641`)
appends a fill for every call that did not finish. Either way the `tool_use`
is no longer dangling, so there is nothing to resume — that is why today's
prompts do not survive `baybo gateway restart`.

Two facts shape the fix. There is **no existing discriminator**: `with_turn`
sees one token and `cancel_row` always writes `ParentCancelled`
(`scope.rs:192-200, :252-259`); the row reads `UserStopped` after `/stop`
only because `handle_stop`'s earlier `TurnLifecycle::cancel(UserStopped)`
trips the token and writes first (`user_input.rs:445-448`,
`lifecycle.rs:203-211`). And the **batch loop is not the first to see the
cancel**: the gate's own `select!` inside the executor completes the tool
future with `Denied` before the loop's biased select observes anything.

So the origin decision moves into the gate race, and the plumbing is new:

- `run_agent_loop` passes `TurnTokens { turn, actor: self.volatile.actor_token.clone() }`
  through `AgentLoop::run` / `run_inner` into `ToolExecutor::execute` and
  onto the handles as `AskCancel`. `CancelOrigin::of(&tokens)` is `Ancestor`
  iff `tokens.actor.is_cancelled()`, else `Turn`. One helper, in
  `baybo-tools::ask`, used by every gate.
- `ApprovalGate::request(req, cancel: AskCancel)` and
  `QuestionGate::ask(req, cancel)` take the tokens. `ChannelAskGate` selects
  the oneshot against `cancel.turn.cancelled()`; on cancel the arm **never
  awaits** — the loop's own `select!` (`agent_loop.rs:1601-1611`, biased
  toward `pending.next()`) trips on the same token and drops the whole tool
  future the moment the arm returns `Pending`, so a sqlite write started
  there would be torn:
  - `Turn` → return `abandoned()` immediately. `QueueCleanup`/`orphan` runs
    on the way out (entry dead, nothing persisted). The executor returns
    `Denied`, the loop collects it.
  - `Ancestor` → **stay parked** (`std::future::pending().await`). The future
    never completes; the loop's `cancel_token.cancelled()` arm wins, `drop(pending)`
    runs `orphan`, the entry goes dead, the row stays pending, the `tool_use`
    stays dangling.
- The loop's cancelled-batch path decides per origin. On `Turn`, in the
  sequential post-processing loop and **before** appending each call's
  result (the `Denied` an abandoned gate returned, or the cancelled fill for
  a call that never finished), it claims
  `manager.abandon_for_tool_call(session, tool_use_id)`. The claim and the
  append are one sequential step in one task, so "row abandoned iff a result
  is on the transcript" holds; a claim that loses (the ask was answered in
  the same instant) means the result the loop is about to append is the
  wrong one — it skips the append and lets the next prelude apply the real
  answer. On `Ancestor` the path **emits nothing and appends nothing** — no
  `tool_completed { error, "cancelled" }` events, no fills, for any
  unfinished call. The transcript is left exactly as a crash would leave it,
  and the next hydration heals the siblings with the ordinary "interrupted"
  fill while skipping the parked ask.

| | call parked on a resumable ask | parked on a non-resumable ask | other unfinished calls |
| --- | --- | --- | --- |
| `Turn` — any `TurnLifecycle::cancel` on the turn id: `/stop` (`UserStopped`), board stop (`OperatorCancel` / `BudgetExhausted`, `stopper.rs:52-65`), admin / CLI `OperatorCancel` | gate returns `Denied`; loop claims `Abandoned`, then appends it | same | cancelled fill, as today |
| `Ancestor` — `actor_token` tripped: process shutdown (`runtime.rs:846`) | nothing appended, nothing emitted; entry dead, row pending | nothing appended; entry removed; row reconciled by `reload_pending` | nothing appended; hydration fills "interrupted" |

For attended sessions the only ancestor cancel is process shutdown (the idle
reaper's `ActorStop` is a mailbox message served after the turn, never a
token; subagent parent-cancels reach only `Spawned` turns, which raise no
asks). A `/stop` racing shutdown reads `Ancestor` — the conservative outcome.

The `Ancestor` branch is **best-effort**: shutdown cancels the parent token
from a detached task and nothing joins actor tasks before the process exits
(`gateway_cmd.rs:857-880`, `runner.rs:49`), so whether the loop even reaches
its cancel arm is timing-dependent. The design does not depend on it. The
guaranteed invariant is that *nothing on the shutdown path ever appends a
result or resolves the row for a parked resumable call*; the turn row settles
`Cancelled { ParentCancelled }` if the loop gets there and boot recovery
cancels it `SystemCrash` otherwise — both terminal, both fine, the Resume
turn links by id regardless.

`ManagerGraph::shutdown` no longer runs `close_open_prompts`; there is nothing
to close. Asks are meant to still be open after the process comes back.

### Restart survival

Boot order in `runtime.rs::build_managers`, new steps in **bold**, in the
order the code can actually have:

1. `Store::open` (`:342`).
2. `recover_orphaned_traces_and_turns` (`:504`) — unchanged. A turn that died
   parked on an ask is cancelled `SystemCrash` like any other; the ask row is
   the bridge, not the turn.
3. **`AskCore::new(stores.human_ask.clone())`**, **`AskQueues::new()`** and
   **`(incoming_tx, incoming_rx) = mpsc::channel::<RouterInbound>`** — the
   router's inbound channel is created here instead of in `wire_router`
   (`:946`) so the manager can hold the `ResumeSink` as a required
   constructor field; `incoming_rx` rides `ManagerGraph` as
   `Option<Receiver<_>>` for `wire_router` to `.take()`, the existing
   `cron_trigger_rx` pattern (`:198-206`). This is the one read of
   `stores.human_ask`.
4. **`install_channels(registry, config, core, queues)` moves here** (today
   `:333-337`, before the store; nothing between depends on it) →
   `install_channel(registry, channel_type, core, queues)` →
   `build_channel(channel_type, kind, core, queues)`, which **fetches** its
   queue through `queues.get_or_create(channel_type)` instead of minting one
   and builds `ChannelAskGate { queue, waker, core }` — the gate depends on
   the store and the queue, never on the manager. The same three functions
   serve the **lazy** install of an out-of-tree `baybo.json` channel on its
   first WS connection (`resolve_or_install_channel`, `adapter.rs:193-206`,
   from `route.rs:93`), so `GatewayDeps` and `WsChannelState` carry
   `Arc<AskCore>` + `AskQueues` for it. A queue created by the reload for a
   channel whose sidecar has not reconnected yet is simply the queue the
   later install attaches its gate and watchers to; the watcher install
   re-announces the entries already on it, since a multiplexed channel has no
   `SubscribeState` catch-up.
5. **`AskPushRelay::new()` + `install(&owner)`** moves here from
   `gateway_cmd.rs:659-666`; `(relay, Option<AskPushStream>)` ride
   `ManagerGraph` and `gateway_cmd` takes the stream when it spawns the push
   task. Queue edges are drained to whoever is watching *at drain time*
   (`approval.rs:372-377, :415-455`, no replay), so the relay must exist
   before the reload publishes anything.
6. **`HumanAskManager::from_config(HumanAskManagerConfig { core, queues, resume_sink })`**
   — the same `AskQueues` the channels fetch from; the manager never holds
   the registry or a `Channel`. **`baybo_project::install_ask_bridge`** runs
   right after, once `ProjectManager` (`:640`) exists.
7. **`manager.reload_pending()`** — best-effort like
   `skill_assessor.recover_pending_jobs` (`:479`): log counts, never block
   boot. After it, the in-memory queue is the read model again and
   `SubscribeState.pending_*`, `ChatSessionSummary.awaiting_human`,
   `cards_awaiting_approval` and the push recheck work unchanged. Each
   dead-ask session with no subscriber pushes once per boot (the relay's nag
   map starts empty; `APPROVAL_PUSH_QUEUE` bounds the burst).
8. `ToolExecutor::new(…, policy_deny)` (`:825`), tool registration,
   `wire_router` (`:935`): the actor spawner passes a `baybo-agent` newtype
   over `manager.unapplied_tool_use_ids` as the `PendingToolCalls` port on
   `ContextManagerConfig` (`:1037`) and `AgentSupervisor::new` takes the
   manager as the `LivePendingAsks` probe. `Arc<HumanAskManager>` rides
   `ManagerGraph` → `GatewayDeps` → `AdminState` / `WsChannelState` like
   `project_manager`, for the resolve doors.

`PendingToolCalls` is a narrow port defined in `baybo-context`:
`#[async_trait] trait PendingToolCalls { async fn unapplied_tool_use_ids(&self, session: &SessionId) -> Vec<String>; }`,
answered by the manager **over the store** (`list_unapplied_for_session`,
resumable rows only) — not over the queue, because by the time a cold-spawned
actor hydrates, `resolve` has already removed the entry and a queue-keyed
probe would miss exactly the ask being resumed. The impl lives in
`baybo-agent` (which depends on both crates) so `baybo-human-ask` gains no
edge to `baybo-context`. Hydration's
`repair_tool_pairing(messages, skip)` **skips the synthetic fill** for those
ids and excludes them from the `needs_repair` dangling-tail trigger, so a
healthy-but-parked transcript takes the fast path; the reposition pass still
runs and leaves the dangling `tool_use` row as the tail so the eventual real
`tool_result` is adjacent. `baybo-context` never sees the store or the
manager type.

`baybo prompt` boots the same graph under the workspace lock (gateway not
running). The reload is side-effect-free apart from the in-memory push and
the non-resumable reconciliation, both of which the gateway would do
identically at its next boot; it builds no relay, so the push stream is
dropped unread (`observe` treats a closed stream as a no-op).

#### The turn prelude: one home for unapplied asks

Every path that builds an LLM request on a session's transcript has to deal
with that session's unapplied asks first, or the provider rejects the
dangling `tool_use`. Those paths are more than "the user typed a message",
and each of them **appends its own input row before the loop runs**: the
user doors (`actor/mod.rs:1155-1158`, `:1205-1217`), the issue brief
(`append_issue_brief`, `:702-705`), the cron fire (`append_cron_fire`,
`:740-743`), the background-notification prompt
(`append_background_notification_prompt_once`, `background_notification.rs:346`,
re-anchored at `:393`/`:552`), plus `/compact` (`:1142-1145`, no row but an
LLM call in `compact_now`). A settled `tool_result` must land **adjacent to
the dangling assistant row**, i.e. before any of those appends — the
`[assistant(tool_use), user(brief), tool(result)]` shape is exactly what
`repair_tool_pairing` classifies as wedged (`transcript_repair.rs:152-153`),
and nothing re-runs that repair before an LLM call.

So the rule has one home and one entry shape: `AgentActor::settle_unapplied_asks()`,
called by every door **before its append** and by `/compact`, is a
`with_turn(TurnInput::Resume { .. })` body that does nothing when the manager
reports no unapplied asks (no turn is opened) and otherwise settles them
under a Resume turn — appends, and for an approved call the re-execution
under a fresh step (it needs the executor and a turn to open a step under,
which is why this is a turn and not a bare append; `publish_cron_notification`
is the precedent for a turn whose body is only an append). The loop's
settlement body is `AgentLoop::settle_unapplied_asks(session, mode)` with
`SettleMode::{SettleOnly, Continue}`: every door uses `SettleOnly` (the turn
ends after the appends; the door's own turn follows and its first LLM call
sees the settled results), the chat Resume door uses `Continue` (the same
turn goes on to the LLM iterations). Nothing else is appended between
hydration and the settlement, so adjacency holds.

For each `Settled` the manager returns:

- `Abandoned { tool_call_id, kind: Approval }` → append the
  `TOOL_DENIED_PREFIX` body with `refusal_reason(Abandoned)` and
  `ToolResultMeta { approval: Some(Deny) }`.
- `Abandoned { kind: Question }` → append
  `{"declined": true, "reason": "user moved on without answering"}`.
- `Apply { resolution: Answered(..) | Declined }` → append the real
  `AskUser` result (identical envelope to the live path), `mark_applied`.
- `Apply { resolution: Approval(Deny) }` → append the denied result as
  `agent_loop.rs:1706-1716` does live, `mark_applied`.
- `Apply { resolution: Approval(Approve | ApproveAlways) }` → `mark_applied`
  **first**, then re-execute: open a step under the Resume turn and call
  `ToolExecutor::execute` with the persisted `tool_use` block's name and
  params and `PreDecided(decision)` so the gate short-circuits without
  raising (`ApproveAlways` also seeds `approved_resources`, as live); append
  the result through the same post-processing helper the batch loop uses
  (`agent_loop.rs:1687-1794`, factored out). The original turn's step/span
  rows were closed by recovery; the re-execution gets fresh ones. Only
  resumable rows ever reach `Apply`; a resolved non-resumable row is stamped
  applied by `settle_unapplied` with nothing appended, which is what keeps a
  Bash command the user approved mid-run from running a second time on the
  user's next message.

When a door calls the settlement no *live* ask can exist for the session —
an actor runs one turn at a time — so `settle_unapplied` only ever meets
dead entries; a live one is a bug and is logged, not touched. For the
issue-run requeue the settlement runs under a `SettleOnly` Resume turn and
the real `IssueRun` turn with its `IssueRunWaiter` follows; the approved
call executes exactly once, under the Resume turn, and `finish_run` fires
once, for the run.

#### Answering a dead ask: the Resume turn

Only the `Router` can spawn an actor (`router/mod.rs:254`), so the dead path
must reach it. The manager holds `Arc<dyn ResumeSink>`
(`fn request_resume(&self, ResumeRequest)`), implemented in `baybo-agent`
over `mpsc::Sender<RouterInbound>` with a new
`RouterInbound::ResumeAsk(ResumeRequest)` variant (the enum lives in
`baybo-channels`, `types.rs:47`). The router routes it like
`route_one_to_actor`, through `resolve_spawn_pins`, so a hydrated actor keeps
its LLM pin. The actor receives `AgentMessage::ResumeAsk { ask_id }` at
`Trigger` priority.

`handle_resume_ask`: if `manager.get(ask_id)` is already applied (a door's
settlement got there first, or a duplicate route) → return, nothing to do.
Otherwise `run_agent_loop(TurnInput::Resume { resumed_turn_id, ask_id, tool_use_id }, SettleMode::Continue, …)`
exactly like `handle_user_input`, including `send_user_reply`. The
settlement applies the answer; the first LLM iteration then runs on a window
whose tail is `assistant(tool_use) + tool_result` — a legal starting state,
the same shape `drive_background_notification_turn` already starts from.

`TurnInputKind::Resume` is a chat turn (`is_chat_turn` true, `/stop` cancels
it, `TurnState` shows it), attended, `memory_recall_query → None`, no title
pass (the session has one), failure notices user-facing (one helper,
`TurnInputKind::is_user_facing`, replaces the `UserChat` match at
`actor/mod.rs:606`; the `UserChat` match at `agent_loop.rs:1028` gates only the
title pass and stays `UserChat`-only), included in the push dispatcher's
kind filter (`push/mod.rs:516`) — the user answered and may have walked away.
`parent_turn_id = None`; the link is in the payload. Exhaustive matches to
extend: `input_kind()`, `turn/src/store.rs` (`turns.kind` = `"resume"`),
`query/src/lib.rs` `turn_input_text` (no title text), `dto.rs:615-624`,
`web_trace_types_sync.rs:312-320` with `app/web/src/types/trace.ts:403-419`.

A `ResumeAsk` can be lost in flight without a crash: the reaper's
`ActorStop` is served while the registry entry is still occupied
(`supervisor.rs:633-637`, `actor/mod.rs:377-394`), so a message routed into
that window is dropped with the mailbox. The row is `resolved`, not
`applied`, so nothing is lost: the next turn on the session (the user's next
message, a notification) applies it through the prelude, and the next boot
re-drives it through `reload_pending`. A live `UserInput` lost in the same
window is today's behaviour and the user resends; an ask answer needs no
resend.

#### A new message while a dead ask is pending

A user who comes back after a restart and types a message instead of
answering the card: the door's settlement abandons the pending ask and
appends the synthetic result before the user row, the `AskClosed(abandoned)`
event dispatches `Frame::AskResolved` so open tabs drop the card, and the
user turn proceeds. If a tab answered the ask a moment earlier, the
settlement's claim loses, `settle_unapplied` returns `Apply` instead, and
the answer lands before the user row — adjacent either way. Live asks are never touched here:
a message that reaches the actor while a *live* ask is parked is a mailbox
interjection, drained at the next iteration boundary as today
(`agent_loop.rs:1128-1151`). The composer rule in
[Composer text answers the question](#composer-text-answers-the-question) is
what keeps a typed answer from becoming such an interjection.

### The outer tool deadline pauses while parked

`tokio::time::timeout(ctx.timeout + APPROVAL_HEADROOM, tool.execute(..))`
(`tool_executor.rs:1051-1058`) is the only wall-clock bound on a tool body and
would kill a parked `DuringExecute` approval at `max_timeout + 300 s`. It is
replaced by a `PausableDeadline`: `ApprovalHandle` and `QuestionHandle` share
a `ParkGauge` (`Arc<AtomicUsize>`) they increment for the duration of a parked
request; the deadline future accrues elapsed time only while the gauge reads
zero. In unattended turns asks never park, so the gauge never moves.

The 300 s headroom served two purposes and only one goes away. Bash's own
command deadline defaults to `ctx.timeout` and its default `on_timeout` is
`background`: the detach-and-register step runs *after* the inner deadline
fires (`bash/mod.rs:798-801, :928-929, :1199-1292`), and the in-tree comment
at `bash/mod.rs:88-91` names the executor's slack as what keeps the outer
timer from racing that handoff. So the budget becomes
`ctx.timeout + TOOL_DEADLINE_GRACE` with `TOOL_DEADLINE_GRACE` a small named
constant (seconds, not minutes) documented as handoff slack, cited from the
Bash comment. The approval-mirroring part of the headroom is what is deleted.

### The `AskUser` tool

`ASK_USER_TOOL_NAME = "AskUser"` (PascalCase like `Read`, `WebSearch`,
`TaskCreate`). The `AskUserQuestionTool` `todo_tool!` stub
(`crates/tools/src/builtin/todo.rs:63-68`) is removed and the `tools.md` row
rewritten. Lives in `baybo_human_ask::tools`, registered from
`crates/baybo/src/runtime.rs` via `agent_tools(manager)` like the task tools.

Input schema (`parameters_schema`), limits as named consts in
`baybo_model::human_ask` (`MAX_QUESTIONS = 4`, `MIN_OPTIONS = 2`,
`MAX_OPTIONS = 4`, `MAX_HEADER_CHARS = 12`):

```jsonc
{
  "questions": [                       // 1..=MAX_QUESTIONS, one tab each
    {
      "header": "Auth",                // tab label, <= MAX_HEADER_CHARS, unique within the call
      "question": "Which auth scheme?",
      "options": [                     // MIN_OPTIONS..=MAX_OPTIONS
        { "label": "JWT", "description": "stateless, multi-client" },
        { "label": "Session", "description": "server-side state" }
      ],
      "multi_select": false
    }
  ]
}
```

Validation failures (counts, empty strings, duplicate headers) return a
`ToolOutput::Error` and raise no ask. "Other" (free text) is **always**
offered by the UI and not controllable by the model; a model-chosen option
set that does not fit is the exact case it exists for. No `preview` field in
v1 (reserved as an optional addition).

Result, `ToolOutput::Json`, serialized verbatim into the `tool_result`:

```jsonc
{ "answers": [ { "header": "Auth", "selected": ["JWT"], "other": null } ] }
// a tab the user did not answer: { "header": "DB", "selected": [], "other": null, "skipped": true }
// declined as a whole:
{ "declined": true, "note": "the user declined to answer; proceed on your best judgment or ask in plain text" }
```

`Declined` is a *result*, not `ToolError::Denied`, so it never inherits the
approval "denied / do NOT retry" framing (`agent_loop.rs:1706-1716`) and the
error type-erasure at `tool_executor.rs:1174-1192` is irrelevant to it.

**Visibility** uses the two existing session-fixed axes, so the tool list stays
byte-stable per session (the prompt-cache property `registry.rs:265-281`
protects): `ToolManifest.channels = vec![ChannelType::owner()]` (precedent:
`put_blob`, deck tools) and a new `ToolTriggerScope::UserConversation`
(`TriggerSource::User` only). That hides it from cron sessions (including a
user's reply inside a cron conversation — acceptable), issue sessions, and,
via the channel door, every subagent (channel `subagent`) and sidecar.
Belt-and-braces at execution: `ToolContext.question` is `Some` only in
attended turns; `AskUser` with `None` returns `declined` with a note that
this turn runs unattended, mirroring the `approval: None ⇒ fail-closed`
contract (`lib.rs:365-371`). The executor's channel/trigger doors already
mirror the registry filters.

**`Tool::sole_call()`** — new trait hook, default `false`, `AskUser` returns
`true`; `ToolRegistry::sole_call(name)` wraps it like `concurrency(name)`.
The agent loop checks the **resolved** dispatch names (`DispatchedToolCall`,
so a `ToolInvoke { name: "AskUser" }` envelope is caught) after the assistant
row is persisted: if a sole-call tool shares the response with any sibling,
that call is not executed and gets
`ToolOutput::Error("AskUser must be the only tool call in its response; put all questions in one call's `questions` array and wait for the answer before acting")`;
the siblings run normally. The permit pool cannot express this (an
`Exclusive` call queues behind siblings rather than rejecting them), so it is
a pre-dispatch batch check. `AskUser` is also `Exclusive`, which costs
nothing when it is alone. The tool description states the rule so the model
rarely trips it.

**Execution**: validate → build `QuestionRequest` →
`ctx.question.ask(req, cancel)` (the handle persists the row, enqueues, wakes
the channel, bumps the park gauge, awaits the oneshot; no timeout) → map
`AskResolution` to the JSON above. The outcome is also recorded as
`SpanEventKind::Question { outcome }` on the tool span, the sibling of
`SpanEventKind::Approval`, so traces show what the human did.

**Step summary** — `ask_step_summary(input: &Value, result: &Value) -> Option<AskStepSummary>`
(in `baybo-human-ask`, typed: question count + per-tab `header` with selected
labels / other / skipped, or declined) is the one derivation used by both the
live plane and REST: the loop computes it at `emit_tool_completed`
(`agent_loop.rs:~1643`, where `dispatch.arguments` and `executed.output` are
in hand) and it rides `AgentEvent::ToolCompleted.ask` → `Frame::ToolCompleted.ask`
→ the in-flight buffer → `WireWorkStep.ask`; the REST reconstruction
(`chat.rs:3557-3669`) computes it from `ToolUse.input` + `ToolResult.content`
into `ChatWorkStep.ask`. Without the live field every tab but the one that
answered would show `summary: "ok"` until a reload (the in-flight buffer
carries only `ToolStarted`/`ToolCompleted`, `work_steps.rs:36-83`).

### Composer text answers the question

Decided in the interview: while a question card is showing, a message typed
in the composer is the "other" answer of the tab the user is looking at; any
other unanswered tab is submitted as skipped.

This has to be a **client-side** rule. Interjections are drained only between
iterations (`agent_loop.rs:1128-1151`); while `AskUser` is parked the loop is
inside the tool batch, so a `Message` frame would sit in the mailbox until
the question resolved some other way. The web composer therefore sends
`Frame::AnswerQuestion` instead of a `Message` whenever its view has a
pending question (live or dead — a dead one resumes the turn), bypassing
`sendToSession` (no optimistic row, no outbox enrolment) but still committing
input history and clearing the draft. **Skip** is the only way to send a
plain message while a card is up. A `Message` that does reach the server
during a live question (another client, a sidecar) stays an interjection, as
documented in `docs/web-chat.md`.

`decideComposerAction` gains two inputs, `isCommand` and `hasPendingQuestion`;
its outputs stay `noop | stop | answer | direct | park` (no `command`
action exists or is added). Order: `noop → stop → answer → direct | park`,
with `isCommand` only vetoing `answer`. `/stop` must outrank `answer` (it is
what abandons a live ask). Any other leading-`/` input falls through to the
existing branch: **parked** while the question is live (the turn is active,
so `busy` is true — and the actor would hold a slash at the mailbox head
anyway, `actor/mod.rs:397-410`), **direct** for a dead question. The hint
says a command will run once the question is resolved. Staged attachments
cannot ride an answer; the send button is disabled with a hint while
attachments are staged and a question is pending.

### Wire surface

Approval frames are kept **byte for byte** (`ApprovalRequested`,
`ApprovalResolved`, `ResolveApproval`, `ApprovalCard`,
`SubscribeState.pending_approvals`): the TUI arms, the iOS native card and
the iOS web sentinel's dropped-key check all pin them. New frames:

```rust
Frame::QuestionAsked   { ask_id, tool_call_id: String, session_id, user_id, questions: Vec<WireQuestion> }  // session-scoped
Frame::AskResolved     { ask_id, outcome: AskOutcomeKind /* answered | declined | abandoned */ }            // session-less, any kind
Frame::AnswerQuestion  { ask_id, reply: QuestionReply /* Answered { answers: Vec<WireAnswer> } | Declined */ } // client → server
Frame::ToolCompleted   { …, ask: Option<AskStepSummary> }                                                   // additive optional field
SubscribeState { …, pending_questions: Vec<QuestionCard> }   // #[serde(default, skip_serializing_if = "Vec::is_empty")]
WireWorkStep   { …, ask: Option<AskStepSummary> }
SessionPatch   { …, awaiting_human: Option<bool> }           // renamed from approval_pending; any pending ask kind
```

- `AskResolved` is kind-agnostic on purpose: it is the only way to drop an
  **approval** card on abandon. `ApprovalResolved` carries an
  `ApprovalDecision`, which must not be widened, and today an abandoned
  approval broadcasts nothing — the web clears the card only on
  `approval_resolved` (`ChatPage.tsx:4239-4245`), so after `/stop` or
  "moved on" every open tab would keep an answerable card whose
  `ResolveApproval` the manager rejects. `AskResolved` is emitted on every
  `AskClosed` event; `ApprovalResolved` is still emitted for an answered
  approval as today. Clients drop the card by `ask_id == call_id`. iOS drops
  nothing on it until PR 3 (stale card after an abandon — listed under
  Deferred).
- Frames are dispatched by **queue watchers the gateway installs** in
  `build_channel`, the same place the `SessionPatch` watcher lives today
  (`boot.rs:184-200`): `AskRaised(entry)` → `SessionEvent::ApprovalRequested` /
  `QuestionAsked`, `AskClosed { session_id, ask_id, kind, outcome }` →
  `SessionEvent::AskResolved { session_id, ask_id, outcome }` (+
  `ApprovalResolved` when answered). The `SessionEvent` keeps the session so
  `session_id()` stays infallible (`types.rs:306-313`) and Subscribed
  delivery can find its subscribers; `session_event_to_frame`
  (`adapter.rs:394-437`) drops it, as it does for `ApprovalResolved` today.
  That gives raise and resolve one dispatch home, lets the settlement's
  abandons reach open tabs without the agent crate knowing channels, and
  removes the manual `broadcast_approval_resolved` from the `route.rs` arm.
- `Frame::routing_session_id`: `QuestionAsked` joins the session-scoped arm,
  `AskResolved` / `AnswerQuestion` the `None` group — same split as the
  approval trio, so iOS's pump routes them to the right transcript sink with
  no iOS change.
- A client→server frame without an arm **closes the socket**
  (`route.rs:586-592`), so the `AnswerQuestion` arm lands in the same PR as
  the web sends it.
- `SessionEvent::{QuestionAsked, AskResolved}` in `baybo-channels` with arms
  in the three exhaustive matches (`types.rs:306-325`, `adapter.rs:394-437`,
  `session_pulse.rs:74-97` — excluded from the pulse like approvals).
- Adding `ask` to `Frame::ToolCompleted` breaks the TUI's full struct
  literals (`tui/src/client/transport.rs:584`, `bin/chat_smoke.rs:208`);
  they gain `ask: None`.
- ts-rs: `#[cfg_attr(feature = "ts-export", …)]` on every new type,
  `ts(optional)` on every `Option`, enums outside `wire` typed as string;
  regenerated bindings under `sidecars/sdk/channel-ts/src/generated/` are
  committed with the Rust change (`scripts/check-ts-bindings.sh`).
- REST: `ChatSessionSummary` and `IssueDto` gain `awaiting_human`;
  `approval_pending` is **kept as a deprecated twin on both DTOs until PR 3**
  (same value), because iOS reads it with `#[serde(default)]` and not only
  for the chat-list mark — `IssueStore.swift:670-674` uses the issue bit to
  *authorize* the card's approve/deny controls and `ProjectsStore.swift:555-560`
  to fill the Waiting-on-you strip. With the self-deny gone, an approval the
  phone cannot show would otherwise park until someone opens the web. The
  `SessionPatch` wire key is renamed outright (option C of the interview:
  the patch-driven list mark on an installed phone goes dark until PR 3; the
  REST twin keeps the cold-start mark and the board working). `docs/openapi.json`
  and `app/web/src/api/schema.d.ts` are regenerated in PR 1; `IssueDto`'s new
  required field touches the 13 `Issue` test fixtures under
  `app/web/src/pages/projects/*.test.ts(x)`.
- The sidecar SDK runner's switch has no `default` (`runner.ts:662-764`);
  question frames reaching a multiplexed sidecar are silently dropped. They
  cannot reach one — `AskUser` is owner-only — but the SDK docs say so.

### Web UI

One view model for both kinds: `SessionView.pendingAsks: PendingAsk[]`
(discriminated on `kind`) replaces `pendingApproval: PendingApproval | null`.
Removal is by `askId`, never "the current card". The transcript foot renders
the **oldest** pending ask; the card shows "+N waiting" when more are queued.
`transcriptTailSignature` hashes the joined ask ids so a second card arriving
or one being answered re-pins the scroll.

`applySubscribeState` builds `pendingAsks` from
`pending_approvals ∪ pending_questions`. The work-step **badge is derived at
render time** from `pendingAsks × step.toolCallId`, and that is its only
home: `WorkStep.awaitingApproval`, `markStepAwaitingApproval`
(`ChatPage.tsx:116, :4567-4573`) and the `awaitingByCall` re-derivation in
`applySubscribeState` (`:5810-5820`) are deleted; `resolveStepApproval` keeps
writing the `approval` label. Mutating steps when a frame lands cannot work
after a restart: with `turn.active == false` the bundle carries
`work_steps: []` and the step only exists once the REST page arrives, with
no ordering between the two (`route.rs:697-701`, `ChatPage.tsx:1725-1729`).
The render path is a pure function of `step` today (`WorkStepView({ step })`
`:6733` ← `WorkMachineryRun` `:6964` ← `ThreadView` `:6393`), so `ThreadView`
gains `pendingAsks?: readonly PendingAsk[]` (default `[]`; the projects
`RunTranscriptPanel.tsx:538` mount passes nothing), forwarded through the
work-block row to `WorkStepView`, and `isCancelledWorkAt(transcript, i, turn, pendingAsks)`
(`:7164`) suppresses the `CancelledTurnIndicator` for a block that owns a
pending ask's step — after a restart the turn row is cancelled and the
trailing block is `workActive: false`, which today renders a "Cancelled" pill
directly above a card that says the turn is waiting on the user.

`QuestionCard` beside `ApprovalCard`, same `connected` / `submittingRef`
pattern:

- tab strip of `header`s, answered tabs ticked; radio for single, checkbox
  for `multi_select`; a fixed "Other…" input per tab;
- **Submit** enabled once every tab has a selection or other-text; **Skip**
  sends `Declined`;
- keys: `←/→`/`Tab` switch tabs, `1–4` pick, `Enter` submits — inactive
  while the other-input has focus; the active tab index lives per `askId` in
  `SessionView` so `handleSend` can read it;
- composer placeholder "Typed text answers "<header>" as Other" and the send
  button title "Answer (Enter)" while a question is pending.

Work step: "waiting for your answer" while parked, spinner suppressed, and
`awaitingReply` cleared on `question_asked` so no "Working…" stacks above the
form. After the answer the step collapses to a summary
(`Asked 2 questions · Auth: JWT · DB: Postgres, Redis`; skipped tabs read
*skipped*; declined reads *declined*), expandable to the full questions with
the chosen options highlighted, rendered from `ChatWorkStep.ask` /
`WireWorkStep.ask` / `tool_completed.ask` — the one server-side derivation;
no new `ToolResultMeta` field, as decided, and the DTO never exposes raw tool
JSON. `tool_label` gets an `AskUser` branch (first header) like `SPAWN_KEYS`.

Sidebar: `pendingAskIds` from subscribed views as today, **plus**
`awaiting_human` mirrored from `SessionPatch` into `SessionSummary` so the
dot reaches sessions the tab never opened and survives a reload via the REST
list. `applySessionPatch` rebuilds the row field by field and compares a
fixed list for referential stability (`ChatPage.tsx:5913-5978`,
`docs/web-chat.md:115`): the new field must be copied in the `merged`
literal, added to the comparison, and mapped in both REST-list →
`SessionSummary` constructors (`:1186-1191`, `:1294-1299`) — a field left
out of that list is silently destroyed by the next title or pin patch. Dot
title "Waiting for you".

`applyToolCompletedStep` no longer retires a badge — with render-time
derivation the badge leaves when the ask leaves `pendingAsks` — and only
writes the `approval` label and `ask` summary; the "a timed-out gate
broadcasts no `approval_resolved`" rationale at `docs/web-chat.md:634` is
deleted with the behaviour it justified.

### Push and the session-list mark

`PendingEdge::Raised` carries `kind`. `ApprovalPushRelay` becomes
`AskPushRelay`: unchanged nag/backoff logic, built in `build_managers` (see
boot order), per-kind title — `APPROVAL_TITLE` as today,
`QUESTION_TITLE = "Baybo has a question"` — and a body that carries no
model-authored text ("Open the conversation to answer"; the approval body
keeps the tool name only, `push/approval.rs:63-76`). The pre-POST recheck
(`push/mod.rs:720-729`) consults the generic pending set.
`RENAG_AFTER_ABANDONED` keeps its value; `Abandoned` now only means `/stop`,
card close or moved-on, never a self-deny. Push is best-effort, as everywhere
in `push/`; the REST list is the cold-start truth.

The chat-list mark (`boot.rs:184-200` watcher → `SessionPatch.awaiting_human`)
fires for either kind.

### Idle reaper

`AgentSupervisor::new` takes `Arc<dyn LivePendingAsks>`
(`fn has_live_pending_ask(&self, &SessionId) -> bool`, over the in-memory
queue). `reap_idle` skips such sessions beside the in-flight-subagent skip
(`supervisor.rs:414-426`), keeping the actor warm so an answer hours later
takes the live path. Correctness never depends on it: a reaped-then-answered
session is just a dead ask, which the Resume door and the prelude handle.

### Observability

- Span events: `SpanEventKind::Approval` as today; new
  `SpanEventKind::Question { outcome }`.
- The `human_asks` row is the durable record of *how* an ask closed
  (`Answered` / `Declined` / `Abandoned` / `PolicyDenied`), when, and whether
  the turn consumed it; the transcript records *what* was decided.
- Logs record `ask_id`, `kind`, `phase`, never option text or params.

### Deferred

- **TUI**: `map_frame` gets `Frame::QuestionAsked` in the silent-drop group
  with `// TODO(human-ask): render AskUser questions inline like ApprovalChatEntry`
  (the `tui` channel never receives it — `AskUser` is owner-only), but
  `Frame::AskResolved { ask_id, .. }` is handled: `queue.drop_call(&ask_id)`
  beside the `ApprovalResolved` arm, because the TUI mirrors every approval
  card (`transport.rs:343-354, :390-402`) and an approval abandoned by
  another client would otherwise stay on screen with nothing to clear it
  now that the self-deny is gone. `ask: None` in the two `ToolCompleted`
  literals. No `TransportEvent` / `AppEvent` variants. The six approval-queue
  call sites (`app.rs:516-522`, `lib.rs:672-679`, `transport.rs:98-108, :343, :393`,
  `prompt_cmd.rs:212-213, :621`) adapt to the re-typed queue through the
  approval wrappers in [Data model](#data-model); `crates/tui/src/lib.rs:38`'s
  `pub use baybo_tools::ApprovalQueue` and the fully-qualified rustdoc link
  at `transport.rs:44` repoint to `baybo_human_ask::AskQueue`.
- **iOS**: compiles (`pump.rs:373` and the three `SessionPatch` test literals
  at `transport/tests.rs:261,284,309` follow the `awaiting_human` rename;
  `SessionListSink::on_approval_pending` keeps its name — it is a Swift
  protocol requirement). Unknown frames are already inert at every iOS
  layer, so an approval card abandoned server-side stays on screen until the
  next `subscribe_state`. Full support (question card, composer routing,
  `OutboundCmd::AnswerQuestion` beside `ResolveApproval`, a content builder
  beside `resolve_approval_frame`, an FFI export beside `chat_resolve_approval`,
  a second native fold beside `ApprovalQueue.apply`, `AskResolved` handling,
  the `onAwaitingHuman` rename with Swift, dropping the REST
  `approval_pending` twin, and refreshing `app/ios/docs/approvals.md` +
  `chat-list.md`) is PR 3.
- `preview` on options; questions on sidecar channels; a REST door for
  answering questions (WS only in v1); a deterministic shutdown that joins
  actor tasks (would make the `Ancestor` branch exact instead of best-effort).

## Data model

```rust
// baybo_model::human_ask
pub struct AskId(String);                       // uuid v4, Display/FromStr
pub enum AskKind  { Approval, Question }
pub enum AskPhase { BeforeExecute, DuringExecute }
pub fn resumable(kind: AskKind, phase: AskPhase) -> bool;
pub enum AskRequest {
    Approval(ApprovalRequest),                  // moved from baybo_tools::approval; tool_call_id: String
    Question(QuestionRequest),
}
pub struct QuestionRequest { pub questions: Vec<Question> }
pub struct Question { pub header: String, pub question: String, pub options: Vec<QuestionOption>, pub multi_select: bool }
pub struct QuestionOption { pub label: String, pub description: String }
pub struct QuestionAnswer { pub header: String, pub selected: Vec<String>, pub other: Option<String> }  // both empty ⇒ skipped
pub enum AskResolution {
    Approval(ApprovalDecision),                 // a human answered
    Answered(Vec<QuestionAnswer>),
    Declined,
    Abandoned,                                  // /stop, card close, user moved on
    PolicyDenied,                               // unattended turn
}
pub struct AskStepSummary { … }                 // what the work step shows after the answer
pub struct ResumeRequest { pub session_id: SessionId, pub ask_id: AskId }

// baybo_store::human_ask
pub struct HumanAskRow { ask_id, session_id, channel: ChannelType, tool_call_id: String, kind, phase, request: AskRequest,
                         created_at, resolved_at: Option<_>, resolution: Option<AskResolution>, applied_at: Option<_> }
#[async_trait] pub trait HumanAskStore: Send + Sync {
    async fn create(&self, row: &HumanAskRow) -> Result<()>;
    async fn resolve(&self, ask_id: &AskId, resolution: &AskResolution, now: DateTime<Utc>) -> Result<bool>;
    async fn mark_applied(&self, ask_id: &AskId, now: DateTime<Utc>) -> Result<bool>;
    async fn get(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>>;
    async fn list_unapplied(&self) -> Result<Vec<HumanAskRow>>;
    async fn list_unapplied_for_session(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>>;
}

// baybo_tools::ask  (tool-facing port, sibling of tools::approval)
pub struct TurnTokens { pub turn: CancellationToken, pub actor: CancellationToken }
pub enum CancelOrigin { Turn, Ancestor }        // CancelOrigin::of(&TurnTokens)
pub struct AskCancel { pub tokens: TurnTokens }
pub struct AskSite { pub channel: ChannelType, pub phase: AskPhase }   // which queue the row reloads into; which phase raised it
pub struct ParkGauge(Arc<AtomicUsize>);         // park() -> ParkGuard (RAII decrement)
#[async_trait] pub trait ApprovalGate { async fn request(&self, req: ApprovalRequest, site: AskSite, cancel: AskCancel) -> ApprovalOutcome; }
#[async_trait] pub trait QuestionGate { async fn ask(&self, req: QuestionAsk, cancel: AskCancel) -> AskResolution; }
pub struct QuestionHandle { gate: Arc<dyn QuestionGate>, tool_call_id: String, park: ParkGauge, cancel: AskCancel, last: Arc<Mutex<Option<AskResolution>>> }
// ToolContext gains `question: Option<QuestionHandle>`; `None` ⇒ fail-closed (declined). ApprovalHandle gains `park` + `cancel`.
// Tool gains `fn sole_call(&self) -> bool { false }`.

// baybo_turn::kind
TurnInputKind::Resume; TurnInput::Resume { resumed_turn_id: TurnId, ask_id: AskId, tool_use_id: String }
TurnInputKind::is_attended(self) -> bool; TurnInputKind::is_user_facing(self) -> bool

// baybo_human_ask
pub struct AskQueue;                            // was baybo_tools::ApprovalQueue; entries pair a data-only, Clone PendingAsk { ask_id, session_id, tool_call_id, kind, phase, request: AskRequest }
                                                // with a private responder: Option<oneshot::Sender<AskResolution>> (None = dead or mirror)
pub struct AskQueues;                           // Arc<parking_lot::Mutex<HashMap<ChannelType, AskQueue>>>; get_or_create(&ChannelType) -> AskQueue (a few ops per boot/connect — no DashMap)
pub enum PendingEdge { Raised { ask_id, kind, tool }, Answered, Abandoned }
pub enum AskEvent { Raised, Resolved(AskResolution) }               // observer events (async, from verbs)
pub enum AskQueueEvent { AskRaised(PendingAsk), AskClosed { session_id, ask_id, kind, outcome: AskResolution } }  // sync queue watchers (frame dispatch)
pub struct AskCore { store: Arc<dyn HumanAskStore>, observers: parking_lot::RwLock<Vec<Arc<dyn AskObserver>>> }  // add_observer(&self, ..) — runs after the gates already share the core
pub struct ChannelAskGate { queue: AskQueue, waker: Arc<dyn Fn(PendingAsk)>, core: Arc<AskCore> }   // impl ApprovalGate (+ QuestionGate in PR 2); stamps applied_at after the oneshot yields
pub struct PolicyDenyGate { core: Arc<AskCore> }                    // impl ApprovalGate
pub struct HumanAskManager;                     // from_config(HumanAskManagerConfig { core, queues: AskQueues, resume_sink: Arc<dyn ResumeSink> })
                                                // verbs: resolve, abandon, abandon_for_session, abandon_for_tool_call, settle_unapplied, mark_applied, reload_pending, get,
                                                //        unapplied_tool_use_ids, has_live_pending_ask (LivePendingAsks impl)
pub enum Settled { Abandoned { tool_call_id, kind }, Apply { ask_id, tool_call_id, kind, resolution } }
pub enum ToolCallAbandon { NoAsk, Abandoned, Answered }   // abandon_for_tool_call's answer to the loop
pub struct ReloadReport { requeued, resumed, abandoned, applied }
#[async_trait] pub trait ResumeSink {
    async fn resumes_on_answer(&self, session: &SessionId) -> bool;   // false for TriggerSource::Issue sessions
    fn request_resume(&self, req: ResumeRequest);
}
pub trait LivePendingAsks { fn has_live_pending_ask(&self, session: &SessionId) -> bool; }
#[async_trait] pub trait AskObserver { async fn on_event(&self, ask: &HumanAskRow, event: AskEvent); }
pub fn ask_step_summary(input: &Value, result: &Value) -> Option<AskStepSummary>;
pub mod tools { pub struct AskUserTool; pub fn agent_tools(manager) -> Vec<(Arc<dyn Tool>, ToolManifest)>; }

// baybo_agent
pub enum SettleMode { SettleOnly, Continue }    // AgentLoop::settle_unapplied_asks(session, mode); AgentActor::settle_unapplied_asks() wraps SettleOnly in a Resume turn
struct UnappliedAsks(Arc<HumanAskManager>);     // impl baybo_context::PendingToolCalls — keeps human-ask free of a context edge
```

`ApprovalSurface` on `Channel` becomes
`AskSurface { approval_gate: Arc<dyn ApprovalGate>, question_gate: Arc<dyn QuestionGate>, queue: AskQueue }`
(PR 1 ships `{ approval_gate, queue }`; `question_gate` joins in PR 2);
`Channel::pending_approvals / pending_approval_sessions / resolve_approval /
dispatch_approval_*` get kind-aware siblings.

The TUI and `baybo prompt` use `AskQueue` as a display mirror of approval
cards. Its approval-typed surface survives as wrappers on the re-typed queue,
importing from `baybo_human_ask`: `enqueue_mirror(ApprovalRequest)`
(synthesizes `PendingAsk { ask_id: call_id, kind: Approval, phase: BeforeExecute, responder: None }`),
`peek_head_approval() -> Option<ApprovalRequest>` (projects
`AskRequest::Approval`, `None` for another kind), `list_approvals()`,
`resolve_head(ApprovalDecision)`
(wraps into `AskResolution::Approval`), `set_resolver` with
`ResolveFn = Fn(AskId, AskResolution)` (the TUI resolver matches
`Approval(d)` and ignores other variants), `drop_call`, `new`.

### Implementation notes (PR 1)

Where the code differs from the sketch above, and why:

- **`AskSite`.** The ledger row needs the channel (which queue a pending row
  reloads into) and the phase (resumability), and neither is on
  `ApprovalRequest`. Rather than widen the payload, every gate call carries
  `AskSite { channel, phase }`: the executor passes `BeforeExecute` and the
  session user's channel; `ApprovalHandle` passes `DuringExecute` and the
  requesting user's channel. `PolicyDenyGate` needs the channel too, since it
  is built once and serves every channel.
- **Issue sessions and the resume sink.** The manager learns whether a session
  resumes through the chat Resume door from the `ResumeSink` port itself
  (`resumes_on_answer`), implemented in `baybo-agent` over the session's
  `TriggerSource`. One port, no extra config field, no schema change.
- **Interim wiring.** Until the gateway's resolve doors move onto
  `HumanAskManager`, `Channel::resolve_approval` still resolves through the
  queue alone (no ledger claim). `build_managers` creates the router intake
  next to `AskCore`/`AskQueues`, builds the `HumanAskManager` right after
  `recover_orphaned_traces_and_turns` (it needs the `SessionManager` for the
  sink's issue check) with `baybo_agent::runtime::asks::RouterResumeSink` as
  its `ResumeSink`, and calls `reload_pending` there (best-effort, logged).
  `AskPushRelay` does not move yet, so a reload's `Raised` edges reach no push
  relay until the gateway stage moves it ahead of the manager.
- **The settlement.** `TurnAsks` grew `unapplied`, `settle_unapplied`,
  `mark_applied` and `get` (the manager gained the public
  `unapplied_for_session` to answer the first). The approval re-execution
  passes `PreExecuteGate::PreDecided(decision)` to `ToolExecutor::execute`
  (`PreExecuteGate::Ask` everywhere else) and runs under a fresh
  `StepKind::LlmIteration` step that carries the tool span but no `LlmCall`
  span — no new step kind, so no trace-viewer mirror changes. A settle-only
  Resume turn completes with `TurnOutput::Structured { "settled_asks": n }`
  (no reply ordinal, so no push). `resumed_turn_id` is derived as the
  session's newest turn created no later than the ask (the row carries no
  turn id); a session with no turns at all is logged and skipped. A
  settlement error fails the door (the door's own append does not happen);
  the background-notification door logs and retries on its timer. The
  `AskUser` result envelope for `Answered` / `Declined` / an abandoned
  question lives in `baybo_human_ask::result`, so PR 2's tool reuses it.
  `Settled::Apply` carries the row's `kind` as well: a dead ask abandoned by
  a verb (card close, `abandon`) is resolved-not-applied, and its
  `Abandoned` resolution alone cannot say whether the refusal is an approval
  denial or a declined question.
- **One more door.** `publish_cron_notification` (a one-shot cron result
  delivered into its origin conversation, or a fire's own outcome row) also
  runs the prelude: it appends an assistant row, which after a dangling
  `tool_use` wedges the transcript just as a user row does.
- **The loop's port.** The agent loop does not hold `Arc<HumanAskManager>`:
  `AgentLoopConfig.asks` is `Arc<dyn baybo_agent::runtime::asks::TurnAsks>`,
  a narrow port implemented for `HumanAskManager` in `baybo-agent`. It carries
  `abandon_for_tool_call` and the settlement verbs listed above. On a store
  error `abandon_for_tool_call` answers `NoAsk`
  (the result is appended, so the session's next LLM call is not wedged by a
  dangling `tool_use`). The cancelled-batch path decides per call from
  `ExecutedTool.abandoned` (the call's last ask resolved `Abandoned`, read
  from the new `ApprovalHandle::last_outcome`) or a missing result, and
  claims for such a call whenever the turn token tripped or the call came
  back abandoned — not only when the batch select observed the cancel, since
  a gate can return `abandoned()` and the call finish before the loop's
  select sees the token.
- A call skipped because its claim lost (`Answered`) or because the cancel
  is `Ancestor` gets neither the append nor the `tool_completed` event; the
  step's completion arrives with the result the next settlement appends.
- **`PausableDeadline`** polls the `ParkGauge` every 250 ms
  (`PARK_POLL_INTERVAL`); `TOOL_DEADLINE_GRACE` is 15 s.
- `ASK_USER_TOOL_NAME` already lives in `baybo_model::human_ask`, because
  `PendingEdge::Raised.tool` names it for question asks.

## Control flows

**Live approval, attended turn**

1. executor computes uncovered accesses → `gate.request(ApprovalRequest, cancel)`
   (the executor no longer races the token itself)
2. `ChannelAskGate`: `core.create(row)` → observer `Raised` → `queue.push(entry)`
   → `Raised` edge + `AskRaised` watcher → gateway dispatches
   `SessionEvent::ApprovalRequested` → wait
3. user clicks → `Frame::ResolveApproval` → `route.rs` arm →
   `manager.resolve(ask_id, Approval(decision))`: claim → observer
   `Resolved` → remove entry → `Answered` edge + `AskClosed` watcher (gateway
   dispatches `ApprovalResolved` + `AskResolved`) → oneshot
4. the gate stamps `mark_applied` as the oneshot yields, then returns the
   outcome; executor continues as today

**Live question**

1. loop: batch check — `AskUser` alone? else error result, siblings run
2. `AskUserTool::execute`: validate → `ctx.question.ask(QuestionRequest, cancel)`
3. `ChannelAskGate`: row → push → `Raised { kind: Question }` →
   `SessionEvent::QuestionAsked` → `Frame::QuestionAsked`; web shows the card,
   badges the step, clears `awaitingReply`
4. user answers (card or composer) → `Frame::AnswerQuestion` →
   `manager.resolve(ask_id, Answered(..) | Declined)` → `AskResolved` broadcast
5. the gate stamps `mark_applied`; tool returns `ToolOutput::Json`; loop
   appends it, `tool_completed { ask: summary }` labels the step everywhere

**Gateway restart with a parked ask**

1. shutdown: ancestor cancel → gate stays parked → loop drops the batch,
   emits and appends nothing; entry orphaned; row pending
2. boot: store → recovery → channels + relay → manager → `reload_pending`:
   resumable rows re-enqueued dead + `Raised` (push fires for sessions with
   no subscriber); non-resumable rows `Abandoned`
3. web reconnects → `SubscribeState.pending_*` carries the card; REST list has
   `awaiting_human: true`
4. user answers → `manager.resolve` → claim → entry removed (the row is now
   `resolved`, store-only) → `AskResolved` → `ResumeSink` →
   `RouterInbound::ResumeAsk` → spawn/hydrate actor — hydration asks the
   **store** for unapplied ids and skips the fill for this `tool_use`
5. actor: `handle_resume_ask` → Resume turn (`Continue`) → settlement
   applies the answer (append, or `mark_applied` + re-execute) → first LLM
   iteration → reply streams as usual

**`/stop` while parked**

1. `handle_stop` → `TurnLifecycle::cancel(UserStopped)` → turn token
2. gate's cancel arm: `CancelOrigin::Turn` → returns `abandoned()` at once;
   `orphan` on the way out → executor returns `Denied`
3. loop's post-processing, per call: `abandon_for_tool_call` claim →
   `Abandoned` persisted, entry removed, `Abandoned` edge,
   `AskResolved { abandoned }` dispatched → then the denied result (or the
   cancelled fill for a call that never finished) is appended

**New message, dead ask pending** — door: `settle_unapplied_asks()`
(`SettleOnly` Resume turn: synthetic or real result appended) → user row →
user turn. See [above](#a-new-message-while-a-dead-ask-is-pending).

**Unattended turn hits a gate** — `PolicyDenyGate` writes an already-applied
`PolicyDenied` row, returns `policy(Deny)`, executor returns
`ToolError::Denied` with `refusal_reason(Policy)`, no queue entry, no frame,
no wait. Identical for `BeforeExecute` and `DuringExecute`.

## PR plan

Three draft PRs, in order, each green on `cargo clippy --all --benches --tests
--examples --all-features`, `scripts/check-rustdoc.sh`,
`cargo nextest run --workspace`, `scripts/check-ts-bindings.sh`, the web
gates, **and the `app/ios` gates run by hand** (`cd app/ios && cargo clippy
--workspace --all-targets --all-features -- -D warnings && cargo nextest run
--workspace`; `cd app/ios/web && pnpm lint && pnpm test && pnpm build`) — the
root gates never build `app/ios`, and CI's iOS jobs only run once the PR
leaves draft.

**PR 1 — foundation: `baybo-human-ask`, durable approvals, no timeout**

- `baybo-model::human_ask` + `ApprovalRequest` move (`tool_call_id: String`);
  `baybo-store::human_ask`; `sqlite/human_ask.rs` + DDL; `Store.human_ask`.
- `crates/human-ask`: `AskQueue` (moved), `AskCore`, `ChannelAskGate`
  (approval half, origin-aware cancel), `PolicyDenyGate`, `HumanAskManager`
  with every verb, `reload_pending`, observers, resume sink,
  `MemoryHumanAskStore`.
- `baybo-tools::ask`: `TurnTokens`, `CancelOrigin`, `AskCancel`, `ParkGauge`;
  `ApprovalGate::request` takes `AskCancel`; `ApprovalHandle` takes a
  `String` id + gauge + cancel.
- `baybo-turn`: `is_attended`, `is_user_facing`, `TurnInputKind::Resume` +
  `TurnInput::Resume` + the four exhaustive mirrors.
- `baybo-agent`: `attended` replaces `InheritedToolContext` (deleted, all
  sites listed under Attendedness); `TurnTokens` threaded from
  `run_agent_loop`; `ToolExecutor::new(…, policy_deny)`; `PausableDeadline`
  + `TOOL_DEADLINE_GRACE`; `Ancestor` branch appends/emits nothing, `Turn`
  branch claims `abandon_for_tool_call` before each append;
  `AgentActor::settle_unapplied_asks()` called by the five doors before their
  append and by `/compact`, `AgentLoop::settle_unapplied_asks(session, mode)`
  with `SettleMode`; `PreDecided` re-execute; `ResumeSink` impl;
  `AgentMessage::ResumeAsk` / `handle_resume_ask`; `PendingToolCalls` port in
  `baybo-context` + the `UnappliedAsks` newtype +
  `repair_tool_pairing(messages, skip)`; reaper probe.
- `baybo-channels`: `AskSurface`; `RouterInbound::ResumeAsk`;
  `SessionEvent::AskResolved { session_id, .. }`.
- `baybo-project`: `TimelineAskObserver` (async, `Weak<ProjectManager>`) +
  `AskCloser` behind the existing `CardPromptCloser` seam, wired by
  `install_ask_bridge`; `resolve` skips the resume sink for Issue sessions.
- `baybo-gateway` / `baybo`: boot order (store → recovery → core + queues +
  inbound channel → channels → relay → manager + project bridge → reload);
  `AskCore` + `AskQueues` through `GatewayDeps` / `WsChannelState` to the
  lazy install; queue watchers dispatch frames; delete `APPROVAL_TIMEOUT`;
  `Frame::AskResolved`; `SessionPatch.awaiting_human`; REST `awaiting_human`
  + deprecated twin; `route.rs` resolve → manager verb; projects REST resolve
  → manager verb, filtered to `Approval`; `Arc<HumanAskManager>` on
  `ManagerGraph` / `GatewayDeps` / `AdminState` / `WsChannelState`; OpenAPI +
  `schema.d.ts` regen + the 13 `Issue` fixtures.
- `baybo-tui`, `app/ios`: the six approval-queue call sites adapt to the
  wrappers, `AskResolved` drops the mirror, rename follow-ups, re-exports and
  rustdoc links; no new behaviour.
- Docs: this file; `agent.md` (`:124-144`, `:241-254`, `:290`, `:307`),
  `turn.md` (kind table, descriptors, recovery, queue policy; fix the
  `origin = Spawned` claim at `:107`), `context.md:17`,
  `channels.md:300-308`, `gateway.md:1025`, `cron.md:106-118`,
  `subagent.md:45-54`, `project.md:1825-1915`, `storage.md` (table + mapping
  + never-deleted note), `README.md` (index, graph, reading order),
  `testing.md:57`, `app/ios/docs/approvals.md:107-109` (the "restart drops
  every parked prompt / self-denies" sentence is server behaviour this PR
  changes).
- User-visible: approvals no longer self-deny; they survive a restart.

**PR 2 — `AskUser` + web**

- `baybo-tools::ask`: `QuestionGate`, `QuestionHandle`, `ToolContext.question`,
  `Tool::sole_call`; remove the todo stub. `ChannelAskGate` question half;
  `AskUserTool` + registration; `ToolTriggerScope::UserConversation`;
  `SpanEventKind::Question`; `ask_step_summary`; sole-call batch check.
- Wire: `QuestionAsked`, `AnswerQuestion`, `QuestionCard`, `pending_questions`,
  `ToolCompleted.ask` / `WireWorkStep.ask`; `SessionEvent::QuestionAsked`;
  `route.rs` `AnswerQuestion` arm; SubscribeState; push question title;
  ts-rs regen; TUI arms + TODO + `ask: None` literals.
- REST reconstruction: `ChatWorkStep.ask`, `tool_label` branch; OpenAPI +
  `schema.d.ts` regen.
- Web: `pendingAsks[]`, `QuestionCard`, render-time badge, cancelled-pill
  suppression, composer `'answer'` action with the stated precedence +
  per-ask tab state, step summary from `.ask`, sidebar `awaiting_human`
  through `applySessionPatch`'s explicit copy list, `chatWs.ts` +
  `wireSentinel.ts` mirrors.
- Docs: `tools.md` row, `web-chat.md` (`:108-109`, `:115`, `:204`, `:630-634`,
  `:669`, `:677`, `:678`, `:695`), `sync-protocol.md:279-293` and
  `gateway.md:983-986, :1017` (SubscribeState listing), `sidecars.md` note
  on dropped question frames.

**PR 3 — iOS** (separate; the seams kept open are listed under Deferred).

## Test plan

Layered per `docs/testing.md`.

Unit (`baybo-human-ask`, `MemoryHumanAskStore` where a store is needed):

- write-ahead: `create` is observed before `push`; a failing `create` raises
  no entry and no edge.
- resolve: resolution persisted before the oneshot fires; second resolve of
  the same ask → `AskClosed`; resolve of a dead entry removes it, clears the
  session mark, fires `Answered`, raises no push, keeps the id in
  `unapplied_tool_use_ids` (store-keyed), routes to the resume sink (not for
  an Issue session), never calls `send`.
- the gate stamps `applied_at` after the oneshot yields, for both kinds and
  both phases.
- `orphan` keeps a resumable entry (dead, no edge) and drops a non-resumable
  one (`Abandoned` edge).
- gate cancel: `Turn` returns `abandoned()` without awaiting and without
  touching the row; `Ancestor` never returns.
- `reload_pending`: pending resumable rows re-enqueued per `channel`
  (`get_or_create`, including a channel not yet installed), `Raised`
  published once per session; resolved resumable non-Issue rows request a
  resume and publish nothing (no `Raised`, no push); non-resumable pending
  rows abandoned and resolved ones applied; idempotent on a second call.
- `settle_unapplied`: pending dead → `Abandoned`; resolved resumable →
  `Apply`; resolved non-resumable → applied, nothing returned; live →
  untouched and logged; a row claimed by a concurrent `resolve` between the
  list and the claim comes back as `Apply`, not `Abandoned`.
- `PolicyDenyGate` writes an applied `PolicyDenied` row and no entry.
- edge-ordering tests from `approval.rs:1018-1060` ported verbatim; the
  `a_timed_out_gate_clears_the_session_mark` test becomes
  abandon-clears-the-mark; `a_dropped_request_future_clears_the_session_mark`
  becomes "a turn-cancelled request persists `Abandoned` before the Drop
  guard runs".
- observers receive `Raised`/`Resolved` from every verb and nothing from
  reload.

Unit (other crates):

- `baybo-store`/`baybo-storage`: round-trip; both claims return `false` the
  second time; `list_unapplied` skip-with-warn on an unknown `kind`.
- `baybo-context`: `repair_tool_pairing` with a skip set — no fill, no
  quarantine, fast path on a healthy-but-parked transcript, reposition still
  adjacent.
- `baybo-turn`: `is_attended` / `is_user_facing` tables; `Resume` in the
  TS-sync tripwire.
- `baybo-agent`: executor tests replacing the `inherited_context_*` suite
  (`tool_executor.rs:2476-2600`) with attended/unattended; `PausableDeadline`
  does not expire while parked; a tool that sleeps `ctx.timeout` then does a
  short post-deadline step completes normally (the grace); sole-call
  violation → error result for the ask, siblings executed; `Ancestor` cancel
  while parked → no `Denied` result, no span event, nothing appended, entry
  dead, row pending; `Turn` cancel → the loop's claim lands before the
  result append, and a losing claim appends nothing; the settlement applies
  each `Settled` variant with one adjacent result, under a `SettleOnly` turn
  from every door (user, merged, issue brief, cron fire, notification,
  `/compact`) and under `Continue` from the Resume door; `mark_applied`
  precedes re-execution on `Approve`.
- `baybo-tools`: `AskUser` validation table; `question: None` → declined.
- `baybo-wire`: round-trips for the new frames, `pending_questions` in the
  bundle, `awaiting_human` patch, `ToolCompleted.ask`.
- `ask_step_summary`: REST and live produce identical summaries for the same
  input/result.

Integration (`crates/integration-tests`, sqlite-backed harness):

- **restart**: raise an approval and, separately, a question; trip the
  parent token and await the actor's quiescence (not process exit); drop the
  graph; rebuild on the same db; assert `SubscribeState` carries the card,
  the REST list says `awaiting_human: true`, the push stream yields exactly
  one push for the unsubscribed session, the transcript still has the
  dangling `tool_use` (no fill, no `tool_completed`); answer; assert the
  Resume turn runs, the real `tool_result` is adjacent (never the
  "interrupted" fill), and for the approval the tool executed exactly once.
- **crash after the claim**: resolve a dead ask, drop the graph before the
  Resume turn runs, rebuild; assert `reload_pending` re-drives it and the
  answer lands.
- **shutdown vs stop**: `/stop` persists `Abandoned` + denied result;
  shutdown leaves the row pending and the transcript dangling.
- **dead ask + new message**: `Abandoned` persisted, synthetic result
  adjacent, `AskResolved` dispatched, the new turn completes against a strict
  provider fake. Variant: a second client answers the ask between the
  message arriving and the prelude running → the real answer lands, adjacent,
  once.
- **dead ask + background notification / `/compact` / issue-run requeue /
  cron fire in a cron conversation**: same guarantees through every door —
  result adjacent, strict provider fake accepts.
- **lazy channel**: an approval raised on an out-of-tree sidecar channel
  resolves through the manager; after a restart with the sidecar
  disconnected the row reloads into a queue the later install attaches to.
- **unattended**: a cron fire and a subagent hitting a gate both get
  `PolicyDenied` rows, no queue entry, no frame.
- **issue run**: an approval raised from an issue session reaches the
  timeline observer; after a restart the requeued run applies an answer
  given before it started, or re-asks live if none was; exactly one run
  attempt executes the approved call and `finish_run` is called once.

Web (vitest): `QuestionCard` (tabs, single/multi, other, submit enablement,
skip, keys), `decideComposerAction` precedence incl. `/stop`, a slash
command under a live question → `park` and under a dead one → `direct`,
`transcriptTailSignature` with two asks, render-time badge with
`turn.active = false` and the bundle arriving before the REST page,
cancelled-pill suppression, step summary rendering from `.ask`,
`workBlock.test.ts` labels for questions (the `awaitingApproval` cases
removed), `applySessionPatch` keeps `awaiting_human` across a title patch.

## Decisions for the owner

The interview settled the shape; the code read surfaced points where the
spec above picks an answer the interview did not, or contradicts it. Each is
implemented as written unless overridden.

1. **Issue runs keep their approval surface** (attended for approvals, never
   see `AskUser`; the boot requeue is their resume path). The interview's Q11
   wording listed "project runs" as unattended; the board's approval feature
   would die. Recommended: keep.
2. **REST keeps `approval_pending` as a deprecated twin of `awaiting_human`
   until PR 3.** Option C renamed everything now; the code read shows the
   REST issue bit also *authorizes* the iOS card's approve/deny controls and
   fills its Waiting-on-you strip, so a bare rename would make issue-run
   approvals unanswerable from the phone while the self-deny that used to
   unblock them is gone. The `SessionPatch` wire key is still renamed
   outright. Recommended: twin on REST only.
3. **Shutdown leaves asks pending; `/stop` abandons them.** This needs the
   actor token threaded into the executor (`TurnTokens`) and the gate to make
   the origin decision — a small but new seam, and best-effort on the
   shutdown side. The alternative — treat shutdown like `/stop` — means
   "survives a restart" only covers crashes, not `baybo gateway restart`,
   which is the common restart.
4. **Composer text answers a dead question too**, and *Skip* is the only way
   to send a plain message while a card is showing; a leading `/` is never
   an answer (it parks behind a live question, sends directly past a dead
   one). The alternative (dead question ⇒ composer sends a message, which
   abandons the question) is simpler but makes "I came back and typed my
   answer" silently discard the question.

(Earlier draft decision "user-rooted subagents become unattended" is
withdrawn: they are already auto-denied by the gate map's fallback for the
`subagent` channel; `is_attended` only gives that rule a home.)

## Constraints

- Rows in `human_asks` are never deleted by the runtime. No `cleanup_*` API.
- `baybo-context` receives `PendingToolCalls` (a verb), never the store or
  the manager. `baybo-tools` never depends on `baybo-human-ask`;
  `baybo-human-ask` never depends on `baybo-channels` or `baybo-context`. No
  `Arc<dyn HumanAskStore>` outside `baybo-human-ask`, except the
  `baybo-storage` `Store` bundle field and the single `stores.human_ask.clone()`
  read in `build_managers` that constructs `AskCore` (tests excepted);
  `baybo-gateway`, `baybo-agent`, `baybo-project` receive `Arc<AskCore>` /
  `Arc<HumanAskManager>` / `AskQueues`, never the store.
- Every exit from a row state is a verb that persists first through a claim.
  A dropped future orphans; it never resolves. The claim is the arbiter
  between concurrent verbs, and whoever appends to the transcript is the one
  who claims, in the same task, immediately before the append.
- Watchers stay sync and non-blocking; no sqlite inside a watcher; a gate's
  cancel arm never awaits. Observers are async and run from verbs only.
- Attendedness has one home (`TurnInputKind::is_attended`); resumability has
  one home (`resumable(kind, phase)`); unapplied-ask settlement has one home
  (`AgentLoop::settle_unapplied_asks`, reached from every door before its
  append and from `compact_now`); "is this step parked" has one home on the
  web (`pendingAsks × toolCallId`, at render time).
- `AskUser` is owner-channel, `TriggerSource::User` sessions, attended turns,
  sole call in its response.
- Approval wire frames and `ApprovalDecision` are not widened; questions and
  the kind-agnostic `AskResolved` are siblings.

## Collaboration

- **tools** — ports (`ApprovalGate`, `QuestionGate`, handles, cancel/park
  types, `sole_call`); see [`tools.md`](tools.md).
- **agent** — executor gates, loop batch rules, the prelude, Resume turn,
  actor/router doors, reaper; see [`agent.md`](agent.md).
- **turn** — `Resume` kind, `is_attended`; see [`turn.md`](turn.md).
- **context** — hydration skip set; see [`context.md`](context.md).
- **channels / gateway** — `AskSurface`, frames, queue watchers,
  SubscribeState, push, REST; see [`channels.md`](channels.md),
  [`gateway.md`](gateway.md), [`../web-chat.md`](../web-chat.md).
- **project** — timeline observer, card close, boot requeue as resume; see
  [`project.md`](project.md).
- **storage** — `human_asks`; see [`storage.md`](storage.md).
