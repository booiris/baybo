# Baybo iOS (SwiftUI generation)

The native successor of `app/mobile` (the Tauri shell): a **SwiftUI app** whose
screens, header, and composer are native — so the iOS keyboard never touches web
content — with **only the chat transcript** rendered in a WKWebView, and the
transport/crypto core kept in **Rust behind UniFFI**. For app behavior the root
`/CLAUDE.md` applies; the visual system follows the retired Tauri app's guide
(monochrome soft line minimalism — the file went with `app/mobile`, read it with
`git show 6141f57b^:app/mobile/CLAUDE.md`) until this app grows its own.

## Layout

```
Cargo.toml            — own cargo workspace (root workspace excludes app/ios)
ffi/                  — UniFFI core: transport legs, pairing, keychain, blobs
                        (lifted from app/mobile/src-tauri, Tauri replaced by
                        callback interfaces). Exports BayboClient + parsePairQr.
bindgen/              — uniffi-bindgen CLI (separate member so the `cli`
                        feature never unifies into the lib build)
project.yml           — xcodegen spec (the committed source of truth)
App/                  — SwiftUI sources + resources
NotificationExtension/ — NSE Swift sources (the sole copy — app/mobile is gone)
web/                  — the transcript-only Vite/React bundle
scripts/              — build-core.sh, build-app.sh, install.mjs, verify-nse.sh
Generated/ Externals/ — build products (gitignored): BayboCore.swift + .xcframework
```

## Build

```bash
scripts/build-app.sh             # web → rust xcframework → xcodegen → sim build
scripts/build-app.sh --device --release
node scripts/install.mjs         # archive + export + devicectl install (USB)
cargo clippy --workspace --all-targets --all-features   # zero warnings
cargo nextest run --workspace    # ffi host tests
(cd web && pnpm build)           # tsc --noEmit + vite build
```

## Tests

Four tiers. The whole base — Rust + the transcript bundle — is plain Linux work
and runs on ubuntu in CI at 1x; only the Swift half pays the macOS 10x.

```bash
# Rust core (app/ios workspace — the ROOT workspace excludes it, so the root
# `cargo test --workspace` has never covered any of this)
cargo nextest run --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Transcript bundle (own pnpm workspace — the root `frontend` job never sees it)
(cd web && pnpm lint)     # eslint: wiring-bug gate (suppression baseline)
(cd web && pnpm test)     # vitest: reducers, sync cursor, bridge isolation, WorkBlock render
(cd web && pnpm build)    # tsc --noEmit -> enforces web/src/wireSentinel.ts

# Swift. Build once, then run either bundle against the built products.
xcodegen generate
xcodebuild build-for-testing -project Baybo.xcodeproj -scheme Baybo \
  -sdk iphonesimulator -destination 'platform=iOS Simulator,name=iPhone 17 Pro,OS=latest' \
  -derivedDataPath build/DerivedData
xcodebuild test-without-building -project Baybo.xcodeproj -scheme Baybo \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro,OS=latest' \
  -derivedDataPath build/DerivedData -only-testing:BayboTests    # or :BayboUITests
```

- **`ffi/`** — inline `#[cfg(test)] mod tests`. The load-bearing ones pin things
  no other check can see: the `PairedRecord` / `DirectCredentials` **golden JSON**
  (the on-keychain byte format is a frozen upgrade contract — break it and every
  upgraded install silently loses its gateway binding), `dispatch_inbound_frame`'s
  four routing invariants, the `invalid_token` untyped-string chain, and
  `since_ordinal` serializing as an **explicit null** (add `skip_serializing_if`
  and a baseline REPLACE quietly becomes an APPEND).
- **`web/`** — vitest + jsdom, mostly over the pure reducers. Mounting
  `<Transcript>` was tried and rejected: jsdom reports `scrollHeight` as 0, so
  every follow/pin/jump branch degenerates — the transcript is tested through its
  extracted reducers, and the simulator demo flags cover what a reducer test
  cannot. A **small presentational component with no scrollHeight/follow/pin
  dependency is fine to render**, though: `WorkBlock.test.tsx` mounts
  `WorkBlockView` (React Testing Library) for its active/closed/toggle wiring —
  do NOT extend this to `<Transcript>`. `pnpm lint` (eslint, mirroring app/web —
  strict-boolean-expressions / no-unnecessary-condition / react-hooks, with a
  suppression baseline for the existing backlog) gates the wiring-bug class over
  `src`.
- **`Tests/` (`BayboTests`)** — Swift Testing (`@Test`/`#expect`), a
  **host-application** bundle (hostless would relink the Rust staticlib and make
  `Lang.t` return bare keys). `ChatStore` takes an injected `any
  BayboClientProtocol` — the protocol UniFFI already generates — so the frame
  paths, the approval queue, and the outbox's two-stage confirmation are testable
  with no gateway. XCTest stays for `BayboUITests` (Swift Testing has no UI API);
  both bundles coexist.
- **`UITests/` (`BayboUITests`)** — XCUITest over the `-baybo-*` fixture flags.
  Non-gating in CI (gesture/timing driven). Reserve it for what is *unreachable*
  below the seam: SwiftUI hit-testing (the stroke-only pill whose flanks were
  dead), UIKit swipe thresholds, presentation-binding lifecycles, the native↔web
  bridge round-trip. Everything else is 100x cheaper as a reducer test.

**`BayboUITestCase` is the base class and every smoke must use its `launch(_:)`.**
It pins three things a test cannot get right by accident:
`-baybo-reset-store` (below), `-baybo.lang en` (our strings), and
`-AppleLanguages (en)` (the *system* chrome — AVKit, the share sheet — which
follows the simulator's locale, not our UserDefaults key; without it a test
matching system chrome by label passes locally and fails on a runner).

**`-baybo-reset-store`** (DEBUG, `AppDelegate`) wipes `Application Support/baybo`
before anything reads it. The demo fixtures use FIXED session ids, so without it
each launch APPENDS its canned turn to the same persisted mirror — and one
simulator is shared across a suite, so the attachment demo reached six video
tiles and a by-label query that is unambiguous on a fresh install started
matching six elements. Demo flags are only hermetic with it.

CI (`.github/workflows/ci.yml`): `ios-checks` (ubuntu, gating) runs the Rust and
web tiers; `ios-sim` (macos-26, gating build + unit tests, non-gating UI smokes)
runs the Swift half. Both are path-filtered — and the filter deliberately
includes `crates/{wire,device-proto,model}` and `remote-host/`, because the ffi
workspace path-depends on them and a root-crate change can break the iOS build
without touching `app/ios` at all.

The Rust core is built OUTSIDE Xcode (no shell build phase): `build-app.sh` runs
`build-core.sh` (cargo per-target + uniffi-bindgen + create-xcframework)
before `xcodegen generate`, so the project always references fresh products.
`generate_context!`-style staleness does not exist here, but the ORDER still
matters: web bundle → `App/Resources/transcript/` → xcodegen → xcodebuild.

**Device builds need the device slice AND a signed xcframework.** `build-app.sh`
defaults to sim-only (`XCF_FLAGS=(--sim-only)`), so a plain run produces a
sim-only `BayboCore.xcframework`; switching Xcode's destination to a physical
device then fails with *"no library for this platform was found."* Pass
`--device` (or run `build-core.sh` with no flags) to add the `ios-arm64`
slice. Xcode 15+/26 also rejects any xcframework referenced by a device build
that isn't code-signed (*"The Framework … is unsigned"*) — `-create-xcframework`
emits an unsigned bundle, so `build-core.sh` now `codesign`s it for
non-sim-only builds (identity via `BAYBO_IOS_CODESIGN_IDENTITY`, default
`Apple Development`). Run the signing build from an interactive Terminal in your
GUI login session: codesign needs the unlocked login keychain, and a headless
shell fails with `errSecInternalComponent` / "User interaction is not allowed."

**Sim-verification loops must not clobber the device xcframework.** A plain
`build-app.sh` run overwrites `Externals/BayboCore.xcframework` with a sim-only
unsigned bundle, and the next Xcode device Run fails with exactly the two
errors above. When iterating on Swift/web only (no `ffi/` changes), pass
`--skip-rust`; after a full run does clobber it, restore with
`scripts/build-core.sh` (no flags). Check the result with `codesign --verify`
— a failed headless sign (`errSecInternalComponent`) still leaves a fresh
`_CodeSignature/` dir in the bundle, so its presence proves nothing.

## Continuity contract (do not change — existing installs depend on it)

- Bundle ids `com.baybo.app` / `com.baybo.app.NotificationExtension`; team
  `KLK5BP5YS6`.
- `keychain-access-groups`: `$(AppIdentifierPrefix)com.baybo.app` stays the
  FIRST (only) entry — the five app-private keychain items live in the default
  group, which is the first entitlement entry.
- Keychain items set **no `kSecAttrService`** (a query with any service string
  finds nothing). Accounts: `baybo.push-key.<bid>` (shared group,
  AfterFirstUnlock), `baybo.paired-gateway`, `baybo.device-identity`,
  `baybo.device-sign-key` (never deleted), `baybo.direct-credentials`,
  `baybo.active-binding` (all ThisDeviceOnly). `ffi/src/keychain.rs` is a
  verbatim port — treat it as frozen.
- The `PairedRecord` / `DirectCredentials` JSON field names are the on-keychain
  byte format shared with Tauri-shell installs.
- NSE `Info.plist` key `BayboKeychainAccessGroup`; push payload/decrypt
  contract unchanged (see `docs/modules/mobile/relay-push-security.md` § Notify
  flow and `docs/modules/mobile/companion.md` § Push preview).
- APNs environment is passed from Swift (`ClientConfig.apnsEnv`, per build
  config) — never `cfg!(debug_assertions)` in Rust, which usually compiles in
  release even for debug apps.

## Architecture notes

- **Navigation**: the home shell (`AppStore.Route.home`) is `HomeTabView`, a
  NATIVE `TabView(selection: $homeTab)` (Liquid Glass bar on iOS 26+, the
  classic system bar on 18–25 — system chrome, degrades on its own) with four
  sections (Deck · Projects · Chats · Settings, `AppStore.HomeTab`). `deck`
  (`DeckScreen` — the board of agent-authored live cards, see the Deck bullet
  below and `docs/modules/deck.md`), `chats` (`ChatListScreen`) and `settings`
  (`SettingsScreen` — language, version, log out) have real screens;
  `projects` is `PlaceholderScreen`. An OUTER
  `NavigationStack(path: $chatPath)` in `RootView` WRAPS the whole TabView;
  opening a session pushes `ChatScreen` over the ENTIRE shell (tab bar
  included), so the bar reveals together with the pop transition. (Do NOT move
  the stack inside the Chats tab and hide the bar with `.toolbar(.hidden, for:
  .tabBar)` — that reappears the bar abruptly AFTER the pop, the "bar missing
  then pops in" glitch.) No session is minted at launch or login; the compose button —
  the Chats header's top-right glass circle — is the only session creator, and
  compose / push-tap routing force `homeTab = .chats` (in `activateSession`) so
  a pushed conversation lands in the Chats stack. `.tint(Theme.ink)` colours the
  selected tab item ink (the HIG blesses a monochromatic tab bar); the selection
  capsule is the system Liquid Glass material — no public API recolours it and
  none is wanted (neutral glass, no forced blue). Compose is NOT in the tab bar:
  the native bar is for navigation not actions (HIG) and exposes no slot for a
  custom button — an earlier custom glass pill bar (with a separate compose
  circle) was dropped for exactly this, to get the native selection morph. The
  `ChatScreen` still hides the system nav bar (custom chrome), which disables
  UIKit's interactive pop — `PopGestureEnabler`
  (attached to ChatScreen) re-enables the edge-swipe back with a root +
  in-flight-transition guard, hands the delegate back on disappear, and
  clamps `velocityInView:` (dynamic subclass, `PopVelocityClamp`) so iOS 26's
  fluid pop can't inherit a fast flick's velocity and overshoot the revealed
  list (the "list slides right then rubber-bands" glitch; stock Settings
  does the same, it just hides it better). The clamp install is
  `#available(iOS 26, *)`-gated: pre-26 pops don't overshoot, and UIKit's
  finish/completion math reads the same velocity — capping it there would
  only make fast flicks feel laggy.
- **Deck** (`docs/modules/deck.md` — the design doc is the source of truth):
  the Deck tab renders the app's SECOND webview (`DeckHost`, kept warm like
  `TranscriptHost`, torn down with the binding), loading `deck.html` — a
  second Vite entry in the SAME `web/` dist the transcript uses, so it rides
  the existing `App/Resources/transcript/` copy + scheme handler with no extra
  build step. The shell (`web/src/deck/`) draws the 2-column size-class grid
  and hosts each card in an `<iframe sandbox="allow-scripts" srcdoc>` (opaque
  origin + injected CSP: no network) with a per-card `MessagePort` as the
  card's identity. `DeckStore` (`App/Core/DeckStore.swift`) is the engine:
  REST refetch over the active leg (`deckFetch`), a `deck.json` mirror for
  instant offline paint, live pushes via the connection-global `DeckSink`
  (`DeckEventsRelay` — the `SessionActivityHandler` idiom; `Frame::DeckCardData`
  is accepted iff its seq beats the cached one, `Frame::DeckChanged` triggers a
  refetch), optimistic layout writes with baseline rollback, and op calls
  (`deckCall`) correlated back to the card that asked. Destructive card
  actions confirm NATIVELY (the shell only reports intent). Bridge
  (`App/Web/DeckBridge.swift` ⇄ `web/src/deck/bridge.ts`): native→web
  `init/deckState/cardData/bundle/callResult/setEditMode/setLanguage`;
  web→native `ready/refetch/requestBundle/call/layout/cardAction/editMode/quickSetup/maximize/log`
  (`quickSetup` is the empty-board CTA: native opens a fresh chat and
  auto-sends a `/deck` request, via `AppStore.startCardDraft`; `maximize`
  reports a card entered/left its full-screen layout so `DeckScreen` fades the
  wordmark header out — the tab bar stays — while `DeckStore.maximized` is set).
  Card size is per-card adaptive: the manifest declares `sizes` (the grid
  sizes it implements; the ⤢ cycle stays in that set) and `maximize` (a
  full-screen `deck.size === "max"` layout via a ⛶ button); both ride the
  `DeckCardInfo`/`DeckStore.Card` model and the shell drives the expand with
  the drag engine's fixed-tile trick (zero iframe reload).
- **Chat list data**: `SessionIndex` (Application Support/baybo/sessions.json)
  is the device-local registry backing the list on BOTH legs. Both direct and
  relay merge `chat_list_sessions()` over it on appear/foreground/pull: direct
  uses REST `GET /v1/chat/sessions` with the stored Bearer plus
  `x-baybo-device-id`, while relay uses the Noise-protected API tunnel. Remote
  wins for existence (a row missing remotely was hidden elsewhere): in-flight
  local mutations (`pendingMutations`) and the `mutationEpoch` guard beat a
  stale snapshot; otherwise server values win wholesale — a local row only fills
  fields the server left nil (never overrides them). Per-session transcript
  mirrors live in `Application Support/baybo/transcripts/<id>.json`, one per
  session this device has rendered, and **nothing sweeps them** — see
  "Transcript mirror retention" below; the legacy single-session UserDefaults
  keys (`ChatDefaults`) are migrated once and retired.
- **Live list unread**: the gateway broadcasts a throttled `Frame::SessionActivity`
  (per-session ping, no content) to EVERY connection on the `owner` channel —
  subscribed or not — when a user send echoes or a session's turn completes
  (`SessionPulse`, installed on the shared `owner` chat channel that both the
  web dashboard and this app register as; TUI is excluded). The FFI transport
  special-cases that frame in
  `dispatch_inbound_frame`, routing it to a
  connection-global `SessionListSink` (set once via `set_session_list_sink`)
  instead of the per-session `FrameSink` — so a session the device never opened
  still updates the list. `SessionActivityHandler` → `SessionIndex.noteActivity`
  bumps `SessionRow.unread` and recency (persisted; ignored for the foreground
  session and unknown ids) as a between-pulls accelerator — the badge is
  server-computed (`unreadCount` on the list summary) and reconciled on every
  list merge, and the webview's `mark_read` advances the server-side read cursor
  (`chat_mark_read`) so the badge clears across devices. `ChatScreen`
  enter/leave marks the foreground session and clears its badge. Relay warms the
  leg via `relay_preconnect`; direct via `direct_preconnect` (both best-effort
  on launch/foreground) so the pings arrive while parked on the list.
- **Push tap routing**: the gateway embeds `session_id` INSIDE the encrypted
  preview plaintext (never the outer APNs payload — C stays blind, matching the
  hashed collapse-id invariant). The NSE decodes it (optional field; the pinned
  AEAD fixture predates it and must keep decoding) and stashes it in the
  delivered `userInfo` under `PushPayloadKeys.sessionId` (one file compiled
  into both targets). The app's `UNUserNotificationCenterDelegate` routes the
  tap to that session via `AppStore.routeToSession` (stash-and-consume across
  the launch restore); foreground pushes present nothing.
- **BayboClient** (ffi) is a long-lived singleton (`Baybo.client`); the chat
  pump and parked pairing sessions live inside it between calls. Frames cross
  the FFI as JSON on a `FrameSink` callback; `onDisconnected` fires ONLY on
  unsolicited pump death (deliberate disconnect aborts first) — the reconnect
  state machine in `ChatStore` depends on that contract. `AppStore` owns a
  cached `ChatStore` per opened session, LRU-bounded to `maxResidentStores` (12):
  after each activation, the least-recently-used stores beyond the cap that are
  idle (not the pushed session, `chatPath.last`) are evicted via `ChatStore.evict()`, which cancels the
  store's timers so it can deallocate and calls `chat_unsubscribe` to drop just
  that session's sink. A memory warning
  (`AppDelegate.applicationDidReceiveMemoryWarning` → `evictAllIdleStores`) evicts
  every idle store regardless of the cap. Re-opening an evicted session mints a
  fresh store that re-subscribes and re-syncs from the gateway (the webview's
  mount-edge sync). The FFI transport owns one global chat leg per binding
  (relay content or direct channel WS); opening a session sends a `Subscribe` on
  that leg and registers/replaces that session's sink. Backing out to the list
  only detaches the
  `TranscriptBridge`; frames that arrive while no webview is attached buffer in
  the store (capped at `maxBufferedFrames`; on overflow the buffer is dropped and
  the transcript refetched from the durable floor on re-attach rather than flushed
  with a hole) and flush in order on the next attach. Switching sessions does not
  redial or disconnect the old subscription. Relay bindings also call
  `relay_preconnect()` on launch/foreground to warm the content leg before a chat
  is opened; it dials + handshakes but sends no `Subscribe`. `chat_unsubscribe`
  drops one session's sink without touching the leg; logout, rebind, or explicit
  app teardown calls `chat_disconnect`, which drops the global leg and all
  registered sinks.
- **Single persistent transcript webview**: there is ONE transcript `WKWebView`
  + `TranscriptBridge`, reused across EVERY conversation — held by a single
  `TranscriptHost` (`App/Core/TranscriptHost.swift`) on `AppStore`
  (`transcriptHost`), booted once (prewarm / first open) and torn down only on
  logout/rebind. The bundle is parsed and the web-content process spun up exactly
  once for the app's bound lifetime; opening a chat never re-boots the runtime.
  `ChatScreen.onAppear` calls `TranscriptBridge.retarget(to:)`: a return to the
  SAME conversation the webview still holds just re-attaches + flushes buffered
  frames (React tree intact → instant, no remount, no fade); a DIFFERENT session
  flushes the old mirror, then `deliverInit` re-renders the transcript (main.tsx
  keys `<Transcript>` on sessionId → React unmounts the old tree and mounts the
  new from its own `restoredState`), then flushes the new store's buffered frames
  after `init`, and replays the inset/jump/reveal the `ready` handler would have
  (no page reload fires). Cross-session isolation is enforced on the WEB side
  because the webview is shared: (a) `bridge.ts`'s `init` clears the per-session
  `buffer`/`blobPending`/`pendingPersist` so nothing leaks into the new tree;
  (b) `persist` messages carry their originating `sessionId` (native writes under
  THAT id, never the current store) so a late debounced flush can't corrupt the
  session now on screen; (c) `key={sessionId}` fully resets the React tree
  (`Transcript.tsx` has no module state; blob object URLs are revoked on
  unmount). `TranscriptWebView` is a reparenting shim (`makeUIView` returns the
  host's webview, `dismantleUIView` only unparents). `prewarmTranscriptHost`
  boots the webview at home so the first open is warm; `startNewChat` adopts that
  prewarmed draft. `ChatScreen.onDisappear` calls `detachCurrent` (flush mirror +
  detach), so the offscreen frame-buffering contract below is unchanged.
- **Frame ordering**: sink callbacks hop to the main queue via GCD (FIFO), not
  `Task` — reordered `answer_delta`s would corrupt the transcript.
- **connState** has exactly one `offline` trigger: a failed dial. Unsolicited
  drops go back to `connecting` + 2s backoff; foreground reconnects debounce
  400ms; the core coalesces concurrent dials.
- **Bridge** (`App/Web/TranscriptBridge.swift` ⇄ `web/src/bridge.ts`):
  native→web
  `init/pushFrame/setConnEpoch/userSent/sendFailed/blobResult/fileState/audioState/videoPoster/setLanguage/setBottomInset/jumpToLatest/requestSync/flushPersist`;
  web→native
  `ready/shown/sync/mark_read/persist/fetchHistory/requestBlob/queryFileState/downloadFile/previewFile/shareFile/viewImage/audioToggle/audioSeek/queryAudioState/playVideo/requestVideoPoster/retry/openUrl/copy/log/jumpVisible/runState`.
  (`copy` is a user-bubble long-press: native writes `UIPasteboard` + fires a
  haptic, because a `file://` WKWebView rejects `navigator.clipboard` outside a
  live gesture.) Tool approvals deliberately add NO bridge message in either
  direction: the card is native and reads the frame stream directly, and the
  transcript's own badges come off frames it already receives (see "Tool
  approvals" below). `runState` mirrors whether a
  turn is in flight to `ChatStore.agentRunning`, which flips the composer's send
  button to a stop control; a tap sends `/stop` as an ordinary `chatSend` (the
  agent Router cancels the turn out-of-band; the channel-echoed `/stop` user
  message is dropped client-side by `isStopCommand`, so no `/stop` bubble). Run
  state is derived from SELF-CORRECTING signals — `awaitingReply` (optimistic
  post-send window) `|| workLive` (active work block) `|| streaming` — never the
  raw `turnActive` latch, which strands true if its closing `turn_state` is lost
  (offscreen buffer overflow). `awaitingReply` is cleared ONLY by race-free
  signals — an effect on `workLive||streaming` (real output takes over), the
  turn's terminal message/notice, `turn_state{inactive}`, `markFailed`, and a
  `AWAITING_MAX_MS` timeout backstop — NEVER by a `sync_page`/`subscribe_state`:
  a session-open/reconnect sync is async and lands just after a send, so clearing
  there dropped the stop button back to send until first output (the "stop
  appears late" bug). The webview is the single source of turn state — native
  never re-derives it. The
  `sync` message carries the webview's cursor (`sinceOrdinal`, null = baseline)
  + elected page size; native answers with a synthesized `sync_page` frame (or
  `sync_failed`). Transcript persistence is the per-session mirror file, a pure
  `{rows, cursor}` cache (NOT webview localStorage — file:// storage is
  unreliable and upgrade-fragile).
- **File attachments** (`kind != "image"`) render as a tappable card whose glyph
  is a download arrow until the blob is on disk, then a document. Tapping fetches
  it; tapping a fetched one opens `FilePreviewSheet` — `QLPreviewController`
  wrapped in `UIViewControllerRepresentable` (SwiftUI's `quickLookPreview` is
  **macOS-only**; it does not exist in the iOS SDK), falling back to
  `UIActivityViewController` when `QLPreviewController.canPreview` says no
  (archives, unknown binaries). QuickLook picks its previewer from the file
  extension, and the core's cache names files by digest, so `previewFile` writes
  `<tmp>/baybo-preview/<digest>/<real name>` first.
  An inline `image` attachment does NOT use this path — a decoded
  `AttachmentImage` is a button whose tap posts `viewImage` (blob id only), and
  `ChatStore.viewImage` decodes the device-cached blob into a `UIImage` and
  presents `ImageViewer` (`App/Screens/ImageViewer.swift`) via
  `.fullScreenCover`. That is a dedicated `UIScrollView`-backed zoomable viewer
  (pinch, double-tap-to-fit/restore, single-tap or ✕ to close, image fades onto a
  black field) rather than QuickLook: QuickLook embedded in a SwiftUI `.sheet`
  gave no reliable double-tap-to-restore (the sheet's gestures fight it), and the
  black edge-to-edge field matches chat images where the document previewer's
  white chrome does not. The blob is already on disk from the thumbnail fetch
  (`requestBlob` → `blob_download_bytes` writes the cache), so it opens instantly.
  (Files still use `previewFile` → QuickLook above.)
  **Compute the zoom fit from the scroll view's `layoutSubviews`, never from
  `updateUIView`** — SwiftUI calls `updateUIView` BEFORE UIKit lays the scroll
  view out, so `bounds` is still zero, a `bounds > 0` guard bails, and it never
  runs again: min = max = zoomScale = 1, the image renders at native size, pinch
  does nothing, and double-tap has no smaller scale to restore to (this exact bug
  shipped once). Re-fit only when `bounds.size` actually changes, or the layout
  passes that zooming itself triggers re-seat the image frame and fight the zoom.
  The viewer's top-right button opens the system share sheet on the blob
  materialised under its real name (`writePreviewFile`, shared with the file
  path) — the FILE, not the decoded `UIImage`, so Save-to-Photos / Files /
  AirDrop keep the original encoding. That share sheet is why the app carries
  **`NSPhotoLibraryAddUsageDescription`**: without it iOS TERMINATES the app the
  moment the user taps "Save Image". `ContentBlock::Image`/`Audio` carry an
  `Option<String>` filename end to end (`AttachFile` → transcript →
  `split_content` → `WireAttachment.filename`), so an agent's image shares under
  its REAL name; a genuinely nameless one (pasted screenshot, MCP bitmap) falls
  back to `attachment.<ext>` derived from the mime. Transcripts persisted before
  that field existed still load (`#[serde(default)]`) — they simply have no name,
  so an OLD message's image keeps sharing as `attachment.png`.
  **An image the transcript has decoded before shows NO loading state at all.**
  Every decode records the image's natural `[w,h]` (keyed by the blob's sha256
  DIGEST — the read token rotates, the digest doesn't) into
  `PersistedState.imageDims`, so it rides the per-session mirror to disk. On the
  next open the bubble is `sized`: `.attachment-bubble.sized` (styles.css) solves
  the same contain-fit the `<img>` will (`min(100%, natural, --attachment-max-h ×
  ratio)` + `aspect-ratio`) and reserves the EXACT final box from the first paint
  — no 12rem tile, no spinner, no release. That release was the bug: a re-opened
  thread grew/shrank every image row as its bytes landed (measured: page height
  3332 → 3396 px on ONE image), and WKWebView has no scroll anchoring to absorb
  it, so the page shook under the reader. The fit MUST be solved on the BUBBLE:
  the frame's containing block is the bubble, a shrink-to-fit flex item, where a
  `%` width is cyclic and WebKit resolves it to zero (a 0×0 reservation). The
  mirror can outlive its blobs (a restored backup carries the transcript, not the
  blob cache), so the spinner still exists inside the reserved box — just delayed
  400ms in CSS, invisible on the cache hit it exists to skip. An image with no
  recorded size (first view, a scrolled-up history page) keeps the old tile and
  records its size on the way through.
  The spinning ring is **indeterminate on purpose** — the byte counter beside it
  (`884 KB / 2.3 MB`) is the progress. Bytes come from the core's `BlobProgress`
  callback (`blob_download_bytes(blob_id, progress)`), rate-limited to one tick
  per 100ms **in Rust**: a 100 MiB download hands the chunk loop thousands of
  buffers, and every tick would otherwise cross the FFI and the webview bridge.
  `downloaded` is bytes ON DISK, so a resumed download (both legs send
  `Range: bytes=N-`) opens at its floor instead of snapping back to zero.
  `blob_is_cached` is the mount-time probe. Downloaded blobs live in
  `Application Support/baybo/blobs` (`ClientConfig.blobCacheDir`, set in
  `Baybo.swift`) — **not** the OS temp dir, which iOS reclaims under storage
  pressure: a file the user downloaded stays downloaded. The directory is
  excluded from backup (a blob runs to 100 MiB and is always re-fetchable) and
  **nothing evicts from it** — it only grows. That is deliberate; when it needs
  bounding it wants a stated retention policy, not a surprise sweep. `ready` is
  still re-asked on every mount rather than remembered, because the directory is
  a fact about disk.
  Cards subscribe to `fileState` **by blob id** (`onFileState`), so a progress
  tick re-renders one card and `MessageRow`'s memo survives — and two cards on
  the same blob (an agent's file the user quotes back) update together.
  **A long-press on any DOWNLOADED file / audio / video card shares it**
  (`useSharePress` → `shareFile` → `ChatStore.fileShare` → the system sheet on
  the materialised file, real name intact). The synthetic click that follows
  the lift is swallowed in the capture phase so a share never also
  downloads/plays/previews; an undownloaded card ignores the hold and keeps
  its plain tap. The audio card's seek bar stops `touchstart` propagation so a
  slow scrub can't arm the share. Images share from inside their viewer; the
  video player carries the same top-right share button.
  **Bridge ANSWERS buffer across the detach window like frames do**: a
  `fileState` (or `videoPoster` reply) that lands while no webview is attached
  is stashed in the store (`pendingFileStates` last-write-wins per blob /
  `pendingPosterReplies`) and flushed on `attachBridge` — a download whose
  terminal `ready` fell while the user was parked on the list used to wedge
  its card at `loading` forever, because a SAME-session re-attach remounts
  nothing and so re-queries nothing. A flushed poster reply whose session
  switched away settles nothing web-side (`init` cleared `posterPending`) and
  is ignored.
- **Audio attachments** (`kind == "audio"`) render as the file card with the
  glyph slot promoted to a play/pause control once the blob is on disk
  (`AttachmentAudio`; the download flow is the file card's, unchanged). The
  track's LENGTH rides the wire — `WireAttachment.duration_ms`, probed by
  `AttachFile` at attach time (the one moment the file is in hand
  server-side) and carried through `ContentBlock::Audio` → `split_content` →
  the REST `ChatAttachment` — so the resting card reads `MP3 · 3:23 · 3.3 MB`
  before any byte is downloaded or played; `None` (inbound channel audio, old
  rows) just drops the middle segment. The probe must not trust headers or
  extensions (measured on synthetic 240s files): a VBR MP3 without a Xing
  header estimates 6× off from first-frame bitrate math, so `audio/mpeg` gets
  a full frame walk (`mp3-duration`); an Opus stream inside `.ogg` fails
  lofty's extension guess, so lofty runs behind a content sniff
  (`guess_file_type`). The ENGINE is native — `AudioPlayerCenter` (`App/Core/AudioPlayerCenter.swift`),
  ONE `AVPlayer` app-wide — driven over the bridge
  (`audioToggle`/`audioSeek`/`queryAudioState` in, `audioState` pushes out:
  play/pause flips, 2 Hz position ticks, `stopped` on end/usurp). Native
  rather than an in-webview `<audio>` because the bytes never cross the bridge
  as base64, AVAudioSession `.playback` means the ringer switch can't silence
  it, and — with `UIBackgroundModes: audio` (project.yml) + Now Playing +
  remote commands — a track keeps playing through lock/background with
  Control Center transport while the user stays IN the chat. Backing out to
  the chat LIST stops it (`AppStore.chatPath`'s didSet, when the last
  `.session` route leaves the stack — NOT `ChatScreen.onDisappear`, which
  also fires under fullScreenCovers like the image viewer): audio with no
  visible card to control it reads as a bug. Playback runs off the
  materialised preview file (`materializePreviewFile` — AVPlayer sniffs the
  container by extension). Starting a track stops the previous one and tells
  its card `stopped`; a card mounting mid-playback resyncs via
  `queryAudioState`; `resetChatStores` (logout/rebind) stops the player
  outright. Engine-truth invariants (each covers a wedge that review found):
  the card mirrors EVERY engine flip via KVO on `timeControlStatus` — the
  system pauses without any interruption notice (headphones unplugged, a
  stall) and the card would otherwise wedge on "playing" with an inverted
  toggle; `AVPlayerItem.status == .failed` / `failedToPlayToEndTime` reset the
  card to rest (an unplayable blob must not play dead air forever); "is it
  playing" checks are `timeControlStatus != .paused` (right after `play()` the
  engine sits in `.waiting…` — intent is playing); an `ended` latch keeps a
  finished track answering `stopped` to late `queryAudioState` (the player
  stays loaded for instant replay, but a remounting card must not resync to an
  engaged "paused @ 0:00" the live card never showed). The engine opens the
  asset with `AVURLAssetPreferPreciseDurationAndTiming` — the default
  duration is a bitrate GUESS on headerless/VBR containers (a 4:00 ogg
  reported 5:04), and the seek bar maps fractions onto it, so an imprecise
  duration also mis-aims every scrub; the card remembers the precise engine
  duration and never falls back to the wire estimate it disproved. The seek
  bar (`AudioTrack`) renders in EVERY state so the card's height never jumps
  as playback starts/ends — inert and empty until the engine engages (a tap
  on it bubbles to the card and just plays). Engaged: drags scrub locally,
  commit ONE `audioSeek` on lift, and the committed value keeps rendering
  until the engine's next push (native answers a seek with an optimistic
  state) — dropping it at lift would snap the fill back to the pre-seek
  playhead. `touch-action: none` so a scrub never scrolls the thread.
- **Video attachments** (`kind == "file"` + `video/*` mime — video has no wire
  kind of its own; `isVideoAttachment` elects the tile by mime) render as a
  fixed-width tile in the image idiom (`AttachmentVideo`): undownloaded, a
  blank surface with a centered download disc and `1:23 · 24 MB` in a corner
  chip — the LENGTH rides the wire like audio's (`ContentBlock::File` carries
  `duration_ms` for videos; `AttachFile` probes mp4/mov via the `mp4` crate
  and webm/mkv via `matroska`), the size is what a tap commits to, and once
  the bytes are local the chip drops to just the length;
  while fetching, the disc becomes a DETERMINATE progress ring (the attachment
  declares its total; the corner chip counts bytes); downloaded, native
  supplies a poster frame + duration over `requestVideoPoster`
  (`AVAssetImageGenerator` on the materialised file, first frame downscaled to
  ≤1024px JPEG — cached as `poster.jpg` + `poster.json` beside the preview
  file, because the tile re-requests on EVERY remount and the generator is too
  heavy to re-run each time) and the disc becomes a play glyph. Tapping a
  downloaded tile
  posts `playVideo` → `ChatStore.videoPlayback` presents `VideoPlayerScreen`
  (`App/Screens/VideoPlayerScreen.swift`) via `.fullScreenCover`: an embedded
  `AVPlayerViewController` on a black field with the viewer chrome's ✕ disc
  (`ViewerChromeButton`, shared with `ImageViewer`) — embedded AVKit shows no
  Done button, only owned presentations get one. Chat audio is stopped before
  the video presents (two engines over one AVAudioSession fight), and
  `playVideo` bails if the bridge detached while the file materialised — the
  user backed out, and presenting late would arm a stale `fullScreenCover` for
  the NEXT entry. Poster/play materialisations coalesce in-flight per target
  path (`previewMaterializations`) so a poster request racing a play tap
  doesn't hold the video in memory twice. The poster's
  natural size is recorded into the same `ImageDimsStore`/`imageDims` mirror
  keyed by blob digest, so a re-opened thread reserves the tile's ratio from
  the first paint; the ratio is clamped to [3:4, 16:9] (`clampVideoRatio`) and
  the cover-fit poster absorbs the clamp as a crop.
- **Keyboard**: the transcript webview is FULL-BLEED and its frame never
  tracks the keyboard — a keyboard-resized WKWebView relayouts once, async,
  at the final size, so content sits still through the slide and snaps at the
  end. Instead ChatScreen measures the composer's top edge (it rides the
  keyboard via safe area) and feeds the covered strip over `setBottomInset`;
  SwiftUI geometry jumps to the target at animation START, so the web side
  animates `--thread-bottom-inset` on a keyboard-like 250ms curve
  (`.chat-log.inset-animated`) and re-pins the newest edge per frame while
  following. One signal covers keyboard, composer growth, and the notice line.
- **Wire-type sentinel**: `web/src/wireSentinel.ts` (and
  `app/web/src/api/wireSentinel.ts` for the web chat) pin the hand-written
  frame mirrors to the ts-rs-generated contract
  (`sidecars/sdk/channel-ts/src/generated/`) at compile time — a wire-side
  rename/retype fails `pnpm build`. `bigint`→`number` is mapped (this bundle
  receives JSON, not the SDK's msgpack).
- **Transcript rendering** (web-chat parity, mobile-restyled): user messages
  keep the black bubble; assistant replies are bubble-less full-width
  react-markdown + remark-gfm prose, rendered live WHILE streaming
  (rAF-coalesced; the web app only applies markdown on finalize). `reasoning`
  / `tool_started` / `tool_completed` / transient-notice frames fold into a
  per-turn collapsible work block ("思考中" card → "思考了 Xs ›"); answer text
  interrupted by more work settles into the block as a prose step. Every
  message row carries a **timestamp under its last bubble** (`.msg-time`),
  sided by the group's `align-items` — the agent's at the reply's bottom-LEFT,
  the user's at the bubble's bottom-RIGHT (web-chat parity; notices, which are
  centered marks, get none). `ChatMsg.createdAt` is the server's `created_at`
  on a reconstructed row; the wire `Frame::Message` has **no time field**, so a
  live reply and an optimistic send are stamped on arrival and a later sync
  redelivery adopts the server's clock over that stamp (else the time under a
  bubble would shift between the live session and the next cold open). Rows in
  a mirror written before this existed simply have none. Markdown
  links post `openUrl` to native (system browser) — an in-webview navigation
  would replace the thread. **LaTeX math** renders via `remark-math` +
  `rehype-katex` + `katex` (CSS/fonts bundled by Vite, served by the transcript
  scheme handler — no CDN), preprocessed by `normalizeMath`
  (`web/src/mathDelimiters.ts`): `\(..\)`/`\[..\]` are rewritten to dollars in
  the raw source (CommonMark eats the backslashes before any remark plugin
  runs); a whole-line column-0 `$$..$$` is promoted to its own lines so it
  renders as a centered DISPLAY block (NOT via `\n\n` injection — that fractures
  a list/table/blockquote holding display math), while an embedded `$$` stays
  inline; and `guardDollars` (a linear pandoc-style pairing scanner) escapes any
  `$` that is not part of a valid inline math span — a `$...$` is math only if a
  valid CLOSER exists (not after whitespace, not before a digit), which is the
  "Balanced" `$` policy: `$x$`, `$3.14$`, `$2 + 2 = 4$` render, but prose money
  "$5 and $3" / "$12.50" stays literal (a local `$`+digit heuristic was tried and
  rejected — it destroyed decimal/arithmetic math). Code is masked by a linear
  line scanner (no ReDoS; any-indent fences so a list-nested fence is caught; an
  unclosed fence — the mid-stream state — masks to end-of-input). Display math
  centers when it fits and left-scrolls when it doesn't
  (`.katex-display > .katex { width: fit-content; margin-inline: auto }`, not
  `text-align: center` — a centered wide equation clips its own unreachable left
  edge under `overflow-x`), and `.md { overflow-x: clip }` keeps a wide inline
  expression from scrolling the whole transcript sideways.
- **Headless UI verification**: `-baybo-open-home` (DEBUG) lands on the tabbed
  home shell WITHOUT pushing a conversation (seeds a few demo list rows), so
  the menu bar / header / sections screenshot headlessly; add `-baybo-home-tab
  <agents|projects|chats|settings>` to preselect a section. `-baybo-demo-pin`
  (with `-baybo-open-home`) seeds nothing pinned, then pins the bottom row
  (demo-1) ~2s in so the reorder is recordable in isolation (`simctl io
  recordVideo` + ffmpeg montage of the transition window). The reorder is not
  animated — the row snaps — and this harness bypasses the swipe gesture, so it
  cannot reproduce anything about how a pin FEELS; drive
  `.swipeActions` from XCUITest for that. `-baybo-demo-tabs`
  cycles the tab selection on a timer so the native Liquid Glass tab morph is
  recordable (`simctl io recordVideo` + ffmpeg montage; the glass morph needs
  a 26+ sim — an 18.x sim records the classic bar). Launch with
  `-baybo-open-chat -baybo-demo-frames` (DEBUG) to feed one canned turn
  (thinking → tool → streamed markdown → finalize) through the real bridge —
  screenshot the sim at ~3s/~6s/~12s. `-baybo-open-chat -baybo-demo-attachments`
  pushes a short agent turn carrying three FILE attachments (long name / nameless
  blob / sub-KB) plus an audio card and a video tile, plus a user send carrying
  one file, so the attachment styling is screenshot-verifiable at ~4s on BOTH
  sides with no gateway. Add
  `-baybo-demo-download` to it and native pushes the `fileState` messages a real
  download would, walking the first file card AND the video tile idle → loading
  (file: ring + byte counter; video: centered determinate ring + corner byte
  chip) → ready over ~6s (shoot at ~4s / ~5.5s / ~10s); it drives the exact web
  reducer the native path drives, only the bytes are fake. The video's `ready`
  makes its card request a poster, served locally (flat 1280×720 PNG + fake
  1:23 duration, ~600ms later) — so the downloaded tile (poster + play disc +
  duration chip) screenshots headlessly too; real playback and real poster
  generation still need real blobs (a live session). Once the poster paints,
  the tile's ink border goes transparent (`.attachment-video.has-poster`) —
  the frame is the edge, matching the image idiom; `.failed`'s err border
  still wins over it. A file chip renders straight
  from the frame; the `image` kind needs bytes, so `-baybo-demo-images` (DEBUG)
  serves its own: one agent turn carrying four images of deliberately different
  aspect ratios (portrait / banner / thumbnail / square) plus a text row UNDER
  them, and `ChatStore.requestBlob` short-circuits the demo blob ids to a locally
  rendered PNG at the declared size (2s delay, so the pre-decode frame is
  screenshot-able). That text row's y-position is the test: run once (tiles →
  release → it moves), relaunch (sizes restored → nothing moves). **The second
  run only works on an UNBOUND simulator** — a bound one's list merge keeps only
  remote rows, so the demo's local-only session is dropped from the registry, and
  a mirror now dies with its row (that merge is how a conversation deleted on
  another client loses its cache), so it is gone before the relaunch can restore
  it. On a bound sim use `-baybo-open-session <id>` (DEBUG) to open a REAL session
  with images and compare `document.scrollHeight` at mount vs after the decode —
  they must be equal.
  `-baybo-demo-jump` scrolls the log off the newest edge at
  4s (native glass jump button pops) and runs the native jump path at 7s.
  `-baybo-demo-keyboard` raises the keyboard 2s in and drops
  it at 5s (record with `simctl io recordVideo`, extract frames with ffmpeg);
  the software keyboard only appears with Simulator.app running and hardware
  keyboard disconnected. `-baybo-demo-approval` (DEBUG, with `-baybo-open-chat`) runs a turn that blocks
  on the approval gate — two parallel `Bash` calls, so the card's queue counter
  renders — through the REAL frame path (`pushFrame`), so the native observer and
  the web reducer under test are the production ones; only the leg is faked
  (`resolveDemoApprovalIfRequested` answers in-process by pushing exactly the
  frames the gateway would, since there is no binding to call
  `chatResolveApproval` on). Screenshot at ~4s for the card + both steps'
  "waiting for approval", then tap Approve/Deny for the verdict labels.
  `-baybo-demo-models` (DEBUG, with `-baybo-open-chat`) seeds a canned model
  catalog into `ModelCatalog` — gpt-5.5 deliberately via TWO provider entries,
  efforts both set and unset — so the header's model pill + the three-level
  menu render headlessly (no gateway to `GET /v1/llm/models` from); picking a
  provider exercises the draft stash path natively (a level pick's effort PUT
  fails without a gateway — notice + catalog revert, which is itself the
  failure UI), and `UITests/ModelPickerUITests.swift` drives the picks through
  the pill's accessibility VALUE (the label is the constant "Model").
  `-baybo-demo-switch` (DEBUG) opens session `demo-a`
  with a session-tagged turn, then switches `chatPath` to `demo-b` at 5s —
  exercising the single reused webview's cross-session remount so a content
  leak is screenshot-verifiable (each thread must show ONLY its own tag; the
  `demo-b` screenshot showing any `demo-a` text is a cross-session bleed). NOTE:
  the demo ids are fixed, so the persisted transcript mirror ACCUMULATES across
  runs — `simctl uninstall com.baybo.app` (wipes the data container) before a
  clean single-turn check. `scripts/build-app.sh` pins products at
  `build/DerivedData/Build/Products/<config>-<sdk>/Baybo.app` for
  `simctl install`.
- **Send path**: native mints the msgId, seeds the webview's optimistic bubble
  + echo-dedup FIRST, enrols the persisted outbox entry, then enqueues on the
  leg (see "Send outbox" below).
- **Model picker** (`ChatHeaderView` + `App/Screens/ModelMenuPanel.swift` +
  `ChatStore.modelPin` + `App/Core/ModelCatalog.swift`): the chat header
  holds the model capsule on the LEFT (beside the glass back circle; mono(15)
  label = the effective entry's MODEL id, string-ellipsized — never cap a
  `Menu`/pill label with `.frame(maxWidth:)`, it renders greedy at the cap).
  Connectivity shows NOTHING in every healthy state; only a SUSTAINED outage
  surfaces, as a red `wifi.slash` in a glass circle on the trailing edge,
  keyed off `ChatStore.legDown` — a DEBOUNCED signal (default 4s away from
  `.connected`, cleared the moment a dial lands), because raw `connState`
  oscillates `connecting ↔ offline` through the retry loop on a real outage
  (`offline` is only the gap between failed dials, and a dead network's dial
  hangs in `connecting`), so an `offline`-gated indicator almost never
  showed. The catalog also keeps a **`models.json` mirror** (the `deck.json`
  idiom, written on fetch + effort edits, deleted on logout/rebind), so a
  cold offline start still paints the pill and panel; the pill NEVER shows a
  placeholder — always the best-known model id. The pill toggles
  `ModelMenuPanel`, a HAND-ROLLED glass panel `ChatScreen` overlays
  (`modelMenuOpen`) — not a system `Menu`, which cannot do any of: trailing
  checkmarks (selection and label icons render LEADING on iOS 26), removing
  a submenu row's `›`, or a live subtitle. Width knob:
  `ModelMenuPanel.panelWidth` (232). Three levels replace in place,
  sub-levels headed by a back row: **entries by name** (trailing ✓ on the
  effective entry, no chevron) → that entry's models (`model` +
  `model_candidates`, ✓ on the effective model) + (below a hairline) the
  Thinking row, subtitled with the entry's current level and carrying the
  panel's one trailing `›` → the levels
  (`none/minimal/low/medium/high/xhigh`, `crates/llm` registry contract).
  Panel rows set `accessibilityLabel` = title and `accessibilityValue` =
  subtitle, so the by-label UI smokes keep working and VoiceOver reads
  "Thinking, Ultra high".
  The catalog (`GET /v1/llm/models`, FFI `llm_list_models`, narrowed to
  name/provider/model/**model_candidates**/reasoning_effort) is global and
  cached per app run in `ModelCatalog.shared` + a `models.json` mirror (offline
  cold-paint; reset on logout/rebind); the pill renders only once it has
  entries. **The pin is a `(entry, model)` PAIR** (`ChatStore.modelPin` +
  `modelPinModel`): the entry is `SessionState.last_llm`, the model a
  `model_candidates` id in `last_model` (`nil` ⇒ entry default). It is re-read
  on EVERY chat open off the session detail with `limit=1` (FFI
  `chat_session_model` → `SessionModelPin{llm,model}`) — never latched
  once-per-store: the store stays cached resident and no frame broadcasts a
  re-pin made on another client, so the open edge is the only sync point.
  Writes go over `PUT /v1/chat/sessions/{id}/model` (FFI
  `chat_set_session_model(llm, model)`, both explicit-nullable —
  `{"llm":null,"model":null}` = follow `default-llm` + default model),
  SERIALIZED through `enqueueModelPut` (overlapping PUTs have no wire order),
  and optimistic: a failed PUT reverts to the last GATEWAY-ACKNOWLEDGED pair
  (`confirmedModelPin`) — never the previous display value, which would
  resurrect a pick the gateway refused; a seed read never applies over a
  bumped epoch or an in-flight PUT. A pick on a DRAFT stashes in
  `pendingModelPin` (epoch-stamped; a newer pick supersedes it) and is applied
  INSIDE `ensureRemoteSession`'s coalesced task, between session creation and
  every awaiter's send, so the first turn already runs on the choice; that
  failure path defers its notice (`draftPinFailed`, surfaced after the send —
  including the recovery path — because the dial clears `notice` as it
  starts). **The pin is a `(entry, model, effort)` TRIPLE**
  (`modelPin`/`modelPinModel`/`modelPinEffort`) — thinking level is
  PER-SESSION, not a global entry edit. `ChatStore.selectEffort` pins
  (entry, its relevant model, effort); `selectModel` keeps the current
  effort; both funnel through `applySelection` → one PUT. The panel's
  Thinking checkmark/subtitle read `store.modelPinEffort ?? entry's default`
  (session value wins, applies whatever entry — effort is INDEPENDENT of the
  llm/model pin). Pin + level apply from the session's NEXT turn — no
  confirmation UI. **Server-side seam** (root workspace, NOT on `app/ios`
  CI): `LlmEntry.model_candidates`/`lite_model` (config); the LLM pool
  pre-builds a client per candidate model, `resolve(name, model)` picks it;
  effort is a PER-REQUEST override on `ChatRequest.reasoning_effort` that
  only `openai-subscription` consumes (`AnyCompletionModel::stream(request,
  effort)`; other providers ignore it — no `additional_params` pollution);
  `last_model` + `last_effort` are their own flat SQLite columns
  (`set_last_model`/`set_last_effort`, additive migrations — keep
  `last_llm`'s golden JSON); `AgentMessage::SetModel{llm, model, effort}`
  threads the triple through the spawner (`ActorSpawner`'s `initial_effort`)
  → `AgentLoop.initial_effort` → every turn's `ChatRequest`;
  `validate_llm_model` + the `LLM_EFFORT_LEVELS` check reject a model outside
  the entry's `entry_model_ids` / an unknown effort. `PUT
  /v1/chat/sessions/{id}/model` carries `{llm, model, reasoning_effort}`; the
  old global `llm_set_reasoning_effort` FFI was removed (superseded).
- **Tool approvals** (`App/Core/ChatApprovals.swift` + `ChatStore.approvalObserveFrame`
  / `Screens/ApprovalCardView.swift`): a tool call whose declared resources
  aren't already granted blocks on the gateway's approval gate, which fans out
  `approval_requested` and **denies itself after 5 minutes** if nobody answers.
  The card is **NATIVE**, mounted inside the composer dock above the pill (so it
  rides the keyboard and inflates the web bottom inset), and the pending set is
  derived **natively in `pushFrame`** — NOT mirrored from the webview: frames
  buffered offscreen can overflow and be dropped, and the sync loop restores
  rows but not pending prompts, so a web-held queue could lose the only way to
  answer a gate that is about to deny. Four inputs, one per way a prompt
  appears or goes away: `approval_requested` (deduped — the gate's waker
  re-fires on the newest queue entry), `approval_resolved` (broadcast to EVERY
  session's sink, so it is matched by prompt id, never by session),
  `subscribe_state.pending_approvals` (the authoritative set, REPLACES the
  queue), and `tool_completed` (a **timed-out** gate broadcasts NO resolution —
  the completion is the only signal that retires the card). Answering dismisses
  optimistically and echoes `Frame::ResolveApproval` over the new FFI
  `chat_resolve_approval` (leg-generic; both legs share the outbound pump); a
  leg that can't carry it raises a notice, because the decision is then lost and
  the gate will deny on its own.
  **Two ids, don't confuse them**: a prompt's `call_id` is minted per prompt
  (one call can prompt more than once via the mid-call `ApprovalHandle`) and is
  what a resolve answers with; `tool_call_id` is the BLOCKED TOOL CALL — the id
  `tool_started`/`tool_completed` carry — and is what the work block badges.
  **The card offers exactly two answers — approve and deny.** The gate also
  accepts an `approve_always` (a standing, session-wide grant covering every
  resource the call touches) and the web chat / TUI both offer it, but the
  phone deliberately does not: a mis-tap is likeliest here, and a standing
  grant is the one decision the user can't walk back by paying more attention
  next time. The FFI enum (`api::ApprovalDecision`) omits the variant outright,
  so the app cannot send it even by accident — but a verdict given on ANOTHER
  client still arrives and renders (see the label below). Don't "restore" the
  third button without re-deciding this.
  The transcript shows the process, never the prompt: the blocked step reads
  "waiting for approval" (glyph BREATHES rather than pulses — nothing is
  executing), and after the decision it carries a permanent
  approved / always-approved / denied label. That label is **durable**:
  `ToolResultMeta::approval` persists it on the tool result, so a reload
  re-labels the same step (`ChatWorkStep.approval` on REST rows,
  `WireWorkStep.approval` in the `subscribe_state` snapshot). A `deny` also
  still reads red via the existing `denied` tool status.
- **Liquid Glass (iOS 26+; deployment target is 18.0)**: every CUSTOM glass
  surface goes through `Theme.swift`'s `glassSurface(tint:interactive:in:)` —
  the real `.glassEffect` on 26+, an `.ultraThinMaterial` fill in the same
  shape below (strokeless on purpose: the composer pill is borderless and the
  jump button is bare). Never call `.glassEffect` raw — it is 26-only and an
  unguarded call breaks the 18.0 target. The one other 26-only API,
  `interactiveContentPopGestureRecognizer`, is `#available`-guarded in
  `PopGesture.swift` (pre-26 the edge recognizer alone carries the feature).
  Building still REQUIRES Xcode 26 / the iOS 26 SDK at any deployment
  target — `#available` gates runtime, not compilation, and the guarded
  branches reference 26-SDK-only symbols (`Glass`, the content-pop
  recognizer).
  The bottom tab bar is the NATIVE `TabView` Liquid
  Glass bar — its selection-capsule morph (the glass that slides + stretches
  between tabs) is the SYSTEM's, and getting that authentic morph is exactly why
  the custom bar was dropped. Kept monochrome via `.tint(Theme.ink)` (ink
  selected item, neutral system-glass capsule, no accent hue); tab icons are
  thin line SF Symbols (`waveform.path.ecg`/`square.stack.3d.up`/`message`/
  `gearshape`). The remaining CUSTOM glass surfaces are the chat composer
  dock, the jump-to-latest button, and the Chats header's compose circle
  (`square.and.pencil`) — a recorded deviation from the flat-monochrome system
  inherited from the retired Tauri app's guide
  (`git show 6141f57b^:app/mobile/CLAUDE.md`), which still governs everything
  else. History (see git log, don't re-tread): a custom glass pill bar was built
  first — `matchedGeometry`
  chip → then a `GlassEffectContainer`+`glassEffectID` morph (which cross-faded on
  far hops and threw a red chromatic fringe) → then a single sliding-`.position`
  lozenge with a drag gel-stretch; none matched the native selection stretch, so
  we went native `TabView` (the native bar can't host the separate compose action
  circle — HIG: tab bar is navigation, not actions — so compose moved to the
  Chats header top-right). The composer is ONE ChatGPT-style glass pill (inline plus
  picker on the left, in-field ink send circle on the right; at rest it holds
  a moderate width, and focus stretches it toward the screen edges — a small
  gutter stays — on the keyboard's beat). Constraints: white tint only, no
  `.interactive()` shimmer on the field, the pill is BORDERLESS — a soft ink
  shadow carries its boundary over the blank at-rest strip (no hairline);
  the jump button is bare glass (no stroke).
  The dock's paper veil (bottom mirror of
  the header veil) is load-bearing, not decoration: it hit-tests the dock
  rect (gutter taps must not scroll the webview) and masks the web-vs-native
  inset animation phase mismatch. Its fade spans the dock itself — alpha 0
  at the dock's top edge, peak only under the PILL'S BOTTOM edge — so
  scrolled content ghosts past the pill's flanks.
  The jump button is native: web posts `jumpVisible` on its `showJump` state,
  native taps call `jumpToLatest` back; the composer-top geometry is measured
  on the ComposerView alone so the button never inflates the web inset.

## Transcript sync (sync-protocol v2 — read BEFORE touching sync/lifecycle)

The old seven-cell hydration matrix is **retired**. Transcript loading and
forward recovery are now **one loop** (`docs/sync-protocol.md`), identical on
both legs (same web bundle, same `GatewayJsonClient` API surface):

```
on open / reconnect / gap / buffer-overflow re-attach / safety tick:
  page = sync(since = cursor)          # cursor null → newest-page baseline
  if page.rebased or cursor == null:
      REPLACE thread with page.rows    # keep the open work block + optimistic sends
  else:
      APPEND/merge page.rows           # dedup by row id / platform_msg_id
  cursor = max(cursor, page.next_cursor)   # frozen while rebase-dirty
```

- **The webview drives sync.** `Transcript.tsx`'s `runSync()` posts
  `{type:"sync", sinceOrdinal, limit}` over the bridge; native
  (`ChatStore.requestSync`) fetches `GET …/sync` over the active leg and pushes
  a synthesized `sync_page` frame back. Clock edges: mount (resident re-entry),
  the `connEpoch` bump (`handleConnEpoch` — reconnect), a `gap` frame, the
  offscreen-buffer-overflow re-attach (native `bridge.requestSync()`), and the
  3-minute safety tick. `syncInFlight` coalesces a burst to one pull.
- **The server replays NOTHING on Subscribe** — it answers with one
  `SubscribeState` bundle (turn/work state) and live frames. `Subscribe` lost
  its `since_ordinal` field; `Frame::Reset` / `WorkSnapshot` /
  `PendingApprovalsSnapshot` are gone.
- **The mirror stays a pure `{rows, cursor}` cache**, written atomically. It is
  never a source of truth — a mirror-less open just syncs a baseline. The
  cursor is `number | null` (`null` = no baseline, never a sentinel); it lives
  in the persisted mirror blob (`lastOrdinal`), advanced from the sync coverage
  watermark and live final-reply ordinals, frozen while **rebase-dirty**.
- **Draft vs listed.** A compose draft stays empty until its first send:
  `ChatStore.requestSync` skips the fetch until the session exists remotely
  (`listed || remoteSessionEnsured`). The webview no longer needs a `listed`
  flag — its loop runs the same on every open.
- **Backward paging (scroll-up)** is unchanged in role: `fetchHistory(before)`
  → `history_page` frame, full-fidelity rows (message/work/notice) keyed by
  their stable server `id`, no client-side filtering.

Re-verify after touching any of this: (a) logout → re-pair the same gateway →
open an old session; (b) open an old (unpinned, outside top-10) session → back
to list → re-enter; (c) kill the app → relaunch → open a session; (d) open A
mid-stream → back → open B → back → A shows A (not B), B's mirror is not
overwritten by A's late flush.

**The single reused webview (`TranscriptHost`) changes WHEN the transcript is
(re)mounted, never the sync loop.** Returning to the SAME session reuses the
LIVE React tree; opening a DIFFERENT session remounts
`<Transcript key={sessionId}>` from that session's `restoredState`, then its
mount effect runs one sync. A jetsam silently reloads → `ready` re-fires →
re-mounts and re-syncs. Cross-session safety rests on `bridge.ts` clearing
`buffer`/`blobPending`/`pendingPersist` on `init` and `persist` writes being
session-tagged.

## Transcript mirror retention (do NOT re-add a sweeper)

**The mirror is the entire cold-open story.** `ChatScreen.onAppear` calls
`retarget` BEFORE `connectIfNeeded`, `deliverInit` reads
`transcripts/<id>.json` straight off disk, and `<Transcript>` seeds its rows in
the `useState` initializer — so a cached conversation paints on the first commit
with **zero network**. Nothing in the paint path reads `connState`. If a chat
opens blank and fills in only once the leg connects, the mirror was **missing**,
not gated.

So: **a mirror is kept for every session this device has rendered, and no
capacity sweeper evicts it.** `save()` writes the registry and nothing else. A
mirror is removed only when its conversation genuinely goes away, and there are
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

**An empty thread with no cursor never writes a mirror at all** (the persist
effect in `Transcript.tsx` bails). The transcript mounts for every compose draft
— including the throwaway one prewarmed at launch — each under a fresh uuid that
never becomes a chat-list row, so persisting them minted a file per abandoned
draft that nothing could ever reach again (no row → no delete → and nothing
sweeps). Don't create the orphan rather than sweep for it later.

This is a scar, so don't re-cut it. `save()` used to end with
`TranscriptStore.prune(keeping: <top 10 by lastActive>)` and every ingredient of
that was hostile: `save()` runs on essentially everything (every list merge —
*including the one the pop back from a chat fires* — every activity ping, every
badge clear); `rows` after a merge is the gateway's WHOLE session list ranked by
the **server's** `lastActive`, so rows this device has never opened (and every
individual cron fire — the list's cron-group collapse is render-time only) spent
keep-slots on mirrors that did not exist; and reading a chat earns nothing
(`touch` deliberately does not bump `lastActive` — "ordering means message
activity, not visits" — and a merge would overwrite it anyway). Net effect: any
conversation outside the ten most recently **messaged** ones had the mirror it
wrote on exit deleted seconds later, and opened blank on **every** re-entry,
forever. An active `*/30` cron job burned all ten slots in about five hours.
`SessionIndexMirrorTests` pins this; the retention tests fail against the old
prune.

Mirrors are text (`{rows, cursor, imageDims}` — never blob bytes) and exist only
for sessions actually read here, each dying with its conversation, so the set on
disk tracks the conversations the user still has. Like the blob cache it only
grows, deliberately: bounding it further wants a stated retention policy, not a
surprise sweep.

**What no cache can fix**: a session this device has never rendered (started on
web/TUI, a cron fire, a push tap into a new session, the first open after a
re-pair) has no mirror by construction — its first rows MUST come off the wire.
That open shows the `.thread-loading` line (empty thread + sync in flight), whose
400ms CSS delay keeps it invisible on every open that resolves instantly — a
restored thread, and a compose draft, whose synthesized empty page lands well
inside the delay.

## Send outbox (sync-v2)

The one-shot "red dot, human retries" send is replaced by a **persisted outbox**
(`App/Core/OutboxStore.swift`, a JSON file per session under
`Application Support/baybo/outbox/`, wiped with the mirrors on logout). Entries
are keyed by `platform_msg_id` with a two-stage confirmation: the server's Echo
(ordinal-less user message, same key) proves transport (`sending` → `sent`,
observed in `ChatStore.outboxObserveFrame`); an ordinal-stamped row with the
same key (from a `sync_page`, scanned in `reconcileOutboxAfterSync` before the
frame reaches the webview) proves durability and releases the entry. No echo
within 10 s → one blind resend, capped at 3 transmissions, then `failed` + the
manual red-dot retry (`resetForManualRetry`). On the reconnect edge the sync
runs first (the reconciliation gate), then unconfirmed entries resend. A
**rebased** sync hides the floor, so each unconfirmed entry goes `unknown` and
resolves via the per-key point lookup (`chatLookupMessage`) — found → released,
absent → retry resumes.

## Known gaps / follow-ups

- ~~Native chrome uses SF Mono~~ — Space Mono is bundled
  (`App/Resources/Fonts`, OFL) and registered via `UIAppFonts`; `Theme.mono`
  serves it with a system-face fallback.
- The old Tauri webview's localStorage (active session id + transcript mirror)
  is deliberately NOT migrated (owner's call — data gets reconstructed by hand);
  first launch after upgrade starts a fresh session. Gateway history is intact.
- Voice input has no composer affordance anymore — the mic placeholder button
  was removed with the Liquid Glass restyle (the deleted Tauri app had one).
  Wiring real capture later means re-adding the button, not just filling in a
  handler.

## Verified on simulator (2026-07-03)

Landing screen smoke on iPhone 17 Pro / iOS 26.5: app launches, Rust core
initializes, Space Mono renders the wordmark/CTAs, zh-Hans localization
resolves, pill styles match the guide. Live chat/pairing flows need a gateway —
see the device checklist below.

## Manual verification checklist (device)

1. Upgrade continuity: install the Tauri build, pair, then install this app
   over it → `pairedDevice()` returns the same id, chat connects, NSE still
   decrypts lock-screen previews.
2. Pairing: scan → confirm code matches → pair; decline + gateway-side abort
   both dismiss cleanly.
3. Direct login incl. `invalid_token` rendering; push binding after foreground.
4. Chat: streaming, history paging at top, image send/receive, background >45s
   → foreground sync (no duplicates), a `gap`/reconnect sync run, and the
   outbox (send offline → red dot / auto-retry → reconnect resend confirms).
5. Keyboard: composer rides the keyboard, header never moves, transcript holds
   the newest edge through the resize.
