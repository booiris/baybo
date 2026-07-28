# Testing the iOS app

*The four test tiers of `app/ios` (Rust core, transcript bundle, `BayboTests`, `BayboUITests`), the launch contract every UI smoke must use, the `-baybo-*` headless demo flags, the CI jobs that gate them, and the manual device checklist.*

## The four tiers

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

### `app/ios/ffi/`

Inline `#[cfg(test)] mod tests`. The load-bearing ones pin things no other check
can see:

- the `PairedRecord` / `DirectCredentials` **golden JSON** — the on-keychain byte
  format is a frozen upgrade contract, and breaking it means every upgraded
  install silently loses its gateway binding;
- `dispatch_inbound_frame`'s four routing invariants;
- the `invalid_token` untyped-string chain;
- `since_ordinal` serializing as an **explicit null** (add `skip_serializing_if`
  and a baseline REPLACE quietly becomes an APPEND).

### `app/ios/web/`

vitest + jsdom, mostly over the pure reducers. Mounting `<Transcript>` was tried
and rejected: jsdom reports `scrollHeight` as 0, so every follow/pin/jump branch
degenerates — the transcript is tested through its extracted reducers, and the
simulator demo flags (see [Headless UI verification](#headless-ui-verification))
cover what a reducer test cannot.

A **small presentational component with no scrollHeight/follow/pin dependency is
fine to render**, though: `app/ios/web/src/WorkBlock.test.tsx` mounts
`WorkBlockView` (React Testing Library) for its active/closed/toggle wiring — do
NOT extend this to `<Transcript>`.

`pnpm lint` (eslint, mirroring `app/web` — strict-boolean-expressions /
no-unnecessary-condition / react-hooks, with a suppression baseline for the
existing backlog) gates the wiring-bug class over `src`.

### `app/ios/Tests/` (`BayboTests`)

Swift Testing (`@Test`/`#expect`), a **host-application** bundle (hostless would
relink the Rust staticlib and make `Lang.t` return bare keys). `ChatStore` takes
an injected `any BayboClientProtocol` — the protocol UniFFI already generates —
so the frame paths, the approval queue, and the outbox's two-stage confirmation
are testable with no gateway. XCTest stays for `BayboUITests` (Swift Testing has
no UI API); both bundles coexist.

`ComposerStaging` takes the same injected client, which is what makes the
composer's *lifetimes* testable below the UI: `FakeBayboClient.holdBlobUploads()`
parks an upload on the wire (deaf to cancellation, exactly like the real UniFFI
binding) so a test can remove the tile underneath it and assert what the strip,
the notice line and the temp spool do next.

### `app/ios/UITests/` (`BayboUITests`)

XCUITest over the `-baybo-*` fixture flags. Non-gating in CI (gesture/timing
driven). Reserve it for what is *unreachable* below the seam: SwiftUI
hit-testing (the stroke-only pill whose flanks were dead), UIKit swipe
thresholds, presentation-binding lifecycles, the native↔web bridge round-trip.
Everything else is 100x cheaper as a reducer test.

## `BayboUITestCase` and the launch contract

**`BayboUITestCase` is the base class and every smoke must use its `launch(_:)`.**
It pins three things a test cannot get right by accident:

1. `-baybo-reset-store` (below),
2. `-baybo.lang en` (our strings), and
3. `-AppleLanguages (en)` — the *system* chrome (AVKit, the share sheet) follows
   the simulator's locale, not our UserDefaults key; without it a test matching
   system chrome by label passes locally and fails on a runner.

## `-baybo-reset-store`

**`-baybo-reset-store`** (DEBUG, `AppDelegate`) wipes `Application
Support/baybo` before anything reads it.

The demo fixtures use FIXED session ids, so without it each launch APPENDS its
canned turn to the same persisted mirror — and one simulator is shared across a
suite, so the attachment demo reached six video tiles and a by-label query that
is unambiguous on a fresh install started matching six elements.

Demo flags are only hermetic with it.

## CI

CI (`.github/workflows/ci.yml`) — **both jobs are currently `if: false`, so
nothing under `app/ios` is covered.** Run the four tiers above by hand and say
so in the PR body; a green check means only that the rest of the workspace
passed.

- `ios-checks` (ubuntu) would run the Rust and web tiers;
- `ios-sim` (macos-26, build + unit tests, non-gating UI smokes) would run the
  Swift half.

Both are path-filtered — and the filter deliberately includes
`crates/{wire,device-proto,model}` and `remote-host/`, because the ffi workspace
path-depends on them and a root-crate change can break the iOS build without
touching `app/ios` at all.

## Headless UI verification

- **`-baybo-open-home`** (DEBUG) lands on the tabbed home shell WITHOUT pushing a
  conversation (seeds a few demo list rows), so the menu bar / header / sections
  screenshot headlessly; add **`-baybo-home-tab <agents|projects|chats|settings>`**
  to preselect a section.

- **`-baybo-demo-pin`** (with `-baybo-open-home`) seeds nothing pinned, then pins
  the bottom row (demo-1) ~2s in so the reorder is recordable in isolation
  (`simctl io recordVideo` + ffmpeg montage of the transition window). The
  reorder is not animated — the row snaps — and this harness bypasses the swipe
  gesture, so it cannot reproduce anything about how a pin FEELS; drive
  `.swipeActions` from XCUITest for that.

- **`-baybo-demo-tabs`** cycles the tab selection on a timer so the native Liquid
  Glass tab morph is recordable (`simctl io recordVideo` + ffmpeg montage; the
  glass morph needs a 26+ sim — an 18.x sim records the classic bar).

- **`-baybo-open-chat -baybo-demo-frames`** (DEBUG) feeds one canned turn
  (thinking → tool → streamed markdown → finalize) through the real bridge —
  screenshot the sim at ~3s/~6s/~12s.

- **`-baybo-open-chat -baybo-demo-attachments`** pushes a short agent turn
  carrying three FILE attachments (long name / nameless blob / sub-KB) plus an
  audio card and a video tile, plus a user send carrying one file, so the
  attachment styling is screenshot-verifiable at ~4s on BOTH sides with no
  gateway.

- **`-baybo-demo-download`** — add it to `-baybo-demo-attachments` and native
  pushes the `fileState` messages a real download would, walking the first file
  card AND the video tile idle → loading (file: ring + byte counter; video:
  centered determinate ring + corner byte chip) → ready over ~6s (shoot at ~4s /
  ~5.5s / ~10s); it drives the exact web reducer the native path drives, only the
  bytes are fake.

  The video's `ready` makes its card request a poster, served locally (flat
  1280×720 PNG + fake 1:23 duration, ~600ms later) — so the downloaded tile
  (poster + play disc + duration chip) screenshots headlessly too; real playback
  and real poster generation still need real blobs (a live session). Once the
  poster paints, the tile's ink border goes transparent
  (`.attachment-video.has-poster`) — the frame is the edge, matching the image
  idiom; `.failed`'s err border still wins over it.

- **`-baybo-demo-images`** (DEBUG) — a file chip renders straight from the frame;
  the `image` kind needs bytes, so this flag serves its own: one agent turn
  carrying four images of deliberately different aspect ratios (portrait / banner
  / thumbnail / square) plus a text row UNDER them, and `ChatStore.requestBlob`
  short-circuits the demo blob ids to a locally rendered PNG at the declared size
  (2s delay, so the pre-decode frame is screenshot-able). That text row's
  y-position is the test: run once (tiles → release → it moves), relaunch (sizes
  restored → nothing moves).

  **The second run only works on an UNBOUND simulator** — a bound one's list
  merge keeps only remote rows, so the demo's local-only session is dropped from
  the registry, and a mirror now dies with its row (that merge is how a
  conversation deleted on another client loses its cache), so it is gone before
  the relaunch can restore it. On a bound sim use **`-baybo-open-session <id>`**
  (DEBUG) to open a REAL session with images and compare `document.scrollHeight`
  at mount vs after the decode — they must be equal.

- **`-baybo-demo-jump`** scrolls the log off the newest edge at 4s (native glass
  jump button pops) and runs the native jump path at 7s. Pair it with a thread
  tall enough to scroll — `-baybo-demo-index` seeds one in 400ms, where
  `-baybo-demo-frames` is still streaming its single turn at 4s — and remember
  the disc is only up for those three seconds: `ComposerAttachUITests` opens the
  `+` panel inside that window, which is the only headless way to put the disc
  and the panel on screen together.

- **`-baybo-demo-index`** pushes ONE synthesized `sync_page` carrying six of the
  user's own sends (one attachment-only, one past the row's text cap) with their
  replies, split across yesterday and today — the header's message-index button,
  its sheet's day headers, the gloss line and the "load earlier" row all render
  headlessly. A `sync_page` rather than live `message` frames on purpose: the
  wire's `Frame::Message` has no time field, so the sheet's clock and day key
  only exist on the reconstructed-row path.

- **`-baybo-demo-compose`** (DEBUG, with `-baybo-open-chat`) seeds the composer's
  staged strip with one pick of each state — a ready image thumbnail, a ready
  file pill, one mid-upload (spinner + byte counter) and one failed (retry
  affordance). Staging for real is not a path a smoke can take: both the
  document picker and the photo picker run OUT OF PROCESS, so this flag is the
  strip's only headless entry point (`ComposerAttachUITests` drives it, and the
  `+` panel itself, which is in-process, by hand). It seeds in
  `ComposerStaging.init`, i.e. once per CONVERSATION — seeded per composer frame
  it refilled the strip after every `fullScreenCover`, which is exactly the
  teardown one of those smokes drives it through. Its fixture names are prefixed
  `staged-` so a by-label query can't confuse a strip tile with a transcript
  card when this flag runs beside `-baybo-demo-attachments`.

- **`-baybo-demo-keyboard`** raises the keyboard 2s in and drops it at 5s (record
  with `simctl io recordVideo`, extract frames with ffmpeg); the software
  keyboard only appears with Simulator.app running and hardware keyboard
  disconnected.

- **`-baybo-demo-approval`** (DEBUG, with `-baybo-open-chat`) runs a turn that
  blocks on the approval gate — two parallel `Bash` calls, so the card's queue
  counter renders — through the REAL frame path (`pushFrame`), so the native
  observer and the web reducer under test are the production ones; only the leg
  is faked (`resolveDemoApprovalIfRequested` answers in-process by pushing
  exactly the frames the gateway would, since there is no binding to call
  `chatResolveApproval` on). Screenshot at ~4s for the card + both steps'
  "waiting for approval", then tap Approve/Deny for the verdict labels.

- **`-baybo-demo-models`** (DEBUG, with `-baybo-open-chat`) seeds a canned model
  catalog into `ModelCatalog` — gpt-5.5 deliberately via TWO provider entries,
  efforts both set and unset — so the header's model pill + the three-level menu
  render headlessly (no gateway to `GET /v1/llm/models` from); picking a provider
  exercises the draft stash path natively (a level pick's effort PUT fails
  without a gateway — notice + catalog revert, which is itself the failure UI),
  and `app/ios/UITests/ModelPickerUITests.swift` drives the picks through the
  pill's accessibility VALUE (the label is the constant "Model").

- **`-baybo-demo-switch`** (DEBUG) opens session `demo-a` with a session-tagged
  turn, then switches `chatPath` to `demo-b` at 5s — exercising the single
  reused webview's cross-session remount so a content leak is
  screenshot-verifiable (each thread must show ONLY its own tag; the `demo-b`
  screenshot showing any `demo-a` text is a cross-session bleed). NOTE: the demo
  ids are fixed, so the persisted transcript mirror ACCUMULATES across runs —
  `simctl uninstall com.baybo.app` (wipes the data container) before a clean
  single-turn check.

`app/ios/scripts/build-app.sh` pins products at
`build/DerivedData/Build/Products/<config>-<sdk>/Baybo.app` for `simctl install`.

## Verified on simulator (2026-07-03)

Landing screen smoke on iPhone 17 Pro / iOS 26.5: app launches, Rust core
initializes, Space Mono renders the wordmark/CTAs, zh-Hans localization
resolves, pill styles match the guide. Live chat/pairing flows need a gateway —
see the device checklist below.

## Manual verification checklist (device)

1. Upgrade continuity: install a previously shipped **`app/ios`** build, pair,
   then install this build over it → `pairedDevice()` returns the same id, chat
   connects, NSE still decrypts lock-screen previews. (`app/ios` → `app/ios` is
   the only supported upgrade path, and the population the continuity contract
   exists for.)
2. Pairing: scan → confirm code matches → pair; decline + gateway-side abort
   both dismiss cleanly.
3. Direct login incl. `invalid_token` rendering; push binding after foreground.
4. Chat: streaming, history paging at top, image send/receive, background >45s
   → foreground sync (no duplicates), a `gap`/reconnect sync run, and the
   outbox (send offline → red dot / auto-retry → reconnect resend confirms).
5. Keyboard: composer rides the keyboard, header never moves, transcript holds
   the newest edge through the resize.
