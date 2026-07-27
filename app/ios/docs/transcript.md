# The transcript webview

_The single reused transcript `WKWebView` and everything that feeds it: `BayboClient`/`ChatStore` lifecycle, the native ⇄ web bridge, keyboard insets, markdown/LaTeX rendering, and the message index. Governs `app/ios/App/Core/TranscriptHost.swift`, `app/ios/App/Web/TranscriptBridge.swift`, `app/ios/App/Web/TranscriptWebView.swift`, `app/ios/App/Core/ChatStore.swift`, `app/ios/App/Core/MessageOutline.swift`, `app/ios/App/Screens/MessageIndexSheet.swift`, and `app/ios/web/src/` (`bridge.ts`, `Transcript.tsx`, `mathDelimiters.ts`, `wireSentinel.ts`)._

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
`requestSync` / `flushPersist`.

**web→native:**
`ready` / `shown` / `sync` / `mark_read` / `persist` / `fetchHistory` / `requestBlob` /
`queryFileState` / `downloadFile` / `previewFile` / `shareFile` / `viewImage` /
`audioToggle` / `audioSeek` / `queryAudioState` / `playVideo` / `requestVideoPoster` /
`retry` / `openUrl` / `copy` / `log` / `jumpVisible` / `runState` / `outline` /
`outlineHere`.

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

## Wire-type sentinel

`app/ios/web/src/wireSentinel.ts` (and `app/web/src/api/wireSentinel.ts` for the web chat)
pin the hand-written frame mirrors to the ts-rs-generated contract
(`sidecars/sdk/channel-ts/src/generated/`) at compile time — a wire-side rename/retype
fails `pnpm build`. `bigint`→`number` is mapped (this bundle receives JSON, not the SDK's
msgpack).

## Transcript rendering

Web-chat parity, mobile-restyled: user messages keep the black bubble; assistant replies
are bubble-less full-width `react-markdown` + `remark-gfm` prose, rendered live WHILE
streaming (rAF-coalesced; the web app only applies markdown on finalize).

`reasoning` / `tool_started` / `tool_completed` / transient-notice frames fold into a
per-turn collapsible work block ("思考中" card → "思考了 Xs ›"); answer text interrupted by
more work settles into the block as a prose step.

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
