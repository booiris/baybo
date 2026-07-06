# Baybo Chat Sync

The vocabulary of chat transcript synchronization between the gateway and its
chat clients (web, iOS, TUI), as settled in
[`docs/sync-protocol.md`](docs/sync-protocol.md) (2026-07-06). Terms below are
the canonical names; the "avoid" aliases are mostly the retired v1 mechanism
names — using them after the migration is a smell.

## Language

**Ordinal**:
The per-session monotonic sequence number a transcript row receives when the
actor persists it; the unit of all sync addressing.
_Avoid_: seq, index, position

**Cursor**:
The highest ordinal a client knows it has covered for one session; `null`
means "no baseline yet", never a sentinel number.
_Avoid_: sync key, since-value, `-1`/`0` sentinels

**Sync**:
The one pull call that returns full-fidelity transcript rows after a cursor
(or the newest page when the cursor is `null`).
_Avoid_: catch-up, replay, hydration (all name retired v1 mechanisms)

**Baseline**:
The newest-page state a client adopts when it has no cursor or after a
rebase; older history stays fetchable.
_Avoid_: initial sync, cold start fetch

**Rebase**:
The server answering sync with a fresh baseline instead of the difference
because the gap exceeds the requested limit.
_Avoid_: reset, truncated

**Backfill**:
Pulling older rows (`before_ordinal`) to extend a thread upward on scroll.
_Avoid_: paging (ambiguous), and the retired iOS "initial backfill"

**Gap nudge**:
The server telling a connection it dropped frames (for one session, or
`None` for unattributable loss); the client's response is always Sync.
_Avoid_: reset

**Echo**:
The server's immediate pre-persist re-broadcast of an inbound user message
to the session's subscribers; ordinal-less by decision, and doubles as the
transport-level send acknowledgment (durability is confirmed separately, by
an ordinal-stamped row).
_Avoid_: ack (alone), user reflect

**Outbox**:
The client-persisted queue of not-yet-confirmed sends, keyed by
`platform_msg_id`.
_Avoid_: pending queue, retry queue

**Safety-net pull**:
The periodic foreground Sync tick that backstops lost Gap nudges and
suspended-client windows.
_Avoid_: polling (it is not the primary transport)

**Durable plane**:
Persisted transcript rows (message / work / notice), ordinal-addressed,
never lossy.

**State plane**:
Idempotent REPLACE snapshots of current session state (turn, work steps,
approvals, tasks) — repaired by the next snapshot, not by history.

**Ephemeral plane**:
Droppable live streams (answer deltas, reasoning, tool lifecycle, activity
pings); their durable *outcome* lives in the other two planes.

**Mirror**:
iOS's on-device cache of rendered rows plus the cursor, written atomically;
a pure cache — never a source of truth.
_Avoid_: local store, offline database

**Channel universe**:
The disjoint session sets owned by the `http` (web) and `device` (iOS)
channels; sessions never cross between them.

## Relationships

- A **Session** owns one ordinal sequence; every durable-plane row has
  exactly one **Ordinal**.
- A client holds one **Cursor** per session; only **Sync** rows and
  ordinal-stamped live frames advance it (max-wins).
- A **Rebase** REPLACEs the thread with a **Baseline**; **Backfill** extends
  it older; the two never interleave in one call.
- An **Echo** confirms *transport* for an **Outbox** entry; an
  ordinal-stamped **Sync**/**Backfill** row confirms *durability* and
  releases it (same `platform_msg_id`).
- A **Gap nudge** and the **Safety-net pull** both terminate in **Sync** —
  there is no other recovery verb.

## Example dialogue

> **Dev:** "After a reconnect, do we replay the missed frames?"
> **Domain expert:** "No — there is no replay. The client Syncs from its
> Cursor; if the gap is too wide the server Rebases it onto a fresh Baseline
> and the older rows come back later via Backfill. Live frames the client
> buffered during the Sync are applied after, deduped by ordinal."
> **Dev:** "And if the send's Echo never arrives?"
> **Domain expert:** "The Outbox entry stays unconfirmed; the next Sync
> either shows the row (confirmed) or the entry retries."

## Flagged ambiguities

- "**catch-up**" historically named both the WS Subscribe replay and the
  REST endpoint — both retire; the concept is just **Sync**.
- "**backfill**" historically meant iOS's `ensureBackfilled` initial
  hydration — that mechanism is deleted; the word now means backward paging
  only.
- "**reset**" conflated three things (over-cap replay, slow-consumer drop,
  client rebuild) — split into **Rebase** (server answer) and **Gap nudge**
  (server notice); the client-side verb is always **Sync**.
