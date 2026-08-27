# Subagents

*The chat header's `Subagents` entry, the list sheet it opens, and the read-only
transcript behind each row: `app/ios/App/Screens/SubagentSheet.swift`,
`SubagentScreen.swift`, `App/Core/SubagentReadStore.swift` (+ `SubagentList.swift`
for the row logic, `TranscriptTarget.swift` for the seam it plugs into), plus the
three read-only gateway routes in `crates/gateway/src/api/admin/chat.rs`.*

A `spawn_subagent` call mints a **real session** — its own row, its own turns,
its own `session_messages`. Until this feature there was no way to look at one
from any client: the parent transcript showed a bare `spawn_subagent` step and
nothing else.

## A child session is invisible three separate ways

Children are minted on `ChannelType::from(SUBAGENT_CHANNEL_TAG)` = `"subagent"`
(`crates/agent/src/runtime/subagent_spawner.rs`), and every existing chat surface
is scoped to `owner`:

- REST — `load_scoped_chat_session` 404s a non-`owner` channel, and returns the
  **same** body as a nonexistent id so existence can't leak.
- WS — `Frame::Subscribe` answers a cross-channel subscribe with a rejection
  Notice (`crates/gateway/src/channel/route.rs`).
- The chat list — `list_by_channel(owner)` never returns them.

**All three stay exactly as they are.** This feature adds a fourth, narrower
door instead of widening any of them: three **GET-only** routes that do their own
lineage-scoped admission.

```
GET /v1/chat/sessions/{parent_id}/subagents   → direct children, ascending
GET /v1/chat/subagents/{child_id}             → ChatSessionDetail
GET /v1/chat/subagents/{child_id}/sync        → ChatSyncResponse
```

No POST, no WS, no outbox. A paired device reaches them over both legs with no
extra wiring — the admin middleware blanket-authenticates the listener and the
relay tunnel gate is a bare `/v1/` prefix check — **which also means a scope bug
ships to the device the moment it merges.** Test the predicate, not the happy
path.

## The readability predicate

```rust
// walk up, capped; the root must be a session the client could already open
let mut s = child;
for _ in 0..MAX_LINEAGE_WALK_HOPS {
    let Some(l) = s.lineage.as_ref() else { break };
    s = load(&l.parent_session_id)?;
}
s.channel == ChannelType::owner() && !is_private_cron_session(&s)
```

Two things this deliberately does **not** do:

- **It does not use `root_session_id`.** That column is denormalized, write-only,
  and read by no query anywhere — `idx_sessions_root` was dropped in the 2026-07
  audit precisely because "it indexed a column no query filters on". Promoting a
  field nothing validates into a permission decision means one bad write is one
  silent authorization bug. The lineage walk uses `idx_sessions_parent`, is
  bounded by the same hop cap `spawn_subagent`'s own depth check uses, and is at
  most three hops in practice.
- **It does not stop at `channel == owner`.** A cron fire session is minted on
  the cron job's own channel, so a job scheduled from an owner conversation
  produces an owner-channel fire session — and a **one-shot** fire is a private
  workspace the chat list drops (`is_private_cron_session`) and the attach path
  404s unconditionally. Without the second clause, a subagent spawned inside one
  becomes a side door into a conversation no client is supposed to reach. The
  rule is "the root must be a session you could already open", not "the root must
  be on the owner channel".

## The transcript opens on the errand — by provenance, never by guessing

A NEW spawn's errand is persisted as `ChatMessage::subagent_seed`
(`Role::User`, `MessageSource::SubagentSeed` — both backends: the baybo path's
`append_spawned_prompt` and the external-agent path). The reconstruction's
user-bubble arm (`renders_as_user_bubble`: `from_user()` OR `SubagentSeed`)
renders exactly that row, so the child's page opens with the instruction it
was given, the way a user turn opens a root session.

What it must NEVER do is render the errand by inference. An earlier version
rendered every `agent_context` row as a user bubble on the subagent path, on
the reasoning that a child has no other user rows so anything with that shape
had to be the errand. **That is false on real data.** The live store's
children carry three different things under that one identity:

- the spawn seed (every spawn from before `SubagentSeed` existed);
- **skill reminders** — `source = 'skill_listing'` only since 2026-08-04; every
  row before that is plain `agent`, so the column cannot separate them
  historically;
- **compaction summaries** (`CONTINUATION_INTRO`), and they are NOT all hidden:
  measured on the live store, 14 of them carry `compaction_inserted = 0` and 7
  carry `1`, so the display read's filter does not remove them.

So the reader got the skills `<system-reminder>` as the first "user" bubble.
Nothing in a row separates the errand from the machinery by shape, and the
schema's own comment says why that must not be guessed at from content:
`source` exists precisely so the genuine prompt is told apart "without
guessing by content". `SubagentSeed` IS that signal — added 2026-08-16, so it
applies to spawns from then on. A historical child (its seed indistinguishable
from the machinery) still opens at the first work block, with the errand
reachable as the row that got the reader here and the page's header.


## Where the list's data comes from

- **Task text** — `resolve_child_session` now stamps `child.title =
  Some(task_summary)` at spawn. A resume does **not** overwrite it. Before this
  the string existed only inside the spawn request and the trace span label;
  nothing put it on the session row.
- **Status / duration** — from the child's **turn rows**, never from the session.
  `append_session_message` writes only `session_messages`; nothing touches the
  child's `sessions` row after creation, so a child's `last_active` sits on
  `created_at` forever and `last_active - created_at` is ~0ms for every child
  ever spawned.
- **Ordering and paging** — ascending, like the transcript, so the newest (and
  usually running) child is last. The listing is a real keyset PAGE of 50, not a
  truncation: the fan-out limiter bounds concurrent breadth, not the cumulative
  count, and an overnight conversation leaves hundreds of children behind. The
  cursor is `(created_at, session_id)` and the id half is load-bearing — one
  turn's fan-out mints siblings inside the same microsecond, which a timestamp
  alone cannot separate. `idx_sessions_parent` is a partial index on exactly the
  listing's predicate, so a page is an index seek.

  The sheet's three-second refresh MERGES the newest page into what is on
  screen rather than replacing it; a reader who has paged back must not watch
  their older rows vanish under them, and a child spawned while the sheet is
  open should appear without disturbing anything above it.

## iOS

### The header

Trailing edge is `[offline alarm][subagents][message index]`. Both persistent
circles are declared **after** the `if store.legDown` branch in the `HStack`, so
the transient alarm always inserts to their left — the existing law
(`ChatScreen.swift`), which exists because a persistent control that slides 54pt
on every network flap is a bug.

`Subagents` shows only when this conversation has any, from two sources OR'd:
the web side posts a flag when it renders a `spawn_subagent` work step (zero
network, correct offline), and entering a conversation fires one bounded list
request to cover spawns that scrolled out of the loaded window. The icon is
static — **no pulse, no badge, no count**. Liveness lives inside the sheet;
an ink capsule with a number already means "unread" one screen up.

### The message index is now permanent

The 3-entry threshold (`OUTLINE_MIN_ENTRIES`) and `OutlinePost.available` are
**gone**, on both sides of the bridge. What survives is `outlineFailed`: if the
outline payload fails to decode, the button stays put and the sheet's empty state
reads "index unavailable" instead of "no messages from you yet". That preserves
the intent of the gate it replaces — *never tell someone with a full thread that
they have never sent a message* — without making a persistent control disappear.

`.accessibilityElement(children: .contain)` on the bar **stays**. It was the
one-line fix for three UI tests that died for weeks when an offline session left
the back chevron as the bar's only focusable child and SwiftUI collapsed the bar
into it (tap targets land on the element's centre, so the tests aimed at empty
header). A permanent index button removes today's trigger — it does not remove
the failure mode, and a future third conditional child re-arms it.

### The container is a sheet, and this is not a style choice

The child browser is a `.sheet` with a `.large` detent holding its own
`NavigationStack` (recursive drill-down pushes there).

**It is not a `fullScreenCover`.** A cover fires the parent `ChatScreen`'s
`onDisappear` — that is written down twice in the codebase already
(`AppStore.chatPath`'s didSet, `docs/attachments.md`) — which runs
`detachCurrent` and leaves the parent transcript unhooked for the cover's whole
lifetime, frames piling into the offscreen buffer. Reading a subagent that ran
for half an hour would overflow that buffer and force the parent into a full
re-sync on return, i.e. straight into the rebase path. A sheet does not fire it.

### A second webview, and a store that is read-only by type

The child renders in its **own** `WKWebView`, built lazily and torn down on
dismiss, so the singleton `TranscriptHost` keeps serving the parent untouched.
Precedent already ships: `DeckHost` is a permanent second webview, and
`ImageViewer`'s `SvgImageWebView` is a third one living inside a cover over the
live transcript. The "one transcript webview" rule is a **latency** decision
(cold-booting a 423KB bundle per chat push), not a memory one.

`TranscriptBridge` talks to a narrow `TranscriptTarget` protocol —
`sessionId`, `requestSync`, `fetchHistory`, `requestBlob`, `queryFileState`, the
three audio calls. `ChatStore` implements it; so does the new
`SubagentReadStore`, which is ~200 lines and has no send path at all.

**Read-only is a type guarantee here, not a flag**, because a `readOnly: Bool` on
`ChatStore` would have to switch off a list of silent killers:

- `requestSync` is gated on `listed || remoteSessionEnsured`. A child session is
  neither — it is remotely real, locally unlisted, never writable, a third state
  the class does not have. Ungated, the miss does not error: it **synthesizes an
  empty `sync_page` and pushes it as a REPLACE**. A permanently blank transcript
  that looks perfectly healthy.
- `disconnect()` tears down the **whole binding's** chat leg, not one session.
  The obvious-looking teardown call would kill the user's live conversations.
- Merely *reading* `store.staging` constructs a `ComposerStaging`, which restores
  the on-disk draft and can resume uploads.
- `bridge != nil` is the class's "still on screen" token in three places; a
  second bridge makes it mean something else.

### No mirror, and why that is the endorsed direction

Enforced through `TranscriptTarget.mirrored` (`false` here; the bridge gates
BOTH halves on it). The `persist` post is declined — the web side writes the
mirror under the session id the *page* reports, so a child would happily create
`transcripts/<child id>.json`, and nothing would ever delete it: every mirror
deleter iterates existing chat-list rows, and a child session never has one.
The transcript-mirror sweeper is explicitly forbidden in
`docs/sync-and-outbox.md`; not creating the orphan is the only correct move.
And `deliverInit` neither restores a mirror for an unmirrored target nor
tolerates one: it DELETES any `transcripts/<child id>.json` it finds. That is
not just orphan hygiene — a child page viewed against an old gateway persisted
renderings the fixed read path no longer serves (the seed and skill-reminder
bubbles), and a restored copy could never heal: the cursor covers the thread,
so every later sync is an empty difference that removes nothing. The delete is
what let installs poisoned that way self-heal on the next open. This section
claimed the persist decline before the enforcement existed; the
`TranscriptTarget` refactor had silently lost it.

### Liveness: polling, and what you will not see

While the sheet is open, the list refreshes every 3s; while a child screen is
open and that child is not terminal, its `sync` runs on the same cadence, which
is cheap because it is a cursor difference. Both stop on background and on
terminal.

**A running child appears in chunks, not as a stream.** `build_history_page`
folds in-flight work steps by looking up `channel_registry.get(&session.channel)`
— and the `subagent` channel is not installed, so that returns `None` and the
page carries persisted rows only. New content appears when a turn's rows land,
not as the tool executes.

Those persisted work rows are necessarily reconstructed as closed. If the
child has no final assistant output yet (or ends without one), its trailing work
sequence defaults open instead of shrinking to `Worked`; a compaction may split
that sequence into adjacent rows, so both halves open around the `Compacted`
divider. Once an assistant output is present, the work keeps the normal closed
default.

Registering a real `subagent` channel to fix this was considered and **rejected**.
It is not one change but three: install a headless channel (it can safely omit an
approval gate — `install` only inserts one when `channel.approval_gate()` is
`Some`), *and* tee the child's `AgentOutput` stream into it, because the child
actor's output goes to a private mpsc owned by the spawner and reaches
`Router::handle_agent_output` never — today it is dropped with a `debug!`. And
even then it would cover in-process children only: external backends have no
actor and emit none of those events. Relaxing the WS subscribe scope was rejected
for the same reason plus the blast radius on unread/list-stale broadcasts.

### Attachments

Fully supported — images, files, video, audio — through `TranscriptMedia`, the
same engine the live chat uses. That class was extracted from `ChatStore` for
this feature and is the one piece deliberately SHARED rather than reimplemented:
every part of it is load-bearing in both surfaces (the digest-keyed preview
directory, the in-flight materialisation dedup, the poster cache, and above all
the detach-window buffers — a download whose terminal `ready` lands while
nothing is attached used to wedge its card at `loading` forever). It publishes
nothing; each store keeps its own `@Published` presentation slots and receives
results through `on…` hooks, which is what keeps `ChatScreen` unchanged and
avoids a nested `ObservableObject` that would republish nothing.

Blob reads need no session scope: the blob id **is** the capability
(`sha256:<digest>.<read_token>`), and the download handler performs no
authorization of its own.

Closing the read-only page calls `AudioPlayerCenter.stop()`. The engine is a
process-wide singleton holding **one** weak bridge, last writer wins, so audio
started in the child page must not outlive it — otherwise the parent's card and
the engine disagree about what is playing.

## Known limitations — document them, don't paper over them

- **External children reconstruct as a chain of mini-turns.** claude and codex
  persist one row per *stream event*, and any tool-free assistant row takes the
  final-answer arm. A codex run reads as `work(reasoning)` / `work(tool)` /
  `message` / … rather than one work card plus one answer.
- **Approval-gated tools inside a subagent are auto-denied.** The gate is
  resolved per `(channel, session)`; nothing registers one for `subagent`, so it
  falls to `AutoDenyGate` (fail-closed). The read-only page will show denied
  steps that no one denied. Pre-existing, not introduced here.
- **"Worked Xs" on a child undercounts.** Only a `from_user` row sets
  `turn_started`, and a child has none at all, so every work block times from
  its own first intermediate row rather than from the turn's start — the first
  LLM call's stretch is missing from the label.

## Two pre-existing bugs this feature makes visible

Neither is caused by the viewer; both were invisible only because nobody could
look at a child session.

- **A cancelled child leaks the internal cancellation marker into its bubble.**
  Stripping it requires a cancelling `/stop` control event immediately after the
  row, and a child session can never receive one — so the raw model-facing marker
  renders, and the block reads "Worked Xs" instead of "Cancelled".
- **`ToolUse.input` is uncapped.** `cap_external_agent_blocks` caps `Text` and
  `ToolResult.content` and lets `Thinking` and `ToolUse.input` through — and
  codex's file-change tool puts its entire `changes` blob in `input`. Session
  rows are never rewritten, so anything already persisted stays that size.

## Testing

`app/ios` has **no CI** — all three iOS jobs are `if: false` while the Actions
quota is out, and a draft PR reports `skipping` with exit 0, which reads exactly
like green. Every tier here is laptop-only; say in the PR body what you actually
ran.

The predicate is the part worth real tests, gateway-side, and those exist in
`crates/gateway/tests/chat_api.rs`: a non-`owner` root, a hidden one-shot cron
root, a RECURRING cron root (readable — the guard is not a blanket "no cron"), a
depth-2 grandchild, a child whose parent row is missing, and an ordinary session
refused through the subagent route.

Note one asymmetry the tests pin: `GET /v1/chat/sessions/{id}` still serves a
hidden one-shot cron fire by id — only the LISTING drops it — so these routes are
deliberately stricter than the surface they sit beside. On the client, the sheet's grouping/status mapping is pure and unit-
testable; the header button and the sheet get a headless `BayboUITestCase` pass
behind a new `-baybo-demo-*` flag. Any new frame-pushing fixture must guard
`sessionId == AppStore.debugSessionId` — only two existing feeders do, and
without it a fixture writes canned rows into a real conversation's durable
mirror.
