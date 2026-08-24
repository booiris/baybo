# Baybo iOS

A **SwiftUI app** whose screens, header, and composer are native — so the iOS
keyboard never touches web content — with **only the chat transcript** rendered
in a WKWebView, and the transport/crypto core kept in **Rust behind UniFFI**.

For app behavior the root [`/CLAUDE.md`](../../CLAUDE.md) applies. The visual
system is [`docs/design-system.md`](docs/design-system.md). Everything else is
indexed under [Docs](#docs) below.

## Layout

```
Cargo.toml            — own cargo workspace (root workspace excludes app/ios)
ffi/                  — UniFFI core: transport legs, pairing, keychain, blobs.
                        Exports BayboClient + parsePairQr.
bindgen/              — uniffi-bindgen CLI (separate member so the `cli`
                        feature never unifies into the lib build)
project.yml           — xcodegen spec (the committed source of truth)
App/                  — SwiftUI sources + resources
NotificationExtension/ — NSE Swift sources
web/                  — the transcript-only Vite/React bundle
docs/                 — the subsystem docs indexed below
scripts/              — build-core.sh, build-app.sh, install.mjs, verify-nse.sh
Generated/ Externals/ — build products (gitignored): BayboCore.swift + .xcframework
```

## Build & test

```bash
scripts/build-app.sh             # web → rust xcframework → xcodegen → sim build
scripts/build-app.sh --device --release
node scripts/install.mjs         # archive + export + devicectl install (USB)

cargo nextest run --workspace                            # ffi host tests
cargo clippy --workspace --all-targets --all-features -- -D warnings
(cd web && pnpm lint && pnpm test && pnpm build)         # transcript bundle
```

Two traps that cost time every single loop, and are the reason
[`docs/build.md`](docs/build.md) exists: `build-app.sh` defaults to **sim-only**
and will overwrite a signed device `BayboCore.xcframework` (use `--skip-rust`
for Swift/web iteration), and the Swift tiers need `xcodegen generate` plus a
`build-for-testing` / `test-without-building` pair rather than a plain
`xcodebuild test`. Read [`docs/build.md`](docs/build.md) and
[`docs/testing.md`](docs/testing.md) before running either half by hand.

**`app/ios` is its own cargo workspace and its own pnpm project.** The root
`cargo test --workspace` and the root `frontend` CI job have never covered any
of it. Its CI is three jobs — `ios-web` (ubuntu, 1×), `ios-core` (ubuntu, 1×)
and `ios-sim` (macos-26, 10×) — and **all three are `if: false`** while the
Actions quota is out, so every tier here is covered by nothing but a
laptop; see [`docs/testing.md`](docs/testing.md).

## Continuity contract (do not change — existing installs depend on it)

**Who this protects.** The only supported upgrade path is `app/ios` → `app/ios`,
and this contract exists for exactly that population: **already-shipped `app/ios`
installs**, which read their own keychain blobs back after an update. Upgrades
from any earlier, retired companion app are out of scope — don't add a compat
shim for one, and don't cite one as a reason for anything. Breaking any bullet
below silently loses a real user's device identity, pairing, and push key — no
error they can act on, no way back but a re-pair.

- Bundle ids `com.baybo.app` / `com.baybo.app.NotificationExtension`; team
  `KLK5BP5YS6`.
- `keychain-access-groups`: `$(AppIdentifierPrefix)com.baybo.app` stays the
  FIRST (only) entry — the five app-private keychain items live in the default
  group, which is the first entitlement entry.
- Keychain items set **no `kSecAttrService`** (a query with any service string
  finds nothing). Accounts: `baybo.push-key.<bid>` (shared group,
  AfterFirstUnlock), `baybo.paired-gateway`, `baybo.device-identity`,
  `baybo.device-sign-key` (never deleted), `baybo.direct-credentials`,
  `baybo.active-binding` (all ThisDeviceOnly). What is frozen in
  `ffi/src/keychain.rs` is every input to item IDENTITY — account names, the
  absent `kSecAttrService`, the access group, the accessibility class. Change one
  and an existing install stops finding its own items. (Error handling is not
  identity and is fair game: a read distinguishes `errSecItemNotFound` from a
  failure precisely so a transient one can't be mistaken for absence — see
  `classify_read`.)
- The `PairedRecord` / `DirectCredentials` JSON field names ARE the on-keychain
  byte format, shared with every already-shipped install. Renaming one is not a
  refactor — it silently loses the gateway binding of every device that
  upgrades. The golden-JSON tests in `ffi/` are what stands in the way.
- NSE `Info.plist` key `BayboKeychainAccessGroup`; push payload/decrypt
  contract unchanged (see
  [`docs/modules/mobile/relay-push-security.md`](../../docs/modules/mobile/relay-push-security.md)
  § Notify flow and
  [`docs/modules/mobile/companion.md`](../../docs/modules/mobile/companion.md)
  § Push preview).
- APNs environment comes from the shared `BAYBO_APNS_ENVIRONMENT` build setting:
  it expands into both the signed `aps-environment` entitlement and the app's
  `BayboApnsEnvironment` Info key, which Swift passes through
  `ClientConfig.apnsEnv`. Never infer it from Swift `DEBUG` or Rust
  `cfg!(debug_assertions)` — optimization and signing environment are independent.

## Docs

Read the doc for the area you are about to change, BEFORE you change it. Each
one is scar tissue: most of its sentences exist because the obvious alternative
was tried and failed, and several name a bug that shipped once already.

- [`docs/build.md`](docs/build.md) — build order, device slices, xcframework
  signing, and the sim-loop clobber trap.
- [`docs/testing.md`](docs/testing.md) — the four test tiers, the
  `BayboUITestCase` launch contract, CI's two jobs and their path filter, every
  `-baybo-*` demo flag for headless UI verification, and the device checklist.
- [`docs/design-system.md`](docs/design-system.md) — the visual system:
  monochrome soft line minimalism, the tokens (mirrored in `web/src/styles.css`
  and `App/Support/Theme.swift`), and the deliberate divergence from
  `app/web`'s brutalism.
- [`docs/navigation.md`](docs/navigation.md) — the tabbed home shell, the outer
  `NavigationStack`, the interactive-pop gesture and its velocity clamp, and
  Liquid Glass on an 18.0 deployment target.
- [`docs/chat-list.md`](docs/chat-list.md) — `SessionIndex`, live unread, the
  approval mark, the app-icon badge, and push-tap routing.
- [`docs/connection.md`](docs/connection.md) — the chat-leg connection
  supervisor (one loop owns dial/death/subscription state), the `LegDialer`
  seam, the send gate, the ack judgment, and the Swift `connState`
  continuations. Shaped by the 2026-08-16 cold-start send black hole; read it
  before touching `ffi/src/transport/` or the ChatStore dial paths.
- [`docs/transcript.md`](docs/transcript.md) — the one reused WKWebView, store
  lifecycle and offscreen frame buffering, the native ⇄ web bridge, the keyboard
  inset, markdown/LaTeX rendering, and the message index.
- [`docs/attachments.md`](docs/attachments.md) — file cards, the image viewer,
  the native audio engine, and video tiles.
- [`docs/sync-and-outbox.md`](docs/sync-and-outbox.md) — sync-protocol v2 on the
  client, transcript mirror retention (**do not re-add a sweeper**), and the
  persisted send outbox. Read
  [`docs/sync-protocol.md`](../../docs/sync-protocol.md) first.
- [`docs/model-picker.md`](docs/model-picker.md) — the header capsule, the
  hand-rolled menu panel, and the per-session `(entry, model, effort)` pin.
- [`docs/approvals.md`](docs/approvals.md) — the native tool-approval card and
  the four frames that drive it.
- [`docs/deck.md`](docs/deck.md) — the iOS half of Deck;
  [`docs/modules/deck.md`](../../docs/modules/deck.md) is the design's source of
  truth.
- [`docs/subagents.md`](docs/subagents.md) — the header's `Subagents` entry, the
  three GET-only gateway routes behind it, the lineage-scoped readability
  predicate, and the read-only child transcript (second webview, no mirror).

## Known gaps / follow-ups

- ~~Native chrome uses SF Mono~~ — Space Mono is bundled
  (`App/Resources/Fonts`, OFL) and registered via `UIAppFonts`; `Theme.mono`
  serves it with a system-face fallback.
- Voice input has no composer affordance — the mic placeholder button was
  removed with the Liquid Glass restyle. Wiring real capture later means
  re-adding the button, not just filling in a handler.
- The Projects tab renders its cards root and the new-board form; the board,
  the card detail, the run transcript and the team screens are still to come
  (see [`docs/projects-plan.md`](docs/projects-plan.md) P4–P7).
