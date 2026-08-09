# Approvals — merge the two gates onto one persisted record

> **Status:** decided, not started. **Target: one gate. The original
> `ChannelApprovalGate` becomes durable, `TimelineApprovalGate` stops being a separate gate,
> and the card and the chat become two renderings of the same record.** Finding 1 below is a
> live bug that this deletes rather than patches — do the merge, or patch it knowing the
> patch gets thrown away.

Found while removing `ApprovalGateMap::type_gate`, an accessor that existed for exactly one
caller: the boot-time wiring that wrapped the owner channel's gate in
`TimelineApprovalGate`. Pulling on that thread turned up why the wiring needed a bespoke
accessor at all — "a tool call is parked at the gate" is announced four times, hooked at
three layers, each seeing a different subset of the truth.

## The shape today

| Surface | Hook | Edges | Payload | Durable |
|---|---|---|---|---|
| In-session prompt | the gate's `waker` — `channel/boot.rs:154` | **in only** | full `ApprovalRequest` | no |
| Chat-list "waiting for you" badge | the queue's `PendingWatcher` — `channel/boot.rs:182` | in + out | `session_id` + bool | no |
| APNs push | the same `PendingWatcher` — `push/approval.rs:128` | in + out | `PendingEdge` | no |
| Card timeline | `TimelineApprovalGate`, a decorator *around* the gate | in + out | projection (tool, summary, actor) | **yes** |

The seams do not line up:

- The waker fires on the way in and has no counterpart on the way out —
  `channel/boot.rs:172` says so itself. That is *why* the badge had to hang off the queue
  instead: a mark retired by `ApprovalResolved` alone sticks forever on exactly the turns
  nobody answered.
- The `PendingWatcher` sees both edges but not *what was decided* — it carries a
  `PendingEdge`, so it can say "no longer pending", never "approved" or "denied".
- Only the timeline decorator sees the `ApprovalDecision`, including the gate's own
  deny-on-timeout. It is also the only durable record, which makes it the only one that can
  still be wrong an hour later. See finding 1.

The read side is split the same way. `Channel::pending_approvals(session)` is filtered three
separate times, for three different questions:

- `channel/route.rs:680` — the reconnect snapshot's authoritative replacement set.
- `push/mod.rs:731` — re-check before a push goes out, in case it was answered meanwhile.
- `api/admin/projects.rs:1134` — reverse-lookup call_id → session, an O(sessions × queue)
  scan because nothing indexes the queue by call id.

## 1. A card keeps a phantom "waiting on you" prompt

`TimelineApprovalGate::request` (`project/src/approvals.rs`) writes `ApprovalRequested`,
awaits `inner.request(req)`, and writes `ApprovalResolved` only once that returns. Drop the
future instead — outer tool timeout, cancelled turn, `/stop`, process restart — and the
second entry is never written.

The inner `ChannelApprovalGate` covers itself for precisely this case with a `QueueCleanup`
RAII guard (`tools/src/approval.rs:645`, whose comment names "the outer tool timeout fires
while the user is still reading the modal"). The wrapper has no counterpart, and
`project/src/settle.rs` settles the *run*, not the prompt.

The symptom: `pendingApprovals()` (`app/web/src/pages/projects/timelineModel.ts:41`) derives
pending as requested-without-resolved with no liveness check, so the card shows a permanent
prompt. Clicking it 404s — `parked_approval_session` (`api/admin/projects.rs:1128`) cannot
find a queue entry the cleanup guard already dropped.

This is the failure the feature was built to prevent: a card that stops explaining itself at
the prompt, which is where a reader goes looking. It also defeats the claim that pending is
derived rather than stored — **deriving from two persisted rows is a stored flag when the
second row can go missing.**

## 2. The target: one gate, one persisted record

`TimelineApprovalGate` exists because the card needed a durable, issue-addressable record
and the gate had none. Give the gate one and the second gate has no reason to exist.

**One gate.** `ChannelApprovalGate` keeps the queue as the live decision authority — one
oneshot per call, first resolve wins, unchanged — and gains a store alongside it. It writes
the request on raise and the outcome on settle, where "outcome" covers all three exits:
a decision, the deny-on-timeout, and abandonment (the dropped-future path, which the queue
already observes as `PendingEdge::Abandoned` in `drop_call`, `tools/src/approval.rs:539`).

**That deletes finding 1 structurally.** Once the settle write is owned by the queue, the
resolved record no longer depends on some future surviving to its own return statement.

**The card and the chat become renderings.** What actually differs between them, after the
merge, is display and addressing:

- chat is *pushed* and session-addressed (fan-out to the call's session subscribers, plus
  the reconnect snapshot);
- the card is *pulled* and issue-addressed.

Both read one record. The four announcement hooks collapse into one seam publishing both
edges with the full request on raise and the outcome on settle; the waker and the
`PendingWatcher` fold together.

### Where things live

The gate must not learn what an issue is. The approval row is channel-agnostic and carries
`session_id`; issue ↔ session is already known (`IssueRunRow.session_id`, and
`session.trigger.issue()`), so the card resolves its own view without the gate importing
anything from `baybo-project`. `ApprovalStore` belongs in `baybo-store` with its sqlite impl
in `baybo-storage` — `baybo-tools` already depends on both.

Per the crate-boundary rules in `CLAUDE.md`: the gate owns this domain and holds the store;
nobody else gets `Arc<dyn ApprovalStore>`. The card side gets a narrow port — "the prompts
waiting on this issue", "answer this one" — not the store.

### The two timeline variants retire, and that needs a wash

With the card rendering from the approval record, `IssueEventBody::ApprovalRequested` and
`ApprovalResolved` (`store/src/project.rs:318`/`329`) stop being written.

They cannot simply be deleted. `issue_events.body` is JSON and
`crates/storage/src/sqlite/project.rs:72` hard-errors on a body it cannot parse
(`"issue_events.body unreadable"`), and the timeline read pulls the whole card at once — so
one legacy approval row would break the entire timeline, not just its own entry. This is the
same trap as retiring a trace `StepKind` (`docs/modules/trace.md`).

Pick one before shipping: keep the variants deserialisable as write-never legacy, or wash
the rows (`DELETE FROM issue_events WHERE kind IN ('approval_requested',
'approval_resolved')`) as an explicit pre-step. The usual "remove the CREATE and the
consumers, leave old data inert" shortcut does **not** apply — this old data is not inert.

### Boot reconciliation is required, not optional

Persistence buys the record. It never buys the ability to answer: the thing blocked is a
future in memory, and a restart takes it with the process. A persisted `pending` row that
survives a restart is a lie unless something reconciles it — so the merge must include a
boot pass that settles every stored-pending approval with no live queue entry behind it.
Without that, the merge reproduces finding 1 with a bigger blast radius, because now every
channel has the durable row, not just cards.

### Retention

A durable approvals table grows with every approval on every channel, forever, and the
existing surfaces (TUI, telegram, web chat) have never had one. Per
`docs/modules/storage.md` this is a plain-`DELETE` table, not a soft-delete bin. Decide the
policy as part of the design — the useful side effect is that "what did I approve last
week" becomes answerable at all, which is not true today outside the board.

### Open, for the owner

1. **Does the card keep an ordered narrative?** Approvals leaving `issue_events` means the
   card merges two ordered sources at read time (timeline rows + approval rows) instead of
   reading one stream. Alternative: keep writing the two timeline variants as a projection
   from the one gate — one write path, no drift, but the same fact stored twice.
2. **Retention policy**, per above.
3. **Legacy rows**: wash or keep-deserialisable.

### What must survive the refactor

- One decision authority: one queue, one oneshot, first resolve wins. The record observes;
  it never decides.
- The watcher runs **inline on the blocked tool's own future**, without the queue's lock
  held, and must stay non-blocking (`tools/src/approval.rs:258`, `channel/boot.rs:180`).
  Three tests pin it: `a_watcher_may_read_the_queue_without_deadlocking` and
  `a_panicking_watcher_does_not_wedge_publication` in `tools/src/approval.rs`, and
  `a_full_queue_drops_rather_than_blocking` in `push/approval.rs`. **A store write is not
  non-blocking** — the persistence hop cannot go inline on that path.
- Broadcast the entry the gate woke us *for*, never re-read off the queue tail
  (`channel/boot.rs:144`): the queue is per-channel, so two sessions blocking at once
  interleave push/wake, and a tail read announces one twice and the other never — and
  nothing re-announces, so that turn silently denies itself at the timeout.
- Answering a prompt from a card still checks the card first (`api/admin/projects.rs`). The
  queue is keyed by call id alone, so that check is the only thing between a request and
  another board's prompt.

## Related

- `docs/modules/project.md` — the card timeline and what may write to it.
- `docs/modules/channels.md` — the owner channel as the merged web + mobile fan-out domain.
- `docs/modules/storage.md` — plain-`DELETE` vs soft-delete tables.
