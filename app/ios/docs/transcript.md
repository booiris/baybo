# The transcript webview

_The single reused transcript `WKWebView` and everything that feeds it: `BayboClient`/`ChatStore` lifecycle, the native ⇄ web bridge, keyboard insets, markdown/LaTeX rendering, and the message index. Governs `app/ios/App/Core/TranscriptHost.swift`, `app/ios/App/Web/TranscriptBridge.swift`, `app/ios/App/Web/TranscriptWebView.swift`, `app/ios/App/Core/ChatStore.swift`, `app/ios/App/Core/MessageOutline.swift`, `app/ios/App/Screens/MessageIndexSheet.swift`, and `app/ios/web/src/` (`bridge.ts`, `Transcript.tsx`, `mathDelimiters.ts`, `wireSentinel.ts`, `restSentinel.ts`)._

## BayboClient and store lifecycle

**BayboClient** (ffi) is a long-lived singleton (`Baybo.client`); the chat pump and
parked pairing sessions live inside it between calls. Frames cross the FFI as JSON on a
`FrameSink` callback; `onDisconnected` fires ONLY on unsolicited pump death (deliberate
disconnect aborts first) — the reconnect state machine in `ChatStore` depends on that
contract.

### The LRU store cache

`AppStore` owns a cached `ChatStore` per opened session, LRU-bounded to
`maxResidentStores` (12): after each activation, the least-recently-used stores beyond
the cap that are idle (not the pushed session, `chatPath.last`) are evicted via
`ChatStore.evict()`, which cancels the store's timers so it can deallocate and calls
`chat_unsubscribe` to drop just that session's sink.

A memory warning (`AppDelegate.applicationDidReceiveMemoryWarning` →
`evictAllIdleStores`) evicts every idle store regardless of the cap.

Re-opening an evicted session mints a fresh store that re-subscribes and re-syncs from
the gateway (the webview's mount-edge sync — see [sync-and-outbox.md](sync-and-outbox.md)).

### One global chat leg

The FFI transport owns one global chat leg per binding (relay content or direct channel
WS); opening a session sends a `Subscribe` on that leg and registers/replaces that
session's sink. Switching sessions does not redial or disconnect the old subscription.

Relay bindings also call `relay_preconnect()` on launch/foreground to warm the content
leg before a chat is opened; it dials + handshakes but sends no `Subscribe`.

`chat_unsubscribe` drops one session's sink without touching the leg; logout, rebind, or
explicit app teardown calls `chat_disconnect`, which drops the global leg and all
registered sinks.

### The offscreen frame buffer

Backing out to the list only detaches the `TranscriptBridge`; frames that arrive while no
webview is attached buffer in the store (capped at `maxBufferedFrames`; **on overflow the
buffer is dropped and the transcript refetched from the durable floor on re-attach rather
than flushed with a hole**) and flush in order on the next attach.

## One persistent webview

There is ONE transcript `WKWebView` + `TranscriptBridge`, reused across EVERY
conversation — held by a single `TranscriptHost`
(`app/ios/App/Core/TranscriptHost.swift`) on `AppStore` (`transcriptHost`), booted once
(prewarm / first open) and torn down only on logout/rebind. The bundle is parsed and the
web-content process spun up exactly once for the app's bound lifetime; opening a chat
never re-boots the runtime.

### `retarget(to:)`

`ChatScreen.onAppear` calls `TranscriptBridge.retarget(to:)`:

- a return to the **SAME** conversation the webview still holds just re-attaches +
  flushes buffered frames (React tree intact → instant, no remount, no fade);
- a **DIFFERENT** session flushes the old mirror, then `deliverInit` re-renders the
  transcript (`main.tsx` keys `<Transcript>` on `sessionId` → React unmounts the old tree
  and mounts the new from its own `restoredState`), then flushes the new store's buffered
  frames after `init`, and replays the inset/jump/reveal the `ready` handler would have
  (no page reload fires).

### Cross-session isolation

Isolation is enforced on the WEB side because the webview is shared:

- **(a)** `bridge.ts`'s `init` clears the per-session `buffer` / `blobPending` /
  `pendingPersist` so nothing leaks into the new tree;
- **(b)** `persist` messages carry their originating `sessionId` (native writes under
  THAT id, never the current store) so a late debounced flush can't corrupt the session
  now on screen;
- **(c)** `key={sessionId}` fully resets the React tree (`Transcript.tsx` has no module
  state; blob object URLs are revoked on unmount).

### Per-session resync (the escape hatch)

A device once rendered a session with a whole span of conversation missing that a
freshly installed simulator rendered correctly off the same server data — live
state accumulated across a compaction. `ChatStore.resync(transcript:)` is the
hatch (not the fix): it puts one conversation back into the state a COLD OPEN
would produce, by re-running the cold path rather than by adding a second
synchronisation routine.

**It is reached from the CHAT LIST** — a row's long-press context menu, behind a
`ConfirmDialog` (`AppStore.promptResync` → `requestResync`). The header capsule's
`ModelMenuPanel` carried it first and no longer does: the list needs no
conversation opened first, does not depend on `ModelCatalog` having loaded, and
already owns the other session-level operations (archive / delete / pin). The
confirm is the one in `RootView` that does not wear red (`commitTint: Theme.ink`)
— nothing is taken away — but it still asks, because the thread blanks under a
reader who may be mid-way through it and there is no undo the way archive has one.

Two steps: **delete the mirror** (`SessionIndex.dropTranscriptMirror` — the row
stays), then **reload the page IF the webview is standing on that session**
(`TranscriptBridge.rebuildIfShowing` → the same `webView.load(indexURL)`
`TranscriptHost.init` runs). The reload is the point: a `reset` bridge message
could only clear the state we thought to enumerate, and state that was *not*
cleared when it should have been is the bug being escaped — so the document dies
instead, taking the rows, the cursor and every live latch (open work block,
`turnActive`, the streaming buffer) with it. The rebuilt page then does its own
baseline pull (`sync` with `sinceOrdinal: null`).

The condition is what the list entry point adds, and it is not an optimisation.
From the list most sessions have no page to reload and their next open IS the
cold path. But the chat the user just backed out of still holds the mounted
document, and re-entering it takes `retarget`'s same-session early return — the
live React tree, rows and all — so *that* page has to die now or the hatch reads
as a no-op. The bridge decides, from `shownSessionId`: a stored value, **not**
`store?.sessionId`, because the LRU can evict and deallocate that store while its
transcript is still on screen, and `<Transcript key={sessionId}>` does not remount
when the same id is re-inited.

Three things the hatch must NOT break, and how:

- **The outbox.** Queued sends live in `OutboxStore`, a sibling file — untouched.
  But their optimistic BUBBLES live in the thread that was just thrown away, and
  the gateway has no row to bring a queued send back from; a `failed` one would
  also lose the red dot, its only retry affordance (the payload lives in the web
  row). `ChatStore.replayUnconfirmedSends` re-seeds them, oldest first, with
  `sendFailed` for the failed ones — see
  [sync-and-outbox.md](sync-and-outbox.md#the-re-seed-runs-on-every-mount) for why
  it runs on EVERY mount rather than only after a resync.
- **The mirror coming back from the dead.** The outgoing document's `pagehide`
  flushes its debounced `persist`, which lands *after* the delete and would write
  the discarded state straight back — `deliverInit` would then restore it and the
  hatch would silently do nothing. `TranscriptBridge` drops `persist` writes from
  `rebuildIfShowing()` until the fresh `ready`.
- **Pending approvals.** Native and frame-derived; a sync page carries rows, not
  prompts. Dropping one leaves a gate nobody can answer until it self-denies, so
  `resync` leaves the queue alone.

The LEG is untouched — no unsubscribe, no redial. An in-flight turn keeps running
and its frames keep arriving through the reload (they buffer in the bridge's
`pending` and flush after the fresh `init`), so the rebuilt thread shows that
turn's TAIL only: there is no fresh `SubscribeState` to reconstruct its opening
work block, and no durable row for it yet either. Its terminal `message` lands
authoritatively, and the block fills back in on the next sync after the turn
persists.

Feedback is the composer's notice line ("Rebuilding this chat from the server…"),
raised by `resync` and retracted by the rebuild's own sync answer — a blanking
transcript is ambiguous on a slow link. Reached from the list it lands on the
NEXT visit, which is when the rebuild is actually visible; a store the user has
already left refuses the write (`chatOpen`) and gets no banner, having rebuilt its
page immediately instead. It cannot strand: any later writer takes ownership of
the line (`ChatStore.notice`'s setter clears the flag), and the visit retracts it
(`leaveChat`).

### Reparenting, prewarm, detach

`TranscriptWebView` is a reparenting shim (`makeUIView` returns the host's webview,
`dismantleUIView` only unparents). `prewarmTranscriptHost` boots the webview at home so
the first open is warm; `startNewChat` adopts that prewarmed draft.
`ChatScreen.onDisappear` calls `detachCurrent` (flush mirror + detach), so the offscreen
frame-buffering contract above is unchanged.

## Frame ordering

Sink callbacks hop to the main queue via GCD (FIFO), **not** `Task` — reordered
`answer_delta`s would corrupt the transcript.

## connState

`connState` has exactly one `offline` trigger: a failed dial. Unsolicited drops go back
to `connecting` + 2s backoff; foreground reconnects debounce 400ms; the core coalesces
concurrent dials.

## The native ⇄ web bridge

`app/ios/App/Web/TranscriptBridge.swift` ⇄ `app/ios/web/src/bridge.ts`.

**native→web:**
`init` / `pushFrame` / `setConnEpoch` / `userSent` / `sendFailed` / `blobResult` /
`fileState` / `audioState` / `videoPoster` / `setLanguage` / `setBottomInset` /
`jumpToLatest` / `jumpToMessage` / `outlineLoadOlder` / `requestOutlineHere` /
`requestSync` / `collapseHtmlPreview` / `flushPersist`.

**web→native:**
`ready` / `shown` / `sync` / `mark_read` / `persist` / `fetchHistory` / `requestBlob` /
`queryFileState` / `downloadFile` / `previewFile` / `shareFile` / `viewImage` /
`audioToggle` / `audioSeek` / `queryAudioState` / `playVideo` / `requestVideoPoster` /
`retry` / `openUrl` / `copy` / `log` / `jumpVisible` / `runState` / `outline` /
`outlineHere` / `htmlPreviewMaximized`.

(The blob/file/audio/video messages are covered in [attachments.md](attachments.md).)

`copy` is a user-bubble long-press: native writes `UIPasteboard` + fires a haptic,
because a `file://` WKWebView rejects `navigator.clipboard` outside a live gesture.

Tool approvals deliberately add **NO** bridge message in either direction: the card is
native and reads the frame stream directly, and the transcript's own badges come off
frames it already receives (see [approvals.md](approvals.md)).

### `runState` and the stop button

`runState` mirrors whether a turn is in flight to `ChatStore.agentRunning`, which flips
the composer's send button to a stop control; a tap sends `/stop` as an ordinary
`chatSend` (the agent Router cancels the turn out-of-band; the channel-echoed `/stop`
user message is dropped client-side by `isStopCommand`, so no `/stop` bubble).

Run state is derived from SELF-CORRECTING signals — `awaitingReply` (optimistic post-send
window) `|| workLive` (active work block) `|| streaming` — **never** the raw `turnActive`
latch, which strands true if its closing `turn_state` is lost (offscreen buffer
overflow).

`awaitingReply` is cleared ONLY by race-free signals — an effect on `workLive||streaming`
(real output takes over), the turn's terminal message/notice, `turn_state{inactive}`,
`markFailed`, and an `AWAITING_MAX_MS` timeout backstop — **NEVER by a `sync_page` /
`subscribe_state`**: a session-open/reconnect sync is async and lands just after a send,
so clearing there dropped the stop button back to send until first output (the "stop
appears late" bug).

The webview is the single source of turn state — native never re-derives it.

### Sync and persistence

The `sync` message carries the webview's cursor (`sinceOrdinal`, null = baseline) +
elected page size; native answers with a synthesized `sync_page` frame (or `sync_failed`).

Transcript persistence is the per-session mirror file, a pure `{rows, cursor}` cache
(**NOT** webview localStorage — `file://` storage is unreliable and upgrade-fragile). See
[sync-and-outbox.md](sync-and-outbox.md).

## Keyboard

The transcript webview is FULL-BLEED and its frame **never** tracks the keyboard — a
keyboard-resized WKWebView relayouts once, async, at the final size, so content sits
still through the slide and snaps at the end.

Instead `ChatScreen` measures the composer's top edge (it rides the keyboard via safe
area) and feeds the covered strip over `setBottomInset`; SwiftUI geometry jumps to the
target at animation START, so the web side animates `--thread-bottom-inset` on a
keyboard-like 250ms curve (`.chat-log.inset-animated`) and re-pins the newest edge per
frame while following.

One signal covers keyboard, composer growth, and the notice line.

## Wire-type sentinels

Two compile-time pins, both type-only, both enforced by `pnpm build` (`ios-web` in CI is
the only place they are ever evaluated):

`app/ios/web/src/wireSentinel.ts` (and `app/web/src/api/wireSentinel.ts` for the web chat)
pin the hand-written frame mirrors to the ts-rs-generated contract
(`sidecars/sdk/channel-ts/src/generated/`) — a wire-side rename/retype fails the build.
`bigint`→`number` is mapped (this bundle receives JSON, not the SDK's msgpack).

`app/ios/web/src/restSentinel.ts` does the same for `TranscriptRowItem`, the shape the
`sync_page` / `history_page` rows arrive in. That one is not a ts-rs type — the ffi passes
each row through as an untouched `serde_json::Value`, so what lands here is the gateway's
utoipa DTO `ChatTranscriptItem`, which `scripts/check-ts-bindings.sh` does not cover.
The sentinel reads `app/web`'s generated schema across the project boundary (type-only,
so `tsc` follows the path and no bundler or pnpm resolution is involved) rather than
generating a second byte-identical copy here — a second copy would be a second thing to
regenerate, i.e. a new drift surface inside the gate built to close one. It asserts
assignability BOTH ways: every generated key must be mirrored (an addition nobody reads
yet is invisible otherwise — how `turn_complete` sat unused for months) and every
mirrored key must still exist. Two deliberate
exemptions, both explained in the file: `Option<T>` fields carry
`skip_serializing_if = "Option::is_none"`, so their `null` never rides the wire, and
`ChatAttachment` stringifies the wire's `AttachmentKind` enum.

## Transcript rendering

Web-chat parity, mobile-restyled: user messages keep the black bubble; assistant replies
are bubble-less full-width `react-markdown` + `remark-gfm` prose, rendered live WHILE
streaming (rAF-coalesced; the web app only applies markdown on finalize).

`reasoning` / `tool_started` / `tool_completed` / transient-notice frames fold into a
per-turn collapsible work block ("处理中" card → "处理了 Xs ›"); answer text interrupted by
more work settles into the block as a prose step.

**The collapse hides the machinery, not the words.** `segmentWorkSteps` (`WorkBlock.tsx`)
splits the step list into maximal alternating runs of *speech* (`prose`) and *machinery*
(reasoning / tool / status / notice); only machinery answers to the "处理了 Xs ›" toggle,
and the chevron is rendered — and the button enabled — only when there is machinery to
hide. Speech renders in every state at `.work-said`, which is deliberately the same
reading band as `.msg.assistant` (1rem Inter / 1.6). That equality is the point:
mid-turn text arrives as an ordinary streaming reply and is reclassified as intermediate
only retroactively, so `foldStreamingIntoProse` used to shrink the paragraph the user was
mid-way through reading from 1rem to 0.85rem and slide it into the step feed, then hide it
altogether at turn end. Matching the destination to the source makes the fold a visual
no-op. Ordering derives from `steps[]`, which the live leg, the `subscribe_state` bundle
and the REST reconstruction all agree on, so a cold reload paints the same shape as the
live view. The web chat implements the same rule under the same function name
(`app/web/src/pages/ChatPage.tsx`), with the same case table in both test suites — there
is no gate enforcing that, so change the two together.

The zh label is 「处理中」/「处理了 Xs」, deliberately NOT 「思考」. It used to say "thinking",
which was already loose and became wrong once the collapse stopped covering narration: what is
left behind the toggle is reasoning, tool calls and status lines, so a tool-heavy turn was being
labelled as time spent thinking. 处理 is the neutral verb that covers both halves and mirrors the
en copy's Working/Worked. Don't "fix" it back.

`bundleAnswer` is three-valued about the reply on screen, and the third value is the trap:
`recovered` (the bundle's trailing prose IS the answer in flight — paint it into `streamingText`),
`superseded` (it carries answer text but has moved past it — the reply on screen is stale, clear it)
and `unknown` (**no answer text at all — say nothing, leave it alone**). An empty or machinery-only
bundle is not evidence of staleness: `AgentEvent::Message` / `TurnState` clears the channel's
in-flight buffer while `active_turn_started_at` keeps reporting the turn active through post-answer
finalization, and the buffer stops recording at `MAX_INFLIGHT_ENTRIES` — clearing there deletes a
paragraph the user is mid-read of. The web chat mirrors this by name.

The REST plane needs the same hoist the `subscribe_state` bundle gets. `build_history_page` folds
the live channel's in-flight buffer into the trailing work block, and an `AnswerDelta` lands there
as a `prose` step — so a BASELINE sync taken mid-answer returns a page whose trailing block ends
with the text `streamingText` is already painting below it. `dropInFlightAnswerStep` strips it in
`applySyncPage`'s REPLACE branch (gated on `turnActiveRef.current`); without it the paragraph
renders twice, and because the collapse no longer hides prose the duplicate survives turn end and
is written into the mirror. Safe because a persisted prose step is never a block's last — an
intermediate row's Text and its ToolUse are one persisted row.

Prose step identity is load-bearing for this: `mergeWorkSteps` keys a prose step by the
tool call it PRECEDES, not by its text, because two identical short paragraphs in one turn
("我看下测试。") would otherwise collapse to one — a silently deleted paragraph now that
narration renders. See `workStepKeys` in `Transcript.tsx` and the `repeated narration
survives` cases in `transcript/rows.test.ts`.

A reconstructed row's own judgements are read, not re-derived: a `/stop`ped turn's block
carries `cancelled` and its closed card says so ("已取消 · 处理了 Xs") instead of reading as
an ordinary completion — the live block learns it only when the reconstruction reconciles
in, so either side carrying it cancels the card. A reloaded notice keeps its
`notice_level` (the same warn/error ramp a folded notice step uses), and a persisted tool
step with no `tool_status` stays NEUTRAL — the old "ok" default painted every
result-less call green.

### Timestamps

Every message row carries a **timestamp under its last bubble** (`.msg-time`), sided by
the group's `align-items` — the agent's at the reply's bottom-LEFT, the user's at the
bubble's bottom-RIGHT (web-chat parity; notices, which are centered marks, get none).

`ChatMsg.createdAt` is the server's `created_at` on a reconstructed row; the wire
`Frame::Message` has **no time field**, so a live reply and an optimistic send are stamped
on arrival and a later sync redelivery adopts the server's clock over that stamp (else the
time under a bubble would shift between the live session and the next cold open). Rows in
a mirror written before this existed simply have none.

### Links

Markdown links post `openUrl` to native (system browser) — an in-webview navigation would
replace the thread.

### Agent-authored HTML previews

An assistant opts into a live preview with one fenced marker whose body is a blob
capability id, never the HTML source:

````markdown
```baybo-html
sha256:<64-lower-hex-digest>.<lower-hex-read-token>
```
````

The owner-only built-in `html-gen` skill authors a self-contained page and calls
the owner-only, content-generic `PutBlob` tool with `mime_type: "text/html"` and
a 16 MiB caller cap. The tool returns structured metadata; the skill takes its
`blob_id` and writes this marker into the final reply. `PutBlob` returns no
attachment, so `AttachFile` keeps its normal send-file semantics and no duplicate
file card is added. Ordinary `html` fences remain ordinary source code.

`HtmlPreview` renders the marker as a 420pt inline iframe from
`baybo-transcript://localhost/html-preview/<blob-id>`. The scheme handler reads
the blob cache-first (no base64 bridge copy), fixes the response MIME to
`text/html`, and supplies the CSP:

```
default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline';
img-src data:; connect-src 'none'; frame-src 'none'; object-src 'none';
base-uri 'none'; form-action 'none'
```

The iframe is `sandbox="allow-scripts"` with no `allow-same-origin`, so it has
an opaque origin: no storage/cookies, parent DOM, or same-origin access.
`TranscriptBridge` also rejects every non-main-frame script message because
WKWebView exposes native handlers inside subframes; the preview cannot bypass
the sandbox by calling `window.webkit.messageHandlers.baybo`. The response is
not stored in WebKit's cache and its Permissions Policy disables device-facing
APIs. Finally, `TranscriptNavigationPolicy` permits only the transcript index
in the main frame and `/html-preview/` in a subframe, so page code cannot
replace its frame with an external URL to escape the no-network CSP.

The card is a framed window: a surface-tinted chrome bar (line-art window glyph
+ label, then stroked reload / expand controls in the transcript's own 20-unit
1.2-weight glyph hand) over the page. Its height is `--html-preview-h` —
nothing on this side can measure an opaque-origin page, so the card picks a
height rather than fitting one, capped as a share of the viewport so a small
phone still shows the reply around it.

The toolbar reloads and expands the SAME iframe. Fullscreen is CSS-fixed rather
than reparented (no page reload or JS-state loss), while
`htmlPreviewMaximized` hides the native header/composer; the trusted parent
toolbar owns the close button. Detach/session-switch sends
`collapseHtmlPreview`, so a reopened chat never gets stranded behind an old
fullscreen preview — and that path alone is instant, because the page is about
to be repointed at another session.

#### Expanding and collapsing — the rect morph

**The maximize engine is Deck's**, down to the timing (`--html-preview-morph`
mirrors `.deck-max-animate`), for the reason Deck's exists: an `<iframe>`
reloads when its node MOVES in the DOM, so the box never moves. It lifts to
`position: fixed` at the exact rect the inline card occupied and animates by
RECT — `left/top/width/height` — so the page inside is laid out at every
intermediate size rather than drawn scaled. The `<pre>` holds that slot open
(`--html-preview-h`) so the collapse has somewhere true to land, and the
collapse re-measures the slot rather than replaying a remembered rect (rows keep
arriving while a preview is up, so its home may have moved down the thread).

The transition itself is declared in `styles.css` and the geometry is written
from `HtmlPreview.tsx`, which waits on `transitionend`; its timer is a loose
upper bound, not a copy of the duration. Three writes are load-bearing:

- `flushSync` puts the fullscreen class on in the SAME synchronous block that
  pinned the starting rect — a scheduled render would paint the box at full
  screen first;
- the chrome bar sheds its status-bar padding one morph EARLY on the way out,
  since the class that would do it only flips at the far end; and
- **the morph is scaled to the distance actually left**
  (`--html-preview-morph-scale`), and a box already home is retired on the spot.
  Leaving `position: fixed` tears the box's compositing layer down and repaints
  every glyph in it — unavoidable, but it has to land WITH the motion. A release
  after a long edge drag has almost no travel left, and running the full
  duration anyway parked the box for a quarter second and THEN repainted:
  measured on a real agent page at 60fps as a single 280ms-late flash of the
  whole card. The scale is a custom property so the bar's own transition
  inherits it and lands with the box. It has a floor
  (`MIN_MORPH_SCALE`) from both directions: a transition scaled to nothing never
  runs, and a tail under ~5 frames stops reading as motion.

#### The morph's end state must BE the final state

Everything above is one rule: when the class flips, nothing may still be
mid-flight. Two ways it was broken, both found by diffing 60fps frames of a real
agent page and both fixed:

- **`.is-maximized` differed from the card in more than the rect** — it carried
  `padding-bottom: env(safe-area-inset-bottom)`, no border and square corners.
  Parked on the slot's rect the box was still reserving a home indicator, so the
  page inside grew ~34pt at the bottom the instant the class flipped. All of it
  now derives from `--html-preview-expansion` (0 = card, 1 = full screen), which
  `HtmlPreview.tsx` writes EARLY — a dismissal starts at 0, a drag rides
  `1 - travelled` — so the values travel with the box. **Anything added to
  `.is-maximized` that is not position/rect has to go through that variable.**
- **the settle retired the box on the first `transitionend`** — that event fires
  PER PROPERTY, and the morph moves seven of them. Whichever landed first flipped
  the class while the rest were a pixel or two short. It now waits on
  `getAnimations()`, the whole set.

**A fade was tried first and cannot work here.** The card can only be in one
place, so the moment the fullscreen box drops below full opacity the reader is
looking at the EMPTY reserved slot behind it. Measured off a 60fps capture:
~80ms of blank screen at the end of every dismissal, and a blank first frame on
every expand. Do not reintroduce an opacity-based enter or exit.

Two ways out, both the same morph home:

- the toolbar's close button; and
- **a left-edge swipe**, which native lends to the preview
  (`EdgeSwipeOverride`, see [`navigation.md`](navigation.md)): with a preview up,
  the interactive pop is held off and the drag arrives here as
  `htmlPreviewDragBegin` / `…Move(px)` / `…End(dismiss)`.

  The finger drives the SAME morph — the box shrinks toward its slot, it does
  not slide. A translate would be a second, unrelated motion that the release
  then has to hand over to a shrink, and it drags a full-screen page sideways,
  which reads as the conversation itself moving. A full screen-width drag
  arrives all the way home; native commits the dismissal well before that, so
  the release always has travel left to animate. Rects are written straight to
  the element — no React state, so a drag costs no re-render — and the chrome
  bar sheds its status-bar padding along the way rather than in one step at the
  release (it is the only part of the box whose height a class flip owns).

Two more things keep the uncovered thread still, because the swipe reveals it
*while it travels*:

- native's chrome stays at FULL HEIGHT behind the preview (opacity 0, inert) —
  the composer's geometry is what native reports as the thread's bottom
  obstruction, and collapsing it moved that edge; and
- `postHtmlPreviewMaximized(false)` fires when the dismissal STARTS, not when it
  finishes — but the native chrome CROSS-FADES with the morph rather than
  cutting (`ChatScreen.chromeFade`). It is composited ABOVE the webview, so a
  hard cut showed at both ends: on the way in it vanished a beat before the box
  had grown over the thread (three raw frames of headerless conversation), and
  on the way out the back button and composer pill were drawn ON TOP of a
  still-full-screen agent page.

And one more, in the stylesheet: the rule that lifts `.md`'s clip (an ancestor's
`overflow: clip` also clips a `position: fixed` descendant) is scoped with
`:has` to the ONE message hosting the box. Applied to every `.md` under the
document class — which is how it started — it relaid every message in the thread
on the way in and again on the way out.

### LaTeX math

**LaTeX math** renders via `remark-math` + `rehype-katex` + `katex` (CSS/fonts bundled by
Vite, served by the transcript scheme handler — no CDN), preprocessed by `normalizeMath`
(`app/ios/web/src/mathDelimiters.ts`):

- `\(..\)` / `\[..\]` are rewritten to dollars in the raw source (CommonMark eats the
  backslashes before any remark plugin runs);
- a whole-line column-0 `$$..$$` is promoted to its own lines so it renders as a centered
  DISPLAY block (**NOT** via `\n\n` injection — that fractures a list/table/blockquote
  holding display math), while an embedded `$$` stays inline;
- and `guardDollars` (a linear pandoc-style pairing scanner) escapes any `$` that is not
  part of a valid inline math span — a `$...$` is math only if a valid CLOSER exists (not
  after whitespace, not before a digit), which is the "Balanced" `$` policy: `$x$`,
  `$3.14$`, `$2 + 2 = 4$` render, but prose money "$5 and $3" / "$12.50" stays literal (a
  local `$`+digit heuristic was tried and **rejected** — it destroyed decimal/arithmetic
  math).

Code is masked by a linear line scanner (no ReDoS; any-indent fences so a list-nested
fence is caught; an unclosed fence — the mid-stream state — masks to end-of-input).

Display math centers when it fits and left-scrolls when it doesn't
(`.katex-display > .katex { width: fit-content; margin-inline: auto }`, **not**
`text-align: center` — a centered wide equation clips its own unreachable left edge under
`overflow-x`), and `.md { overflow-x: clip }` keeps a wide inline expression from
scrolling the whole transcript sideways.

## Message index

`app/ios/App/Screens/MessageIndexSheet.swift` + `app/ios/App/Core/MessageOutline.swift` ⇄
`Transcript.tsx`'s `outlineEntries`: the chat header's trailing glass circle
(`text.alignright`) opens a sheet listing the user's OWN sends, each glossed with the
agent's answer; a tap parks the transcript on that message.

### The web layer owns the list

**The WEB layer owns the list** and native only renders it — `ChatStore.send` seeds the
optimistic bubble over the bridge and never routes through `pushFrame`, so anything
derived from the native frame stream would not see this device's own message until the
server echoed it back, and an offline send would never appear at all (the web tree also
filters `/stop` echoes and holds the loaded window).

Deriving from the same `messages` array the transcript renders is what guarantees every
listed row has a `data-row-id` anchor — the sheet can never offer a jump it cannot reach.
It is therefore a WINDOW over the loaded thread, surfaced honestly as `24+` plus a "load
earlier" row that runs the transcript's own backward paging.

### The jump

The jump is web-side:

- clear `followRef` SYNCHRONOUSLY before the scroll write (five writers slam `scrollTop`
  to the bottom while it is set);
- never SET `glidingRef` (its only self-clear is entering the bottom follow band, which an
  upward jump never reaches — clearing a stale one is fine, and the jump does);
- and let `onScroll` own `showJump` — which is also how "back to the newest edge" comes
  free, as the existing jump-to-latest circle.

Landing clearance is `.msg-group.user`'s `scroll-margin-top`, not arithmetic at the call
site. The arrival ring mounts inside `.bubble.user` (or the last `.attachment-bubble`)
because `.msg-group` is unpositioned, and replays off a NONCE — a boolean would
`Object.is`-bail a repeat jump to the same row.

### Two traps

1. The five `@Published` outline mirrors reset in BOTH `retarget(to:)` and `case "ready"`
   (one webview, every conversation).
2. `deliver()` in `bridge.ts` ends in a bare `else e.jumpToLatest()`, so every new
   `Buffered` variant needs its own `else if` **ABOVE** it or the command silently becomes
   "scroll to the bottom" — TypeScript cannot see it; `bridge.test.ts` pins it.
