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
scripts/              — build-xcframework.sh, build.sh, install.mjs
Generated/ Externals/ — build products (gitignored): BayboCore.swift + .xcframework
```

## Build

```bash
scripts/build.sh                 # web → rust xcframework → xcodegen → sim build
scripts/build.sh --device --release
node scripts/install.mjs         # archive + export + devicectl install (USB)
cargo clippy --workspace --all-targets --all-features   # zero warnings
cargo test --workspace           # host tests (QR parser etc.)
(cd web && pnpm build)           # tsc --noEmit + vite build
```

The Rust core is built OUTSIDE Xcode (no shell build phase): `build.sh` runs
`build-xcframework.sh` (cargo per-target + uniffi-bindgen + create-xcframework)
before `xcodegen generate`, so the project always references fresh products.
`generate_context!`-style staleness does not exist here, but the ORDER still
matters: web bundle → `App/Resources/transcript/` → xcodegen → xcodebuild.

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

- **BayboClient** (ffi) is a long-lived singleton (`Baybo.client`); the chat
  pump and parked pairing sessions live inside it between calls. Frames cross
  the FFI as JSON on a `FrameSink` callback; `onDisconnected` fires ONLY on
  unsolicited pump death (deliberate teardown aborts first) — the reconnect
  state machine in `ChatStore` depends on that contract.
- **Frame ordering**: sink callbacks hop to the main queue via GCD (FIFO), not
  `Task` — reordered `answer_delta`s would corrupt the transcript.
- **connState** has exactly one `offline` trigger: a failed dial. Unsolicited
  drops go back to `connecting` + 2s backoff; foreground reconnects debounce
  400ms; the core coalesces concurrent dials.
- **Bridge** (`App/Web/TranscriptBridge.swift` ⇄ `web/src/bridge.ts`):
  native→web `init/pushFrame/setConnEpoch/userSent/imageResult/setLanguage`;
  web→native `ready/ordinal/persist/fetchHistory/requestImage/openUrl/log`.
  Transcript persistence lives in UserDefaults (`ChatDefaults.*`), NOT webview
  localStorage (file:// storage is unreliable and upgrade-fragile).
- **Transcript rendering** (web-chat parity, mobile-restyled): user messages
  keep the black bubble; assistant replies are bubble-less full-width
  react-markdown + remark-gfm prose, rendered live WHILE streaming
  (rAF-coalesced; the web app only applies markdown on finalize). `reasoning`
  / `tool_started` / `tool_completed` / transient-notice frames fold into a
  per-turn collapsible work block ("思考中" card → "思考了 Xs ›"); answer text
  interrupted by more work settles into the block as a prose step. Markdown
  links post `openUrl` to native (system browser) — an in-webview navigation
  would replace the thread.
- **Headless UI verification**: launch with `-baybo-open-chat
  -baybo-demo-frames` (DEBUG) to feed one canned turn (thinking → tool →
  streamed markdown → finalize) through the real bridge — screenshot the sim
  at ~3s/~6s/~12s. `scripts/build.sh` pins products at
  `build/DerivedData/Build/Products/<config>-<sdk>/Baybo.app` for
  `simctl install`.
- **Send path**: native mints the msgId, seeds the webview's optimistic bubble
  + echo-dedup FIRST, then enqueues on the leg.

## Known gaps / follow-ups

- ~~Native chrome uses SF Mono~~ — Space Mono is bundled
  (`App/Resources/Fonts`, OFL) and registered via `UIAppFonts`; `Theme.mono`
  serves it with a system-face fallback.
- The old Tauri webview's localStorage (active session id + transcript mirror)
  is deliberately NOT migrated (owner's call — data gets reconstructed by hand);
  first launch after upgrade starts a fresh session. Gateway history is intact.
- `verify-nse.sh` still lives in app/mobile and targets the Tauri project;
  port it when app/mobile retires.
- Voice input is a placeholder (as in the Tauri app).

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
