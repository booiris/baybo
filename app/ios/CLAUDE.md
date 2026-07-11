# Baybo iOS (SwiftUI generation)

The native successor of `app/mobile` (the Tauri shell): a **SwiftUI app** whose
screens, header, and composer are native — so the iOS keyboard never touches web
content — with **only the chat transcript** rendered in a WKWebView, and the
transport/crypto core kept in **Rust behind UniFFI**. For app behavior the root
`/CLAUDE.md` applies; the visual system follows `app/mobile/CLAUDE.md`
(monochrome soft line minimalism) until this app grows its own guide.

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
NotificationExtension/ — NSE Swift sources (copied from app/mobile/apple;
                        dedupe when app/mobile retires)
web/                  — the transcript-only Vite/React bundle
scripts/              — build-core.sh, build-app.sh, install.mjs
Generated/ Externals/ — build products (gitignored): BayboCore.swift + .xcframework
```

## Build

```bash
scripts/build-app.sh             # web → rust xcframework → xcodegen → sim build
scripts/build-app.sh --device --release
node scripts/install.mjs         # archive + export + devicectl install (USB)
cargo clippy --workspace --all-targets --all-features   # zero warnings
cargo test --workspace           # host tests (QR parser etc.)
(cd web && pnpm build)           # tsc --noEmit + vite build
```

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
`scripts/build-core.sh` (no flags).

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
  contract unchanged (see `app/mobile/apple/README.md` while it exists).
- APNs environment is passed from Swift (`ClientConfig.apnsEnv`, per build
  config) — never `cfg!(debug_assertions)` in Rust, which usually compiles in
  release even for debug apps.

## Architecture notes

- **Navigation**: the home shell (`AppStore.Route.home`) is `HomeTabView`, a
  NATIVE iOS 26 `TabView(selection: $homeTab)` (Liquid Glass tab bar) with four
  sections (Agents · Projects · Chats · Settings, `AppStore.HomeTab`). Only `chats`
  (`ChatListScreen`) and `settings` (`SettingsScreen` — language, version, log
  out) have real screens; `agents`/`projects` are `PlaceholderScreen`. An OUTER
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
  does the same, it just hides it better).
- **Chat list data**: `SessionIndex` (Application Support/baybo/sessions.json)
  is the device-local registry backing the list on BOTH legs. Both direct and
  relay merge `chat_list_sessions()` over it on appear/foreground/pull: direct
  uses REST `GET /v1/chat/sessions` with the stored Bearer plus
  `x-baybo-device-id`, while relay uses the Noise-protected API tunnel. Remote
  wins for existence (a row missing remotely was hidden elsewhere) unless the
  local row saw newer activity. Per-session transcript mirrors live in
  `Application Support/baybo/transcripts/<id>.json`
  (pruned to the ~10 most recent); the legacy single-session UserDefaults keys
  (`ChatDefaults`) are migrated once and retired.
- **Live list unread**: the gateway broadcasts a throttled `Frame::SessionActivity`
  (per-session ping, no content) to EVERY connection on the `device` channel —
  subscribed or not — when a session's turn completes (`SessionPulse`, now
  installed on `device` as well as `http`; TUI is excluded). The FFI transport
  special-cases that frame in `dispatch_inbound_frame`, routing it to a
  connection-global `SessionListSink` (set once via `set_session_list_sink`)
  instead of the per-session `FrameSink` — so a session the device never opened
  still updates the list. `SessionActivityHandler` → `SessionIndex.noteActivity`
  bumps `SessionRow.unread` (local-only, persisted; ignored for the foreground
  session and for unknown ids) and recency; `ChatScreen` enter/leave marks the
  foreground session and clears its badge. Relay warms the leg via
  `relay_preconnect`; direct via `direct_preconnect` (both best-effort on
  launch/foreground) so the pings arrive while parked on the list.
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
  `init/pushFrame/setConnEpoch/userSent/blobResult/fileState/setLanguage/setBottomInset/jumpToLatest/requestSync`;
  web→native
  `ready/sync/persist/fetchHistory/requestBlob/queryFileState/downloadFile/previewFile/viewImage/openUrl/copy/copyImage/log/jumpVisible/runState`.
  (`copy` is a user-bubble long-press: native writes `UIPasteboard` + fires a
  haptic, because a `file://` WKWebView rejects `navigator.clipboard` outside a
  live gesture. `copyImage` is the image-bubble long-press — same reason, but the
  bytes aren't in hand, so `ChatStore.copyImage` fetches the device-cached blob
  and writes it to `UIPasteboard` under its image UTI, i.e. text copies inline in
  the bridge while an image round-trips the store.) `runState` mirrors whether a
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
  **A long-press on the image COPIES it** (`copyImage`, mirroring
  the user bubble's long-press copy — shared `useLongPress` + the same squish/pill
  confirm): the trailing tap-click is swallowed (`suppressClick`) so a copy
  doesn't also open the viewer, and `.attachment-open` turns OFF
  `-webkit-touch-callout`/`user-select` so iOS's own image callout can't fire at
  ~500ms and fight the gesture.
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
  interrupted by more work settles into the block as a prose step. Markdown
  links post `openUrl` to native (system browser) — an in-webview navigation
  would replace the thread.
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
  recordable (`simctl io recordVideo` + ffmpeg montage). Launch with
  `-baybo-open-chat -baybo-demo-frames` (DEBUG) to feed one canned turn
  (thinking → tool → streamed markdown → finalize) through the real bridge —
  screenshot the sim at ~3s/~6s/~12s. `-baybo-open-chat -baybo-demo-attachments`
  pushes a short agent turn carrying three FILE attachments (long name / nameless
  blob / sub-KB) plus a user send carrying one, so the file-card styling is
  screenshot-verifiable at ~4s on BOTH sides with no gateway. Add
  `-baybo-demo-download` to it and native pushes the `fileState` messages a real
  download would, walking the first card idle → loading (ring + byte counter) →
  ready over ~6s (shoot at ~4s / ~5.5s / ~9s); it drives the exact web reducer
  the native path drives, only the bytes are fake. A file chip renders
  straight from the frame, while the `image` kind would need a live blob leg.
  `-baybo-demo-jump` scrolls the log off the newest edge at
  4s (native glass jump button pops) and runs the native jump path at 7s.
  `-baybo-demo-keyboard` raises the keyboard 2s in and drops
  it at 5s (record with `simctl io recordVideo`, extract frames with ffmpeg);
  the software keyboard only appears with Simulator.app running and hardware
  keyboard disconnected. `-baybo-demo-switch` (DEBUG) opens session `demo-a`
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
- **Liquid Glass (iOS 26)**: the bottom tab bar is the NATIVE `TabView` Liquid
  Glass bar — its selection-capsule morph (the glass that slides + stretches
  between tabs) is the SYSTEM's, and getting that authentic morph is exactly why
  the custom bar was dropped. Kept monochrome via `.tint(Theme.ink)` (ink
  selected item, neutral system-glass capsule, no accent hue); tab icons are
  thin line SF Symbols (`sparkles`/`square.stack.3d.up`/`message`/
  `gearshape`). The remaining CUSTOM glass surfaces are the chat composer
  dock, the jump-to-latest button, and the Chats header's compose circle
  (`square.and.pencil`) — a recorded deviation from the `app/mobile/CLAUDE.md`
  flat-monochrome system, which still governs everything else. History (see git
  log, don't re-tread): a custom glass pill bar was built first — `matchedGeometry`
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

## Transcript hydration (sync-protocol v2 — read BEFORE touching hydration/lifecycle)

The old seven-cell hydration matrix is **retired**. Transcript hydration and
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
- `verify-nse.sh` still lives in app/mobile and targets the Tauri project;
  port it when app/mobile retires.
- Voice input has no composer affordance anymore — the mic placeholder button
  was removed with the Liquid Glass restyle (the Tauri app still shows one).
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
