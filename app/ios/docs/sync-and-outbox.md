# Transcript sync, mirror retention, and the send outbox

*How the iOS chat transcript loads and recovers rows (sync-protocol v2), why the on-disk transcript mirror is never swept, and how sends survive a flaky leg — governs `app/ios/App/Core/ChatStore.swift`, `app/ios/App/Core/OutboxStore.swift`, the transcript mirror store, and `app/ios/web/src/Transcript.tsx` + `app/ios/web/src/bridge.ts`.*

## Send path

Native mints the msgId, seeds the webview's optimistic bubble + echo-dedup FIRST,
enrols the persisted outbox entry, then enqueues on the leg (see "Send outbox"
below).

## Transcript sync (sync-protocol v2)

> **Read `docs/sync-protocol.md` BEFORE touching sync or lifecycle.** This
> section is a client-side companion to that protocol doc, not a replacement for
> it.

The old seven-cell hydration matrix is **retired**. Transcript loading and
forward recovery are now **one loop** (`docs/sync-protocol.md`), identical on
both legs (same web bundle, same `GatewayJsonClient` API surface):

```
on open / reconnect / gap / buffer-overflow re-attach / safety tick:
  page = sync(since = syncSince(cursor, rows))   # null → newest-page baseline
  if REPAIR (since == null because a rendered row outran a non-null cursor):
      REPLACE the whole thread with page.rows    # order in doubt; newest edge
  elif page.rebased or since == null:
      REPLACE the page's window, KEEP the rows above its floor + the reader's place
  else:
      APPEND/merge page.rows           # dedup by row id / platform_msg_id
  cursor = max(cursor, page.next_cursor)   # frozen while rebase-dirty
```

Every REPLACE branch keeps the open work block + the optimistic sends
(`applySyncReplace`).

### Only a REPAIR may throw the loaded history away

A REPLACE used to mean one thing: the page IS the thread now, drop everything
else and snap to the newest edge. Three different situations reach it, and only
one of them actually puts the rendered rows in doubt.

- **Repair** — `syncSince` refused a NON-null cursor because a rendered row
  outran it. The thread may be out of ORDER (the scramble that gate was written
  against), so the rows it drops are exactly the ones under suspicion. This one
  keeps the old behaviour.
- **Rebase** — `rebased` says only that the difference outran the server's
  `limit` or its scan bound and here is the newest page instead
  (`docs/sync-protocol.md` "Gap > limit → rebase"). It does **not** mean ordinals
  were rewritten.
- **Cursor-less baseline** — a fresh install (no older rows to keep anyway), or
  the deliberate `restoredUntimedWork` demotion, which asks the gateway to
  re-time ONE block and says nothing about the rest.

For the last two, `rowsAboveFloor(prev, page.oldest_ordinal, …)` keeps the rows
above the page's floor, along with `oldestOrdinal`/`hasMoreOlder`/the withheld
head, which still describe them. The cut is by POSITION, not a filter on
`ordinal < floor`: a notice or a `/stop` mark carries no ordinal, and a filter
would delete every one of them out of the half that survives.

The client cannot read the reason off the reply — the frame carries only
`since_ordinal: null` — so `runSync` records it (`baselineIsRepair`) when it
posts.

Both non-repair cases are reached by ordinary use, not by pathology. The rebase
test counts *emitted* rows against `limit`, but the server scans past the
invisible ones only to `SYNC_SCAN_BOUND_MULTIPLIER × limit`, and one agentic turn
persists hundreds of invisible tool rows — a handful of turns since the cursor is
enough. `restoredUntimedWork` fires whenever the mirror holds a closed work block
with no duration, which backing out of a chat mid-turn mints on its own
(`keepAnchor` strips the anchor on restore, then `closeWork` has nothing to
compute from). Before this rule, either one landed as: open a long chat, scroll
up, watch the pages you just fetched vanish and the thread slam to the bottom.

The scroll half is the same rule stated once more. A reader **at** the newest
edge is snapped back to it — that is where they were. A reader parked in history
is not: a REPLACE reuses every surviving row's key, so `captureRowAnchor` takes
the row under the top of the viewport before the swap and a layout effect puts it
back under the same pixel afterwards. Only when the rebuild dropped that row —
the repair's whole premise — do they land at the bottom. Every row carries
`data-row-id` for this (the message index's jump reads the same handle).
`transcriptScroll.test.tsx` mounts `<Transcript>` under a fake layout to pin it.

**`hasUntimedWork` only asks the tail, and that window is load-bearing.** The
demotion buys a re-timing only for a block the baseline page re-delivers, and
that page is the newest `SYNC_BASELINE_LIMIT` rows. Older blocks used to be
healed by deletion — the REPLACE dropped everything the page didn't carry — but
now they SURVIVE, so an out-of-reach one would match again on every open and
demote the cursor forever. Within the window it stays self-limiting, and its
worst case (a turn the gateway cannot time either — a cancelled or crashed turn
has no `work_ended_at`) now costs one round trip per open and nothing on screen.

### The webview drives sync

`app/ios/web/src/Transcript.tsx`'s `runSync()` posts
`{type:"sync", sinceOrdinal, limit}` over the bridge; native
(`ChatStore.requestSync`) fetches `GET …/sync` over the active leg and pushes a
synthesized `sync_page` frame back.

Clock edges — there are five:

- mount (resident re-entry),
- the `connEpoch` bump (`handleConnEpoch` — reconnect),
- a `gap` frame,
- the offscreen-buffer-overflow re-attach (native `bridge.requestSync()`),
- the 3-minute safety tick.

`syncInFlight` coalesces a burst to one pull.

### A difference must EXTEND the thread, never overlap it

`syncSince` (`Transcript.tsx`) is the gate, and it exists because of a shipped
bug. The cursor is a **coverage watermark**, not a high-water mark of what is
rendered: scroll-up paging (`prependOlder`) and the rebase-dirty freeze both put
rows on screen without advancing it. Ask for `since = cursor` from such a thread
and the server answers correctly — a *difference*, so no `rebased` REPLACE ever
fires — while the merge, which appends any row it doesn't already hold, welds
that whole span onto the BOTTOM of the thread and fuses the page's leading work
block into the tail's card. One device rendered three hours of conversation 75
rows too high, under a single "Worked 2h 47m" block; nothing was lost, and
nothing could self-heal (a re-sync returns nothing, and `sanitizeRestoredRows`
does not sort).

So: if any rendered row carries a durable ordinal **above** the cursor, present
`null` and take the baseline REPLACE — the path a fresh install proves correct.
"Durable ordinal" is `rowCoverageOrdinal`, not the id alone: a user row is KEYED
by its `platform_msg_id` (so an optimistic bubble reconciles), so it carries the
server `ordinal` as a field beside the id. Reading only the id made the gate
blind to every user message ever sent — from this device or another — which is
precisely the rebase-dirty case it exists for. `app/web` never had the hole: it
keys every row `row-<sid>-m<ordinal>` and keeps `clientMsgId` separately.
**The gate is posted-time only, so the merge also places.** `syncSince` runs
when the request goes out, and the thread keeps growing during the round trip —
a difference asked for at `since` can land on a thread it is no longer a prefix
of, and the append then files its rows under rows they predate. So
`mergeSyncPage` puts a row it does not already hold at its ordinal rather than
at the end. It does that **only when a durable row still sits below the
insertion point**, which is the proof the row really did land late; with nothing
ordinal-bearing below, it appends exactly as before. That restraint is the whole
safety of it, and it took three attempts: a thread's trailing run is
ordinal-less almost always — the live work block is `uid()`-keyed, a notice is
`n<seq>` (a sequence number, not an ordinal), and every user row rendered live
carries no ordinal at all, since the echo brings none. Ordering a durable row
against those is a guess, and guessing wrong files a turn's own answer above its
own question: the reply renders over the card that produced it, the tail-only
`closeTrailingWork` never runs, and the block spins "Working…" while the next
turn's steps weld into it. A differential test against the pre-placement
behaviour is the way to check a change here — the two must agree on every
ambiguous shape and differ only where a durable row proves lateness.

`applySyncPage` re-checks the invariant at apply time as a backstop, and refuses
a page **only** when it is both stale and carries a row placement cannot file
(an `n<seq>` notice, or a slash-command echo, which `control_event_item` emits
with no ordinal). Both conjuncts matter: on the overrun alone it would discard
pages that merge perfectly — one live reply landing mid-round-trip is enough —
and each discard costs a round trip plus a REPLACE. It re-runs the
sync without demoting the cursor; `runSync` re-derives `syncSince` from the
current thread, so it posts a fresh difference when the live rows already
advanced the cursor and falls through to a baseline only when the thread is
genuinely uncovered. That is also what terminates it.

**`app/web` has NOT moved.** Its merge still plain-appends and its `syncSince`
says "Mirrors iOS"; only the gate is shared. Port this before relying on the
ordering there.

**This prevents the scramble; it does not repair one.** The welding sync ends by
advancing the cursor to `next_cursor`, and a difference is only returned when its
scan did not overrun, so that watermark is the session's newest ordinal — above
every rendered row. A mirror already scarred is persisted covering itself, so the
gate stays quiet and the order stands; the exit is the per-session resync. It is quiet
on a healthy thread (a sync's `next_cursor` covers every row it delivered, a
live final reply advances the cursor with itself, and paged rows are older than
everything) and it is self-terminating (a REPLACE leaves the thread a prefix of
its own watermark). Ordinal-less rows — an optimistic send, a live work block —
are not durable coverage and never trip it; a send gains its ordinal from its
durable twin (`mergeSyncPage`, or a REPLACE page carrying it) — **never from the
echo**, which the gateway fans out before the router persists the message and
which therefore carries `ordinal: None` by design (`channel/adapter.rs`).
`markSent` stamps whatever the echo brought, so for our own send it clears the
send chrome and nothing else.

That distinction is load-bearing, and reading it the other way cost a shipped
bug. The REPLACE-overlay (`applySyncReplace`) keeps an optimistic send iff its
id is in **`unconfirmedSends`** — the set `handleUserSent` mints into and only a
sync page carrying the `platform_msg_id` retires from, the same proof native's
`reconcileOutboxAfterSync` releases its own outbox entry on, off the same frame.

Two cheaper-looking predicates are both wrong, in opposite directions, and the
tests in `rows.test.ts` pin each:

- **Send chrome** (`sendState !== undefined`) deletes the row the moment the echo
  lands. A first send whose own `connEpoch` sync raced its persistence vanished,
  could never be re-fetched (the cursor leapfrogged it on the answer's ordinal,
  and a difference selects strictly `>`), and came back only as the outbox's
  mount-edge replay — appended below the answer.
- **A missing ordinal** (`ordinal === undefined`) looks like the fix for that,
  and is worse. It is not a property of unconfirmed sends: since the echo never
  stamps an ordinal and the cursor outruns the durable twin, it is the *steady
  state* of every user row this client rendered live. Keying on it makes those
  rows immortal, so the first REPLACE whose window is narrower than the thread
  tears every settled question out of place and welds it below the newest answer
  — the `syncSince` scramble class, re-entered through the front door.

The lesson generalises: **on this side of the bridge nothing about a rendered row
proves durability.** Only the outbox knows, so it has to say so.

That makes `userSent` a two-way seam. Native mints an id into the web's set on
every mount edge (`replayUnconfirmedSends`), and `sendConfirmed` is the return
leg, fired from `ChatStore.releaseDurable` — the single place an entry is
retired, kept single so a future release path cannot take the outbox half and
forget the transcript half. The web cannot derive the release: native's dominant
proof is a per-key point lookup (`chatLookupMessage`, on every reconnect and
after every rebase) that touches no frame at all, and even the sync-page proof
is unreachable for an ordinary send, whose ordinal the turn's own reply has
already carried the cursor past. Without the return leg the set degenerates from
"sends still owed" into "sends made since mount", and the first REPLACE with a
narrower window welds them below the newest answer — the same corruption, one
door along. `applySyncPage` also retires on a page carrying the
`platform_msg_id`, which is belt to the bridge call's braces.

### Only the server says two work blocks are one turn

Two adjacent `work` rows used to be folded into one card on ADJACENCY alone
("a healthy turn has a message row between its block and the next"). The
scramble above disproved the premise — three turns' blocks ended up side by side
and welded into a single "Worked 2h 47m" card — and it is not even bug-specific:
a turn whose empty final reply leaves no bubble abuts the next fire the same
way.

So the fold asks the server instead. Every reconstructed `work` row carries
`turn_complete` (`ChatTranscriptItem`, already on the wire — the FFI passes sync
/ history rows through verbatim): `false` = the page window's trailing edge cut
this block off and its turn continues into the adjacent page, `true` = a real
boundary (the final answer, the next user turn, a `/stop`, a compaction
watermark) closed it in-window. `sameContinuingTurn` (`Transcript.tsx`, mirroring
`app/web`'s) lets a block be JOINED onto the previous one only when that previous
one is a cut-off (`false`) head — every seam asks it: `foldAdjacentWork` (the
prepend and REPLACE folds), `mergeSyncPage`'s tail fold, and the REPLACE
overlay's live-block fuse. A live block is keyed by `uid()`, carries no flag, and
still fuses with its own reconstruction — that is RECONCILE (one span, two
representations), not a join. An ABSENT flag declines: an extra card is
cosmetic, a wrong join swallows a whole turn into another's card.

The mirror's restore heal (`sanitizeRestoredRows`, which folds a legacy
[work][work] tear) skips a head the server called complete — otherwise the next
cold open would weld back exactly what the guard kept apart.

`crossesCompaction` STAYS alongside it. It is subsumed only for a watermark the
server saw: a split inside one reconstruction window flushes the pre-compaction
half `turn_complete: true`, which `sameContinuingTurn` refuses on its own. A
watermark falling in the GAP between two pages is straddled by no single window,
so neither page splits its own half and the head is an ordinary cut-off block —
there the compaction guard is the only refusal. `mergeSyncPage` carries both
guards for the same reason `foldAdjacentWork` does.

### The server replays NOTHING on Subscribe

It answers with one `SubscribeState` bundle (turn/work state) and live frames.
`Subscribe` lost its `since_ordinal` field; `Frame::Reset` / `WorkSnapshot` /
`PendingApprovalsSnapshot` are gone.

### The mirror stays a pure `{rows, cursor}` cache

Written atomically. It is never a source of truth — a mirror-less open just syncs
a baseline. The cursor is `number | null` (`null` = no baseline, never a
sentinel); it lives in the persisted mirror blob (`lastOrdinal`), advanced from
the sync coverage watermark and live final-reply ordinals, frozen while
**rebase-dirty**.

### Draft vs listed

A compose draft stays empty until its first send: `ChatStore.requestSync` skips
the fetch until the session exists remotely (`listed || remoteSessionEnsured`).
The webview no longer needs a `listed` flag — its loop runs the same on every
open.

### Backward paging (scroll-up)

Unchanged in role: `fetchHistory(before)` → `history_page` frame, full-fidelity
rows (message/work/notice) keyed by their stable server `id`, no client-side
filtering.

### Re-verify after touching any of this

- (a) logout → re-pair the same gateway → open an old session;
- (b) open an old (unpinned, outside top-10) session → back to list → re-enter;
- (c) kill the app → relaunch → open a session;
- (d) open A mid-stream → back → open B → back → A shows A (not B), B's mirror is
  not overwritten by A's late flush.

### The single reused webview changes WHEN, never HOW

**The single reused webview (`TranscriptHost`) changes WHEN the transcript is
(re)mounted, never the sync loop.** Returning to the SAME session reuses the LIVE
React tree; opening a DIFFERENT session remounts `<Transcript key={sessionId}>`
from that session's `restoredState`, then its mount effect runs one sync. A
jetsam silently reloads → `ready` re-fires → re-mounts and re-syncs.
Cross-session safety rests on `app/ios/web/src/bridge.ts` clearing
`buffer`/`blobPending`/`pendingPersist` on `init` and `persist` writes being
session-tagged.

## Transcript mirror retention (do NOT re-add a sweeper)

**The mirror is the entire cold-open story.** `ChatScreen.onAppear` calls
`retarget` BEFORE `connectIfNeeded`, `deliverInit` reads `transcripts/<id>.json`
straight off disk, and `<Transcript>` seeds its rows in the `useState`
initializer — so a cached conversation paints on the first commit with **zero
network**. Nothing in the paint path reads `connState`. If a chat opens blank and
fills in only once the leg connects, the mirror was **missing**, not gated.

### The first commit paints the mirror's TAIL, not all of it

`splitForFirstPaint` (`Transcript.tsx`) holds everything older than
`FIRST_PAINT_ROWS` back; a nested `requestAnimationFrame` folds it in on the
frame after the paint, through `prependOlder` — the scroll-up seam, so the
viewport is anchored, `sentIds`/`renderedOrdinals` are re-seeded, and the
work-block fold runs. The mirror only grows (every turn and every scroll-up page
a session ever rendered is in it), and seeding `messages` with all of it made the
first paint wait on the markdown parse, DOM build and WebKit layout of the WHOLE
conversation — which is why a long chat opened to a longer white screen than a
short one, off the same disk. Nothing about the sync loop changes: withholding
rows only removes ordinals BELOW the ones `syncSince` scans for, so the gate
answers identically on the split thread (pinned in `rows.test.ts`).

Two ways the withheld head can be lost, both closed:

- the persist effect **must not write while it is withheld** — that persists the
  truncated thread over the only copy of rows the cursor has long since passed,
  and `flushPersist` is synchronous on both `pagehide` and native's detach, so a
  back-out inside that frame reaches it;
- a **baseline REPLACE drops the reservoir unrendered** (`applySyncPage`) —
  those rows describe a thread the repair has just rebuilt away, and the page
  brings its own paging window. A *rebase* that keeps the rows above its floor
  keeps the reservoir too: it is older than those, so it still describes the
  thread on screen.

While the head is withheld, `oldestOrdinal`/`hasMoreOlder` describe what is
RENDERED (the tail's own floor, and "there is more older"); the drain hands the
mirror's own values back. `loadOlder` drains the reservoir instead of hitting the
network — the safety net for a frame callback that never ran (rAF is throttled
while the webview is hidden), which would otherwise re-fetch what is on disk and
fail outright offline.

So: **a mirror is kept for every session this device has rendered, and no
capacity sweeper evicts it.** `save()` writes the registry and nothing else.

### The three deletion paths

A mirror is removed only when its conversation genuinely goes away, and there are
exactly three such paths — all of them a user's deletion, none of them a sweep:

- `beginHide` — the user deleted the conversation here. Deletes the file BEFORE
  the row guard: a racing merge may already have dropped the row, and the delete
  must still land.
- `merge` — the row is absent from the server's list, i.e. the user deleted it on
  another client. That rebuild is where it leaves the list for good, so its
  transcript leaves with it (a targeted set difference against the surviving ids,
  skipping `pendingMutations`, which `beginHide`/`rollBackHide` own — **not** a
  directory sweep).
- `removeAll` — logout/rebind: the rows belong to the gateway we just left.

The one delete that is **not** a conversation going away is
`dropTranscriptMirror` — the per-session resync escape hatch
([transcript.md](transcript.md#per-session-resync-the-escape-hatch)). The row,
the outbox and the pending approvals all stay; only the local rendering is
discarded, so the cold-open path can rebuild it from the gateway. It is still
user-triggered and still per-session: nothing here sweeps.

### Never mint the orphan in the first place

**An empty thread with no cursor never writes a mirror at all** (the persist
effect in `app/ios/web/src/Transcript.tsx` bails). The transcript mounts for every
compose draft — including the throwaway one prewarmed at launch — each under a
fresh uuid that never becomes a chat-list row, so persisting them minted a file
per abandoned draft that nothing could ever reach again (no row → no delete → and
nothing sweeps). Don't create the orphan rather than sweep for it later.

### The `TranscriptStore.prune` post-mortem

This is a scar, so don't re-cut it. `save()` used to end with
`TranscriptStore.prune(keeping: <top 10 by lastActive>)` and every ingredient of
that was hostile:

- `save()` runs on essentially everything (every list merge — *including the one
  the pop back from a chat fires* — every activity ping, every badge clear);
- `rows` after a merge is the gateway's WHOLE session list ranked by the
  **server's** `lastActive`, so rows this device has never opened (and every
  individual cron fire — the list's cron-group collapse is render-time only)
  spent keep-slots on mirrors that did not exist;
- and reading a chat earns nothing (`touch` deliberately does not bump
  `lastActive` — "ordering means message activity, not visits" — and a merge
  would overwrite it anyway).

Net effect: any conversation outside the ten most recently **messaged** ones had
the mirror it wrote on exit deleted seconds later, and opened blank on **every**
re-entry, forever. An active `*/30` cron job burned all ten slots in about five
hours. `app/ios/Tests/SessionIndexMirrorTests.swift` pins this; the retention
tests fail against the old prune.

### The set on disk only grows, deliberately

Mirrors are text (`{rows, cursor, imageDims}` — never blob bytes) and exist only
for sessions actually read here, each dying with its conversation, so the set on
disk tracks the conversations the user still has. Like the blob cache it only
grows, deliberately: bounding it further wants a stated retention policy, not a
surprise sweep.

### What no cache can fix

A session this device has never rendered (started on web/TUI, a cron fire, a push
tap into a new session, the first open after a re-pair) has no mirror by
construction — its first rows MUST come off the wire. That open shows the
`.thread-loading` line (empty thread + sync in flight), whose 400ms CSS delay
keeps it invisible on every open that resolves instantly — a restored thread, and
a compose draft, whose synthesized empty page lands well inside the delay.

## Send outbox (sync-v2)

The one-shot "red dot, human retries" send is replaced by a **persisted outbox**
(`app/ios/App/Core/OutboxStore.swift`, a JSON file per session under
`Application Support/baybo/outbox/`, wiped with the mirrors on logout).

Entries are keyed by `platform_msg_id` with a two-stage confirmation:

- the server's Echo (ordinal-less user message, same key) proves transport
  (`sending` → `sent`, observed in `ChatStore.outboxObserveFrame`);
- an ordinal-stamped row with the same key (from a `sync_page`, scanned in
  `reconcileOutboxAfterSync` before the frame reaches the webview) proves
  durability and releases the entry.

No echo within 10 s → one blind resend, capped at 3 transmissions, then `failed` +
the manual red-dot retry (`resetForManualRetry`).

### The re-seed runs on every mount

An entry's optimistic BUBBLE lives in the webview's thread, and the gateway has
no row to bring a queued send back from — so whenever the thread a page mounts
with is older than the outbox, the user's message silently vanishes and a
`failed` one loses the red dot that is its only retry affordance (the retry
payload lives in the web row). `ChatStore.replayUnconfirmedSends` re-seeds the
surviving entries, oldest first, `sendFailed` for the failed ones.

It runs on **every** mount edge — the `ready` of any page load, and every
`retarget` attach — never behind a "a resync just happened" latch. The
[per-session resync](transcript.md#per-session-resync-the-escape-hatch) is only
the loudest way to lose the bubbles; a jetsam between a send and the mirror's
debounced write, a session whose mirror was never written, and a rebased page
that dropped the row all do it too, and a latch reachable only from the hatch
left every one of those permanent. (The resync itself does not touch the outbox
file, and seeds nothing of its own: reached from the list, the session it
rebuilds usually has no page yet.)

Running it always is safe because the re-seed is **idempotent, and the WEBVIEW is
what makes it so**. A user row is keyed by its `platform_msg_id`, so
`handleUserSent` asks `holdsUserSend` and returns before appending — before
`awaitingReply` too, which would otherwise raise the composer's stop button on a
send that failed days ago. A live send mints a fresh uuid and can never take that
exit. Native cannot make the call itself: it does not know whether the tree it is
seeding came up from a mirror that already carries the bubbles.

The one native-side rule the idempotency needs: `retarget` skips the re-seed
while the page is not `ready`. Those calls would only queue in the bridge's
`pending`, and the `ready` that flushes them re-seeds again — the same work
twice, arriving before React has committed the first pass and can recognise it.

On the reconnect edge the sync runs first (the reconciliation gate), then
unconfirmed entries resend. A **rebased** sync hides the floor, so each
unconfirmed entry goes `unknown` and resolves via the per-key point lookup
(`chatLookupMessage`) — found → released, absent → retry resumes.
