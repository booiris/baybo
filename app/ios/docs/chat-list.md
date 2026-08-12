# Chat list, unread, and push routing

*The device-local session registry behind the chat list, the live unread and approval marks that ride on it, the app-icon badge, and where a push tap lands. Governs `SessionIndex`, `SessionActivityHandler`, `ChatRowBody`, `app/ios/App/Core/BadgeCenter.swift`, `AppDelegate.registerForPush`, the NSE, and the FFI `SessionListSink` plumbing.*

## Chat list data

`SessionIndex` (`Application Support/baybo/sessions.json`) is the device-local
registry backing the list on BOTH legs.

Both direct and relay merge `chat_list_sessions()` over it on
appear/foreground/pull:

- **direct** uses REST `GET /v1/chat/sessions` with the stored Bearer plus
  `x-baybo-device-id`;
- **relay** uses the Noise-protected API tunnel.

### Merge rules

Remote wins for existence (a row missing remotely was hidden elsewhere).

In-flight local mutations (`pendingMutations`) and the `mutationEpoch` guard
beat a stale snapshot; otherwise server values win wholesale — a local row only
fills fields the server left nil (never overrides them).

### Transcript mirrors

Per-session transcript mirrors live in
`Application Support/baybo/transcripts/<id>.json`, one per session this device
has rendered, and **nothing sweeps them** — see "Transcript mirror retention" in
[sync-and-outbox.md](sync-and-outbox.md).

The legacy single-session UserDefaults keys (`ChatDefaults`) are migrated once
and retired.

### Row gestures

Trailing swipe archives / deletes, leading swipe pins — the three everyday verbs
(`ChatListScreen.chatRow`). **Long-press** carries the other two, in a
`.contextMenu` rather than more swipe buttons: a menu row has space to say what
it does, and rename in particular opens an editor, which is not something a
fling should raise.

- **Rename** (`AppStore.promptRename` → `RenameDialog`) — see
  [Renaming a conversation](#renaming-a-conversation) below.
- **Resync**, the per-session transcript rebuild
  ([transcript.md](transcript.md#per-session-resync-the-escape-hatch)). It lives
  on a row because it needs no conversation opened first and belongs beside the
  other session-level operations; the header capsule's model panel carried it
  first and no longer does.

The long-press is one shared modifier, `sessionContextMenu`, and **every screen
that lists a conversation applies it** — this list, `CronGroupScreen`'s fires and
`ArchivedScreen` — because where a row is listed says nothing about whether it
can be named or its transcript can drift. A new session-listing screen wires it
in too.

## Searching conversations

Full-text search over every conversation's prose, served by the gateway's
`GET /v1/chat/search` — the same endpoint app/web's `SearchPanel` calls, so this
is **one protocol implemented twice** and anything that differs between the two
clients is a bug on one of them. The index itself is documented in
[`docs/search.md`](../../../docs/search.md); nothing about it is iOS-specific.

**Entry point: the tab bar's trailing circle.** `HomeTab.search` carries
`TabRole.search`, and on iOS 26 the system lifts a search-role tab OUT of the
glass pill and floats it as its own detached circle at the trailing edge. On
18–25 the role degrades to an ordinary tab item, so no version branch is needed.
**The field takes the tab bar's place.** Selecting search hides the native bar
(`.toolbar(.hidden, for: .tabBar)`, driven by `homeTab == .search`) and
`SearchScreen` docks its own field there via `.safeAreaInset(edge: .bottom)`,
with a ✕ circle trailing it. The field animates from the search circle's own
footprint (`circleDiameter`, 62pt) out to full width, so the detached circle
reads as stretching into a field — the Telegram shape. A `safeAreaInset` rather
than an overlay so the results list insets itself and its last card never parks
under the field.

**The field carries no magnifier glyph, and that is not an oversight.** It
coexists with the native tab bar for the length of the bar's own hide/show
animation — entering AND leaving — and the bar's search circle sits at exactly
that end of the screen, so a glyph in the field put two magnifiers one above the
other. The bar's timing is not the app's to control (it returns on its own
schedule, not on the tab swap — verified by holding the swap for the animation's
full duration and watching the bar come back anyway), so the fix is to stop
drawing the duplicate rather than to sequence them. The placeholder reads
"Search"; the glyph was decoration. The field's CONTENTS also fade with the
stretch, so the shrinking pill empties out instead of carrying text into a 48pt
circle.

✕ calls `exitSearch()`, which returns to `tabBeforeSearch` — the tab search was
opened FROM, recorded in `homeTab`'s `willSet`. Returning to a hardcoded `.chats`
would be a different bug wearing the same clothes, and `SearchUITests` enters
from Deck specifically to catch it.

The tab's content is `SearchScreen`, mounted WITHOUT the shared `section`
wordmark header — the docked field is the only chrome it needs.

**Opening a hit keeps the tab.** `openSearchResult` passes `keepTab: true`, the
one exception to `activateSession`'s `homeTab = .chats` rule. It works because
the push lands on the OUTER `NavigationStack` — the one that WRAPS the whole
`TabView` — so the conversation covers the shell and the tab selection is simply
preserved underneath it. Popping returns to the Search tab with its query and
results intact. Without `keepTab` the back gesture would land on the chat list
with the results gone, which is the whole reason the flag exists.

`SearchScreen` focuses its field on ENTERING the tab, not on every `onAppear`: a
`TabView` keeps its pages alive, so `onAppear` also fires when the reader comes
back from a conversation, and raising the keyboard over results they just
navigated back to is not what they asked for.

**The keyboard lands well after the field opens, and that is not jank.** Measured
on 26.5 across repeated entries: SwiftUI does not APPLY the focus until ~700ms
after the tab change (steady, ±20ms), and the keyboard follows within ~40ms of
that — so the slow part is the focus handoff, not the keyboard. Disabling the
stretch entirely and deferring the focus request to the next runloop each changed
nothing, which places the cost in the `TabView` page transition rather than in
anything this screen does. The stretch is deliberately left immediate: syncing it
to `keyboardWillShow` would make the two move together at the price of ~700ms of
dead bottom bar right after the tap. Note the measurement is simulator-only — a
device may well be faster.

The idle state draws **nothing**: the field's placeholder is the only instruction
it needs. `.empty` / `.failed` still render, because those are answers to a query
rather than instructions.

**Scope is the gateway's default and is not configurable from the client:**
hidden sessions stay lost, archived ones stay archived, and cron *workspaces* —
fire sessions that are not conversations of their own — are excluded
server-side. That last one is not cosmetic. Such a session is dropped by
`/v1/chat/sessions` and 404s on the REST attach path, but the read path and the
device channel's `Subscribe` both scope by CHANNEL only, so before
`SearchScope::include_cron_workspaces` existed a hit there let the phone open,
read and even post into a conversation no client can list.

**`SearchModel` owns the querying**, apart from the view, so the three rules
that are easy to get wrong are testable without a UI host:

- a **300ms debounce** (longer than app/web's 200ms — that panel talks to
  localhost, this one may cross a relay tunnel budgeted at 15s to first byte);
- a **monotonic sequence** guard, because cancelling the task cannot un-send a
  request already awaiting its answer and the relay leg can reorder two of them;
- **no request while an input method has an open composition**
  (`FocusedTextInput.isComposing`) — a Chinese keyboard puts the uncommitted
  pinyin in the binding, so a naive debounce spends a tunnel round trip on
  `shuju` and flashes "no matches" against a query nobody typed.

Results stay on screen while the next query is in flight; only the first query
blanks the view.

**Excerpts are highlighted by `SearchSnippet`**, a port of app/web's
`searchSnippet.ts` held to it byte-for-byte by shared vectors
(`app/web/src/pages/chat/searchSnippetVectors.json`, checked here by
`SearchSnippetVectorTests` and there by its own vitest suite; regenerate with
`pnpm --filter baybo-web gen:snippet-vectors`). Both ports work in **grapheme
cluster** space, which is what keeps a window edge from splitting an emoji ZWJ
sequence or stranding a combining mark. A card's title prefers THIS device's row
(`SessionHeadline`, the same rule the list's bold line uses) so one conversation
is not called two different things two screens apart.

### Jumping to a hit

Each excerpt is its own button: with anchored jumping every hit is a distinct
destination, so a card-wide tap target would show the reader the line they
wanted and land them on a different one.

The ordinal travels in `AppStore.pendingJump[sessionId]`, consumed once by
`ChatScreen.onAppear` — **not** on the route (`ChatRoute` is `Hashable` and
compared for equality in several places, so a payload would make one
conversation two routes) and **not** by calling the bridge at the tap site (the
queued JS would be lost if the `TranscriptHost` were rebuilt in between).

`TranscriptBridge.jumpToOrdinal` addresses the row **by ordinal, never by row
id**. A user row is keyed by its `platform_msg_id` with the ordinal carried
beside it, so building `m<ordinal>` resolves agent rows and silently misses
every user-authored hit — most of what a search finds. The web side resolves
through `rowCoverageOrdinal`, which knows both shapes.

The rest of the mechanism lives in
[`transcript.md`](transcript.md#jumping-to-a-search-hit): the row usually is not
loaded, and the window has no forward frontier, so reaching it means paging
backward under a budget.

**`superseded_by` is not a jump target.** It names the ordinal where a
compaction's re-inserted rows begin — rows the display read excludes — while the
superseded ORIGINAL still renders, so `ordinal` is always the address. See
`docs/search.md`; the gateway's own doc comment used to say the opposite.

## Renaming a conversation

`PUT /v1/chat/sessions/{id}/title` (`chat_set_title` over the active leg). Titles
are otherwise machine-written — the auto-titler generates one from the first user
question — and a rename settles the conversation against it: the titler writes
only where there is no title (`set_title_if_absent`), so a hand-written name is
never overwritten.

**The rules live in `RenameTitle`**, the Swift mirror of the web's
`renameTitle.ts` and, through both, of the gateway's `validate_session_title`:
interior whitespace collapsed, ends trimmed, capped at 80 **Unicode scalars**
(Rust's `chars()`, not Swift graphemes). The client normalizes rather than merely
validating, because the string it sends is the one it renders optimistically —
the endpoint stores the normalized form and broadcasts *that*, so anything else
would visibly rewrite itself a moment later.

An **empty** field cannot be committed and an **unchanged** one commits nothing.
There is deliberately no clear/reset-to-auto: an absent `SessionPatch.title`
already means "unchanged" on the wire, so a cleared title has no representation.
"Unchanged" is measured against the SEED the editor opened with (snapshotted in
`AppStore.PendingRename`) — for an untitled row that seed is the last user
message, and committing it would rename the conversation to its own preview.

**The staged intent** (`SessionIndex.pendingTitles` + `titleBaselines`) is its
own map, not a fourth `PendingMutation` case: that map holds ONE desired state
per session, so a rename staged there would discard a pending archive/pin. It
shields the optimistic title from both other writers — a REST snapshot composed
before the rename, and a live `SessionUpdated` title patch, which mid-rename can
only be an older auto-title the user's PUT is about to replace. `AppStore.pumpRename`
serializes one request per session (`pumpSessionMutation`'s contract) so two
renames cannot land out of order; a failure rolls back to the last
server-acknowledged title, including back to *no* title.

The editor is `RenameDialog`, hosted at the app root beside the confirms. It is
the one dialog here that raises a keyboard, so it owns its own avoidance and the
surfaces it floats over (`HomeTabView`, `ArchivedScreen`, `CronGroupScreen`) opt
out with `.ignoresSafeArea(.keyboard)` — otherwise the whole shell, glass tab bar
first, slides up behind the scrim while the user types.

## Live list unread

The gateway broadcasts a throttled `Frame::SessionActivity` (per-session ping,
no content) to EVERY connection on the `owner` channel — subscribed or not —
when a user send echoes or a session's turn completes (`SessionPulse`, installed
on the shared `owner` chat channel that both the web dashboard and this app
register as; TUI is excluded).

The FFI transport special-cases that frame in `dispatch_inbound_frame`, routing
it to a connection-global `SessionListSink` (set once via
`set_session_list_sink`) instead of the per-session `FrameSink` — so a session
the device never opened still updates the list.

`SessionActivityHandler` → `SessionIndex.noteActivity` bumps `SessionRow.unread`
and recency (persisted; ignored for the foreground session and unknown ids) as a
between-pulls accelerator — the badge is server-computed (`unreadCount` on the
list summary) and reconciled on every list merge, and the webview's `mark_read`
advances the server-side read cursor (`chat_mark_read`) so the badge clears
across devices.

`ChatScreen` enter/leave marks the foreground session and clears its badge.

Relay warms the leg via `relay_preconnect`; direct via `direct_preconnect` (both
best-effort on launch/foreground) so the pings arrive while parked on the list.

## Chat-list approval mark

A conversation whose tool call is parked on the gateway's approval gate wears an
ink `hand.raised` glyph in the row's trailing meta column, leading the unread
capsule (`ChatRowBody.approvalMark`); a cron GROUP row ORs it over its fires.

The bit is `SessionRow.approvalPending`, fed by two paths that mirror unread's:

- server-computed `approvalPending` on the REST list summary (cold-start truth);
- a connection-global `SessionUpdated{approval_pending}` patch tee'd to
  `SessionListSink` (`on_approval_pending` → `SessionIndex.noteApprovalPending`)
  as the between-pulls accelerator.

### Why a broadcast PATCH, not `Frame::ApprovalRequested`

It rides a broadcast PATCH rather than `Frame::ApprovalRequested` because that
frame only reaches connections **subscribed to that session**, and the client
that needs it is the one parked on the list, subscribed to nothing.

### Why the QUEUE publishes the edges

Server-side the edges are published by the approval QUEUE itself
(`PendingEdge::Raised/Answered/Abandoned`), because the five-minute gate timeout
and a `/stop` both retire a prompt through `QueueCleanup::drop` and broadcast no
resolution at all — a mark hung off `ApprovalResolved` would stick forever on
exactly the turns nobody answered.

### Never restored from disk

`approvalPending` is **never decoded from disk**: a parked gate lives in gateway
memory, so a mark restored on an offline cold start could only describe a prompt
that no longer exists.

`-baybo-demo-approval` with `-baybo-open-home` flips three rows live 2s in
(screenshot before/after).

## App-icon badge

`BadgeCenter` (`app/ios/App/Core/BadgeCenter.swift`) is the one writer on the app
side, driven from `SessionIndex.save()` (the funnel every list mutation already
passes through) and counting exactly what the main list counts — archived rows
excluded, coalesced on equality.

The gateway is the other writer, sealing a `badge` into the encrypted preview so
the NSE can set `content.badge` while the app is dead; it counts through the same
`fold_unread` as the chat list's per-row badges, so the icon and the rows cannot
drift into two implementations.

Only `SessionIndex.shared` owns the icon (`ownsAppBadge`) — the suites run in
parallel against temp directories and would otherwise race over the host app's
real badge.

### Full authorization, not provisional

**`AppDelegate.registerForPush` asks for FULL authorization**
(`[.alert, .sound, .badge]`), not provisional: provisional is granted silently
but delivers *quietly* — no sound, no lock-screen alert, and no badge — which is
fine for "your agent replied" and useless for "a tool call denies itself in five
minutes unless you answer".

iOS honours `options` only on the FIRST determination, so an install that already
took provisional on an earlier build does not widen its grant;
`logAuthorizationState` records what was actually granted so that case is
observable instead of presenting as a feature that silently never works.

## Push tap routing

The gateway embeds `session_id` INSIDE the encrypted preview plaintext (never the
outer APNs payload — C stays blind, matching the hashed collapse-id invariant).

The NSE decodes it (optional field; the pinned AEAD fixture predates it and must
keep decoding) and stashes it in the delivered `userInfo` under
`PushPayloadKeys.sessionId` (one file compiled into both targets).

The app's `UNUserNotificationCenterDelegate` routes the tap to that session via
`AppStore.routeToSession` (stash-and-consume across the launch restore);
foreground pushes present nothing.
