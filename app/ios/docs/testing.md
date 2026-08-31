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
(cd web && pnpm build)    # tsc --noEmit -> enforces both drift sentinels

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

vitest + jsdom, mostly over the pure reducers. Bare jsdom cannot say anything
about the scroll model — `scrollHeight` is 0 and `getBoundingClientRect` is all
zeros, so every follow/pin/anchor branch degenerates to a no-op — so most of the
transcript is tested through its extracted reducers, with the simulator demo
flags (see [Headless UI verification](#headless-ui-verification)) covering what a
reducer test cannot.

`app/ios/web/src/transcriptScroll.test.tsx` is the one suite that mounts
`<Transcript>`, and it does so **under a fake layout**: it stubs
`document.scrollingElement`, the document's `scrollTop`/`scrollHeight`/
`clientHeight`, and a per-row `getBoundingClientRect` derived from the row's
index. That is enough to exercise the arithmetic — a prepend's
`prevScrollTop + (scrollHeight - prevScrollHeight)`, a REPLACE's
`top - anchor.top` — for real, which is what the whole scroll half of the
component is. It exists because that half had no test and shipped a bug: a
rebased `sync_page` discarded the history a reader had scrolled up for and
slammed them to the newest edge. It is **not** a WKWebView — momentum,
rubber-band overscroll and the UI-process scroll thread stay device-only.

A **small presentational component is fine to render** the plain way:
`app/ios/web/src/WorkBlock.test.tsx` mounts `WorkBlockView` (React Testing
Library) for its active/closed/toggle wiring, with no layout stubs at all.

`pnpm lint` (eslint, mirroring `app/web` — strict-boolean-expressions /
no-unnecessary-condition / react-hooks, with a suppression baseline for the
existing backlog) gates the wiring-bug class over `src`.

**Three compile-time drift sentinels live here**, all type-only, none producing
a byte of runtime output — which is why `pnpm build` (not `pnpm test`) is the
only thing that evaluates them:

| file | pins | against |
|---|---|---|
| `src/wireSentinel.ts` | the live frame mirrors | the ts-rs contract under `sidecars/` |
| `src/restSentinel.ts` | `TranscriptRowItem` | `app/web`'s generated `ChatTranscriptItem` |
| `src/issue/issueSentinel.ts` | the card page's DTO mirrors | `app/web`'s generated `IssueDto` / `IssueEventDto` / `IssueRunDto` / `ActorDto` |

Each reads `app/web`'s generated schema rather than generating a second copy:
both would come from the one committed `docs/openapi.json` through the same
generator and be byte-identical, and a second copy is a second thing to
regenerate — a new drift surface inside the gate built to close one. The import
is type-only under `noEmit`, so `tsc` follows the path and no bundler or pnpm
resolution is involved.

`issueSentinel.ts` is worth reading as a case for the pattern: the mirrors were
written by hand from the Rust source, and it caught three wrong guesses at once
(`assignee` as an object rather than a bare id, `IssueRun.agent` rather than
`agent_id`, and an externally-tagged `Actor`). Every one of them fails silently
at runtime as a missing `@handle`.

### `app/ios/Tests/` (`BayboTests`)

Swift Testing (`@Test`/`#expect`), a **host-application** bundle (hostless would
relink the Rust staticlib and make `Lang.t` return bare keys). `ChatStore` takes
an injected `any BayboClientProtocol` — the protocol UniFFI already generates —
so the frame paths, the approval queue, and the outbox's two-stage confirmation
are testable with no gateway. XCTest stays for `BayboUITests` (Swift Testing has
no UI API); both bundles coexist.

**`LocalizedKeyTests` is a gate, not a nicety.** `Lang.t` echoes the key on a
miss, so a screen ships with a button labelled `chat.cancel` and every existing
assertion stays green — an assertion on a label matches the key as happily as
the word. The suite walks every `lang.t("…")` literal under `App/` and fails on
one the catalog does not carry, and on a key one language has and the other does
not. It has caught two already: `chat.cancel` (never existed) and `issue.system`
(exists in the WEB locales and not in the Swift catalog).

**Golden fixtures shared across ends** are read by BOTH a vitest suite and a
Swift test: one file, two readers, so a rule with a copy per language cannot
drift silently. `searchSnippetVectors.json` is the live example.
`commentHintVectors.json` was the other and is **gone (2026-08-26)** along with
both copies of the rule it pinned — what sending a comment will do is decided
in Rust (`crates/project/src/comments.rs::comment_delivery`) and is not exposed
over REST, so a composer that wants to say it in advance must re-derive it; the
phone stopped drawing that sentence and the web only ever had it as a tooltip,
so neither client re-derives anything now. A client that wants it back needs
the port AND the fixture, in one commit.

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

### Typing into a SEEDED field: clear it yourself, never via the edit menu

`typeText` **appends at the caret**. A field the app pre-filled (the rename
editor seeds the current title) therefore needs an explicit clear, and reaching
for the long-press edit menu's *Select All* to do it is a trap: that menu is
system chrome whose appearance is timing-dependent — it showed for one case and
not its sibling in the same run — and when it does not appear the typing lands
appended to the seed. The test then drives a rename nobody asked for, and only an
exact-match assertion catches it (a `contains` or a "the dialog closed" check
goes green).

Send `XCUIKeyboardKey.delete` once per existing character instead
(`RenameMenuUITests.replaceText`), and do not tap the field first — a tap moves
the caret into the middle of the text, where backspaces eat the wrong half. The
dialog focuses the field itself and parks the caret at the end.

### A `.plain` button is only tappable where it PAINTS

Under `.buttonStyle(.plain)` the hit region is whatever the label actually
draws. A `Text` hit-tests its own box; a `.frame(minHeight:)` is layout and
adds nothing tappable; a stroke-only `Capsule` hit-tests a 1px outline. So a
row without a `contentShape` is dead wherever there is no ink — and it looks
completely healthy: `exists`, `isHittable` and the accessibility frame are all
satisfied by a control whose paint does not fill it.

`.tap()` will not find it either, because it lands dead centre — which on a
card row is usually right on the title. **Walk coordinates to test this**, and
measure the dead points rather than guessing them: a throwaway probe test that
taps a grid of `coordinate(withNormalizedOffset:)` points and logs which ones
open is ten minutes and tells you exactly which offsets a regression test has
to use. `testEveryPartOfTheRowOpensTheCardNotJustItsText` uses three offsets
found that way, and it fails without the fix — which the first version of it,
written from reasoning, did not.

The app has now shipped this bug three times: the logout pill
(`OutlinePillButtonStyle`), a board card row, and the board's budget chip.

### Assert that a thing GOES, not only that it appears

`ProjectsUITests` asserted all four Waiting-strip kinds appear and nothing
asserted any of them clears — which is how two of the four shipped answering
nothing on screen. A strip that only ever grows looks identical to a healthy
one at the moment the screenshot is taken.

The same rule found the demo's blind spot: under `-baybo-demo-projects` a
write's whole effect IS its `apply` closure (the network half is
short-circuited), so a verb with no `apply` is a verb that visibly does
nothing there. That makes the demo a decent detector for exactly this class —
but only if a test presses the button and then asserts the row is gone.

### A tap lands on the element's CENTRE, so an a11y frame is a test surface

`element.tap()` synthesises a touch at the middle of the element's
**accessibility frame** — not on the pixels you can see. When the two disagree
the button is dead to XCUITest and perfectly alive to a finger, which reads as
"navigation is broken" and is nothing of the sort.

It has already happened once. `ChatHeaderView`'s back chevron reported
`(0, 0, 402, 108)` — the whole bar, status bar included — because an offline
session renders neither the model pill nor the index button, leaving the chevron
as the bar's ONLY focusable child; SwiftUI collapsed the bar into it, and the
inherited frame was the one the veil's `ignoresSafeArea` had stretched. Every
back tap landed on empty header. `CronExitUITests` (both cases) and
`ArchiveFlowUITests.testDeepNavChainLeavesStackResponsive` failed for weeks with
no navigation code involved. The fix is one line — `.accessibilityElement(children:
.contain)` on the bar — and the general rule is: **a container whose visible
children come and go must be declared a container**, or the last child standing
inherits its geometry.

Diagnose it by printing `app.debugDescription` and reading the frames, then
confirm with a raw-coordinate tap (`app.coordinate(withNormalizedOffset: .zero)
.withOffset(...)`): if the coordinate tap works and `element.tap()` does not, the
frame is the bug.

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

CI (`.github/workflows/ci.yml`) defines three iOS jobs and **runs none of them**
— all three are `if: false` while the Actions quota is out. Every tier below is
run by hand; say so in the PR body, because no check anywhere will.

- `ios-web` (ubuntu, 1×) — **off, and the cheapest to restore.** `pnpm lint &&
  pnpm test && pnpm build` in `app/ios/web`. `pnpm build` is
  `tsc --noEmit && vite build`, and that typecheck is the only place the two
  compile-time drift sentinels are ever evaluated: `src/wireSentinel.ts` (frame
  mirrors ⇄ the ts-rs contract in `crates/wire`) and `src/restSentinel.ts`
  (`TranscriptRowItem` ⇄ the gateway's `ChatTranscriptItem`). Until it is back
  on, both sentinels only fire on a laptop.
- `ios-core` (ubuntu, 1×) — **`if: false`.** Would run `cargo fmt` / `clippy` /
  `nextest` over the ffi workspace. Off because it shares no cache with the root
  workspace and pays a cold ~286-crate build.
- `ios-sim` (macos-26, 10×) — **`if: false`.** Would run the build + unit tests
  and the non-gating UI smokes.

All three are path-filtered — and the filter deliberately includes
`crates/{wire,device-proto,model}`, `remote-host/` and `docs/openapi.json`,
because the ffi workspace path-depends on the first two and the transcript
bundle's REST sentinel is generated from the third, so a change to any of them
can break the iOS build (or silently drift the transcript's row contract)
without touching `app/ios` at all.

## Headless UI verification

- **`-baybo-open-home`** (DEBUG) lands on the tabbed home shell WITHOUT pushing a
  conversation (seeds a few demo list rows), so the menu bar / header / sections
  screenshot headlessly; add **`-baybo-home-tab <agents|projects|chats|settings>`**
  to preselect a section.

- **`-baybo-appstore-data`** — add it to `-baybo-open-home` for the denser,
  English-only App Store chat-list fixture. It keeps the ordinary six-row UI-test
  seed unchanged and expands only the release screenshot launch to ten realistic
  conversations derived from the device's existing subjects. On Projects it
  serves the copied, English-normalized device mirrors without a gateway refresh,
  opens the archived rows, and resolves Agent images from the temporary
  `Application Support/baybo/appstore-avatars` screenshot cache. That cache is
  simulator data only; no personal project or avatar asset enters the App bundle.
  Add **`-baybo-appstore-board`** to open the mirrored `rglide` board directly on
  Done, where a dense English task set and the full Agent roster fit one frame.

- **`-baybo-demo-pin`** (with `-baybo-open-home`) seeds nothing pinned, then pins
  the bottom row (demo-1) ~2s in so the reorder is recordable in isolation
  (`simctl io recordVideo` + ffmpeg montage of the transition window). The
  reorder is not animated — the row snaps — and this harness bypasses the swipe
  gesture, so it cannot reproduce anything about how a pin FEELS; drive
  `.swipeActions` from XCUITest for that.

- **`-baybo-demo-search`** (with `-baybo-open-home`) answers every search with
  canned hits over the demo rows, so the search surface is drivable with no
  gateway. Without it every state on that screen is the failure page.

- **`-baybo-demo-slow-morph`** stretches the search field's open from a 0.42s
  spring to a 4s linear ramp, so the expansion can be SAMPLED with plain
  screenshots instead of reconstructed from video frames. This is what verified
  the field grows LEFTWARD out of the tab bar's search circle rather than
  rightward from the leading edge — a distinction no end-state screenshot can
  make, and one the first implementation got backwards.

- **`-baybo-demo-tabs`** cycles the tab selection on a timer so the native Liquid
  Glass tab morph is recordable (`simctl io recordVideo` + ffmpeg montage; the
  glass morph needs a 26+ sim — an 18.x sim records the classic bar).

- **`-baybo-open-chat -baybo-demo-frames`** (DEBUG) feeds one canned turn
  (thinking → tool → streamed markdown → finalize) through the real bridge —
  screenshot the sim at ~3s/~6s/~12s.

- **The webview crash reload needs no flag at all** — a simulator's WebContent
  is a host macOS process. Launch a throwaway sim with
  `-baybo-open-chat -baybo-demo-frames`, find the app's
  `com.apple.WebKit.WebContent` instance (`pgrep -fl WebContent` filtered to
  the booted runtime path), `kill -9` it, and verify with `log stream`
  ("transcript web content process died; reloading" then "transcript bridge
  ready") plus `simctl io booted screenshot` before/after — sampled at the
  PIXEL level, because existence/hittability assertions are blind to paint.
  Kill it three times inside 30s to watch the crash-loop budget give up.

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
  carrying four rasters of deliberately different aspect ratios (portrait /
  banner / thumbnail / square) and two wide SVGs, plus a text row UNDER them, and
  `ChatStore.requestBlob` short-circuits the demo blob ids to a locally rendered
  PNG or SVG at the declared size (2s delay, so the pre-decode frame is
  screenshot-able). That text row's y-position is the test: run once (tiles →
  release → it moves), relaunch (sizes restored → nothing moves).

  The two vectors are both spellings of the same wide diagram because they fail
  in opposite directions, and only one of them is a size question at all: an SVG
  declaring `width`/`height` past the column reports the CLAMPED layout back as
  its natural size (192px, measured inside the loading tile) and comes back a
  third of the column on the next open, while a bare `viewBox` has no intrinsic
  width and lays out at ZERO until something reserves a box for it.
  `AttachmentImageUITests` drives both, plus the tap — `ChatStore.viewImage`
  reads its blob NATIVELY rather than over the bridge, so `demoImageBytes` backs
  that path too, and without it the image viewer is the one attachment surface no
  fixture can reach.

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

- **`-baybo-demo-paste`** (DEBUG, with `-baybo-open-chat`) swaps the composer's
  clipboard for `DemoPasteboard` — one PNG, statelessly served, so every read
  yields it again. It exists because the Paste row is *conditional*: it appears
  only when `UIPasteboard.general` holds an image, and a UI test cannot seed the
  real board. Unlike `-baybo-demo-compose`, which fakes the RESULT, this one
  fakes only the clipboard and leaves the whole staging path real — so it is the
  one end-to-end staging witness the UI tier has (both pickers being out of
  process), covering the row's paint, the taller 3-row panel, and the fact that
  the row is not one-shot. It also drives the OTHER entry point,
  `testLongPressPasteReachesTheResponderChain` — the only case that can see
  UIKit walk past the text field to `AppDelegate`, which the unit tier cannot
  (it asks the delegate directly and stays green even if nothing reaches it).
  That case empties `UIPasteboard.general` first, and that is load-bearing: the
  walk only continues when the FIELD declines, and Simulator.app syncs the host
  Mac's clipboard by default, so a developer's last copy would otherwise let the
  field take the paste. It passed on a virgin device and failed on the same
  device an hour later before that line existed. See
  [attachments.md](attachments.md) § Paste is a third source.

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

- **`-baybo-demo-html`** (DEBUG, with `-baybo-open-chat`) pushes one agent turn
  whose answer carries rendered LaTeX followed by a `baybo-html` marker, so one
  release scene shows both rich-output paths while the inline preview card, its
  fullscreen expansion and the left-edge swipe out of it remain drivable with
  no gateway. The bytes behind the marker are served by `TranscriptSchemeHandler`
  under the SAME flag (a demo session has no leg, so a real blob read could only
  ever answer `notFound` and the reader would get the failure document);
  `app/ios/UITests/HtmlPreviewUITests.swift` drives the swipe. That test is the
  only tier that can see the gesture at all — the web half's tests stop at the
  bridge, and the native half only exists against a live
  UINavigationController — and it asserts BOTH failures separately: the chat
  header still existing (the pop did not fire) and the header being HITTABLE
  again (the preview really left, rather than the swipe doing nothing behind an
  `allowsHitTesting(false)` chrome layer).

  **The demo document is dark on purpose.** It was white, which is the one colour
  that cannot show how the frame around it is painted: the expanded preview
  reserved the home indicator in `--color-paper`, and no screenshot of a white
  page could ever show that band. The same suite now samples the screenshot's
  bottom rows for brightness, so a white fixture would take that assertion with
  it. `-baybo-appstore-data` is the screenshot-only exception: it serves a
  light-colour derivative of the same self-contained document while leaving
  the dark UI-test fixture unchanged.

- **`-baybo-demo-models`** (DEBUG, with `-baybo-open-chat`) seeds a canned model
  catalog into `ModelCatalog` — gpt-5.5 deliberately via TWO provider entries,
  efforts both set and unset — so the header's model pill + the three-level menu
  render headlessly (no gateway to `GET /v1/llm/models` from); picking a provider
  exercises the draft stash path natively (a level pick's effort PUT fails
  without a gateway — notice + catalog revert, which is itself the failure UI),
  and `app/ios/UITests/ModelPickerUITests.swift` drives the picks through the
  pill's accessibility VALUE (the label is the constant "Model").

- **`-baybo-demo-projects`** (DEBUG) seeds a canned set of boards into
  `ProjectsStore` and short-circuits every refresh, so the Projects tab renders
  with no gateway to fetch from. Four boards on purpose: one wanting all three
  attention kinds at once (so the tab badge and a card's waiting count both
  paint), one merely busy, one idle, and one archived (so the archived toggle
  exists to press). Nothing is persisted — a later plain launch on the same
  simulator inherits none of it, which is why this flag needs no uninstall
  between runs. Add **`-baybo-demo-board`** to land straight on the seeded
  board rather than driving two taps through the cards root; that board is the
  one with something in every stage and all four Waiting-strip kinds (the
  parked approval and the agent's question are seeded directly, because
  `refreshWaitingDetails` reads them off the network and the demo has none). Add
  **`-baybo-demo-card`** to land one level deeper again, on card #41. Every
  card page under `-baybo-demo-projects` fills itself in from that board's own
  fixture (`IssueStore.seedDemoCard`) — the landing flag says which SCREEN to
  open, never which card is real, since a card reached by tapping a row is the
  same card — plus what the board's rows leave empty: a description, a branch,
  a timeline, and a run LOG for the cards the board says have run (41, 42, 43;
  the rest have never run, which the page draws differently). It used to open on the page's own
  loading line for ever, since the card store talks to a gateway of its own and
  the demo has none — so everything the card itself draws had no tier at all,
  which is what `ProjectCardUITests` now covers: where the head, the text and
  the state chips LAND, that the chips are really painted a hue (`redCoverage`,
  since jsdom is blind to colour), the run log in the ⋯, and that the last
  comment can be scrolled out from under the dock. Two of the demo board's eight teammates carry a
  `demo-avatar-<hex>` blob, which `AgentAvatars` draws as a flat disc without a
  gateway — so one screenshot shows both face paths side by side (an uploaded
  picture and the monogram an agent without one falls back to). Eight is two
  past what the face row draws, so the `+N` counter and the `dev-*`/`docs-*`
  monogram widening both paint; card #42 arrives pinned, which is the only way
  the Pinned band header and the row's pin glyph appear without driving a
  swipe.

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
   On a physical device, fresh-install the app (or reset its Local Network
   permission), connect to a private IPv4 address, and verify the system prompt
   appears. Accepting must allow the connection; after denying, enabling Baybo
   again under Settings → Privacy & Security → Local Network must recover it.
4. Chat: streaming, history paging at top, image send/receive, background >45s
   → foreground sync (no duplicates), a `gap`/reconnect sync run, and the
   outbox (send offline → red dot / auto-retry → reconnect resend confirms).
5. Keyboard: composer rides the keyboard, header never moves, transcript holds
   the newest edge through the resize.
