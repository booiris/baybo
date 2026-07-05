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
  state machine in `ChatStore` depends on that contract. `AppStore` owns one
  cached `ChatStore` per opened session. The FFI transport owns one global chat
  leg per binding (relay content or direct channel WS); opening a session sends a
  `Subscribe` on that leg and registers/replaces that session's sink. Backing out
  to the list only detaches the `TranscriptBridge`; frames that arrive while no
  webview is attached buffer in the store and flush in order on the next attach.
  Switching sessions does not redial or disconnect the old subscription. Relay
  bindings also call `relay_preconnect()` on launch/foreground to warm the
  content leg before a chat is opened; it dials + handshakes but sends no
  `Subscribe`. Logout, rebind, or explicit app teardown calls `chat_disconnect`,
  which drops the global leg and all registered sinks.
- **Frame ordering**: sink callbacks hop to the main queue via GCD (FIFO), not
  `Task` — reordered `answer_delta`s would corrupt the transcript.
- **connState** has exactly one `offline` trigger: a failed dial. Unsolicited
  drops go back to `connecting` + 2s backoff; foreground reconnects debounce
  400ms; the core coalesces concurrent dials.
- **Bridge** (`App/Web/TranscriptBridge.swift` ⇄ `web/src/bridge.ts`):
  native→web
  `init/pushFrame/setConnEpoch/userSent/blobResult/setLanguage/setBottomInset/jumpToLatest`;
  web→native
  `ready/ordinal/persist/fetchHistory/requestBlob/openUrl/log/jumpVisible`.
  Transcript persistence lives in UserDefaults (`ChatDefaults.*`), NOT webview
  localStorage (file:// storage is unreliable and upgrade-fragile).
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
  <agents|projects|chats|settings>` to preselect a section. `-baybo-demo-tabs`
  cycles the tab selection on a timer so the native Liquid Glass tab morph is
  recordable (`simctl io recordVideo` + ffmpeg montage). Launch with
  `-baybo-open-chat -baybo-demo-frames` (DEBUG) to feed one canned turn
  (thinking → tool → streamed markdown → finalize) through the real bridge —
  screenshot the sim at ~3s/~6s/~12s. `-baybo-demo-jump` scrolls the log off the newest edge at
  4s (native glass jump button pops) and runs the native jump path at 7s.
  `-baybo-demo-keyboard` raises the keyboard 2s in and drops
  it at 5s (record with `simctl io recordVideo`, extract frames with ffmpeg);
  the software keyboard only appears with Simulator.app running and hardware
  keyboard disconnected. `scripts/build-app.sh` pins products at
  `build/DerivedData/Build/Products/<config>-<sdk>/Baybo.app` for
  `simctl install`.
- **Send path**: native mints the msgId, seeds the webview's optimistic bubble
  + echo-dedup FIRST, then enqueues on the leg.
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
4. Chat: streaming, history paging at top, reset recovery, image send/receive,
   background >45s → foreground catch-up (no duplicates).
5. Keyboard: composer rides the keyboard, header never moves, transcript holds
   the newest edge through the resize.
