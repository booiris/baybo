# Android companion — lift the shared core out of `app/ios`, add a Kotlin shell

> **Status:** in progress. P0 (spike + contract docs) and P1 (the relocation)
> have landed on `feat/android`; P2 (the platform seams) is next. Recorded
> 2026-09-05 from a repo-wide survey
> (six area readers, three competing architectures, a judge panel, 24 fact
> checks against the code and the vendored crates, and two critique passes).
> Every `file:line` below was verified against master at that date; the
> external claims (uniffi Kotlin generator, reqwest/rustls-platform-verifier
> behaviour on Android, FCM delivery semantics, 16 KiB page policy) were checked
> against the vendored sources or official docs, and the ones that could not be
> are marked *unverified*.

Baybo's phone app today is one SwiftUI shell over a Rust core exported through
UniFFI (`app/ios/ffi`), plus a Vite/React transcript bundle rendered in a
WKWebView (`app/ios/web`). Almost none of that core and none of that bundle is
iOS-specific in substance — only in location and naming. This document is the
plan for adding an Android shell without writing a second core: relocate the
shared halves to `app/mobile/`, add the three platform seams the core is
missing (secure storage, logging, direct-leg TLS), add one transport seam to
the web bundle, build `app/android/` as a Kotlin/Compose shell, ship a
production FCM sender in `remote-host`, and keep CI honest through the move.

Three architectures were scored — *minimal* (Android path-depends on
`app/ios/ffi` in place), *lift the seam* (relocate the shared halves), and
*core-first* (relocate, then push the Swift stores down into Rust). They came
out 106 / 105 / 87 across three judges with different lenses. The plan below
is *lift the seam*, re-sequenced to spike first and move second, with the
push-down rule from *core-first* made binding for the small stores and
deferred for the two large ones.

## Contents

- [Target layout](#target-layout)
- [What must not change](#what-must-not-change)
- [Rust core](#rust-core)
- [Web bundle and its Android host](#web-bundle-and-its-android-host)
- [Swift shell → Kotlin shell, and what moves into Rust](#swift-shell--kotlin-shell-and-what-moves-into-rust)
- [Push](#push)
- [Secure storage on Android](#secure-storage-on-android)
- [Build, CI, release](#build-ci-release)
- [Docs](#docs)
- [Phases and exit criteria](#phases-and-exit-criteria)
- [Decisions for the owner](#decisions-for-the-owner)

## Target layout

```
app/
├── mobile/                          # NEW home of the shared halves (own cargo workspace + own pnpm project, as today)
│   ├── CLAUDE.md                    # layout, commands, the frozen list
│   ├── Cargo.toml / Cargo.lock      # moved from app/ios; same depth, so ../../crates/* path deps need no edit
│   ├── ffi/                         # moved from app/ios/ffi; package → baybo-mobile-ffi; [lib] name = "baybo_ffi" UNCHANGED
│   │   ├── uniffi.toml              # + [bindings.kotlin]
│   │   └── src/
│   │       ├── api.rs               # + SecureStore foreign trait + SecureStoreError, + PushPreview record
│   │       ├── keychain.rs          # iOS imp body untouched; ACCOUNT_PREFIX hoisted; non-iOS imp over SecureStore; read_push_key ungated
│   │       ├── secure_store.rs      # NEW: OnceLock<Arc<dyn SecureStore>>; account → storage-key derivation lives here
│   │       ├── tls.rs               # NEW: http_client_builder(); android = tls_backend_preconfigured(rustls + webpki-roots)
│   │       ├── push_preview.rs      # NEW: decrypt_push_preview(bid, enc_b64, n_b64)
│   │       ├── stores/              # NEW: the pushed-down small stores (title, snippet, approvals, outbox, model catalog)
│   │       └── logging.rs           # file format frozen; mirror branch → __android_log_write under cfg(target_os = "android")
│   ├── bindgen/                     # moved; package → baybo-mobile-bindgen; same binary, now also --language kotlin
│   ├── web/                         # moved; package.json name baybo-ios-transcript → baybo-mobile-web
│   │   └── src/native/transport.ts  # NEW: nativeChannel('baybo'|'deck'): webkit | Android WebMessageListener | console
│   ├── strings/                     # PROPOSED: Localizable.xcstrings moved here so both shells read one catalog
│   └── scripts/
│       ├── build-core.sh            # NEW shared cargo build + uniffi-bindgen (--target / --lang swift|kotlin / --out-dir)
│       └── sync-web.sh              # NEW: pnpm build + copy dist/ into <dest>
├── ios/                             # THIN Swift shell: App/ NotificationExtension/ Tests/ UITests/ untouched
│   ├── project.yml                  # unchanged except the strings resource path if the catalog moves
│   ├── scripts/build-core.sh        # wrapper over ../mobile/scripts/build-core.sh + xcframework/codesign
│   └── scripts/build-app.sh         # web step calls sync-web.sh
└── android/                         # NEW Kotlin/Compose shell
    ├── CLAUDE.md                    # Android continuity contract, written BEFORE the first install ships
    ├── settings.gradle.kts / build.gradle.kts / gradle.properties (NDK pin) / gradle/libs.versions.toml / gradlew
    ├── core/                        # Android library: generated artefacts only, nothing hand-written
    │   ├── build.gradle.kts         # kotlin.srcDirs += build/generated/uniffi; jniLibs; jna@aar; buildRustCore Exec task
    │   ├── src/main/jniLibs/{arm64-v8a,x86_64}/libbaybo_ffi.so      # cargo-ndk output (gitignored)
    │   └── build/generated/uniffi/com/baybo/core/baybo_ffi.kt      # uniffi-bindgen output (gitignored)
    ├── app/
    │   ├── build.gradle.kts         # applicationId com.baybo.app; minSdk 28; compile/target 36; abiFilters arm64-v8a,x86_64
    │   ├── google-services.json.example   # real file gitignored, injected in CI
    │   ├── src/main/assets/transcript/    # sync-web.sh output (gitignored)
    │   ├── src/main/res/values{,-zh-rCN}/strings.xml   # generated from the xcstrings catalog; CI --check
    │   └── src/main/kotlin/com/baybo/app/
    │       ├── BayboApp.kt          # Application: client + notification channels ONLY (WebView warm-up etc. belong to the first Activity)
    │       ├── MainActivity.kt      # single Activity, edge-to-edge, NavHost, onNewIntent routes baybo.sessionId
    │       ├── secure/KeystoreSecureStore.kt   # implements the generated SecureStore; null ONLY for not-found
    │       ├── state/               # Kotlin ports: SessionIndex, ChatSession, DraftStore, AppStore subset (thin adapters over the pushed-down stores)
    │       ├── platform/web/        # TranscriptHost / TranscriptAssets (interceptor + CSP) / TranscriptBridge / WebMediaSeam
    │       ├── platform/            # Lang, Theme, Haptics, Clipboard, QrScanner, ComposerStaging, …
    │       ├── push/                # BayboMessagingService, PushPayloadKeys, NotificationChannels
    │       └── ui/                  # Compose screens
    ├── scripts/                     # build-core.sh (cargo-ndk) / build-app.sh / install.mjs / release.mjs / gen-strings.mjs
    └── docs/                        # build / testing / design-system / navigation / transcript / connection / sync-and-outbox / push

crates/deck/src/render.rs            # include_str! path → app/mobile/web/src/deck/sdkCard.js (the ROOT workspace compiles this file)
crates/skills/src/builtin.rs         # html-gen CSP test reads both the Swift and the Kotlin host's CSP constant
crates/skills/src/builtin/html-gen/SKILL.md   # CSP quote becomes origin-neutral

remote-host/crates/push/src/
├── fcm.rs                           # NEW FcmProvider: ProviderSender (data-only message carrying exactly enc/n/bid)
├── fcm_http.rs                      # NEW HttpFcmSender: POST fcm.googleapis.com/v1/projects/<id>/messages:send + classify
├── fcm_oauth.rs                     # NEW service-account JSON → RS256 assertion → access token (exchange behind a trait)
└── serve.rs / main.rs               # PushConfig { apns: Option, fcm: Option }; push role on when EITHER is configured

docs/modules/mobile/
├── core.md                          # NEW shared-core design doc (UniFFI surface, ClientConfig, SecureStore seam, push-down rule, logging, platform matrix)
├── web-bundle.md                    # NEW bridge contract (message inventory, ready/pending, CSP/sandbox/navigation policy, inset semantics)
└── companion.md                     # retitled "Mobile companion"; lines 325-400 (signing/install/troubleshooting) move to app/ios/docs/build.md

.github/workflows/
├── ci.yml                           # changes: android filter; iOS jobs re-pathed; NEW android-build; NEW remote-host
└── release-android.yml              # NEW dispatch-only: signed APK attached to an EXISTING release tag
scripts/dev-merge-sync.sh            # re-pathed regexes + ANDROID pattern + new job display names (the only name-based consumer)
android.code-workspace               # NEW: rust-analyzer target aarch64-linux-android
```

Why `app/mobile/` and not `crates/mobile-core/`: `app/ios/Cargo.toml` is a
separate workspace on purpose — so `cargo clippy --all --all-features` at the
root never compiles a phone-configured cdylib, so uniffi's `cli` feature never
unifies into the lib build, so the root release profile does not govern the
phone artefact, and so the ffi keeps its own TLS feature set (reqwest
`rustls-no-provider` + rustls `ring`, versus the root's default TLS +
`aws_lc_rs` — the *versions* already match; it is feature unification that
would break). Not `app/core/`: "core" already means the gateway core, the
`baybo_ffi::core` module, and the Swift `BayboCore` module. The subdirectory
stays `ffi/` for the same reason.

## What must not change

Already-shipped iOS installs read their own keychain back after an update.
The move and the seams must leave these byte-identical, and the phase exit
criteria prove it by diff:

- `keychain.rs`: the body of the iOS `mod imp` (`cfg(target_os = "ios")`,
  lines 56-264), the **five** module-level `*_ACCOUNT` consts (314, 338, 372,
  413, 437) **plus `ACCOUNT_PREFIX = "baybo.push-key."` at line 93, which is
  private to the iOS `imp`**. That one const must be hoisted to module level so
  the non-iOS `imp` can use it — a deliberate edit that changes no identity
  input, and the one line exempted from the "empty diff" gate. Every `pub fn`
  signature stays.
- `PairedRecord` / `DirectCredentials` serde field names; the golden-JSON tests
  at `relay/pairing.rs:631-775` and `direct/mod.rs:567-661` are the guard and
  become the guard for both platforms.
- The cache namespace `Application Support/baybo/servers/gateway-<key>/`. The
  key is computed in Rust (`server_cache.rs`); Swift only joins the path.
- `project.yml` bundle ids, team, entitlements. Changing a resource *path* (the
  strings catalog) is not a contract change.
- `build.rs`'s `BAYBO_IOS_KEYCHAIN_ACCESS_GROUP` plumbing.
- **No `panic = "abort"` in `[profile.release]`.** `app/ios/Cargo.toml` has no
  profile section today, so iOS release builds unwind and uniffi's `rust_call`
  turns a Rust panic into an FFI error through `catch_unwind`. Abort would kill
  the iOS app instead. If Android wants it, add `[profile.android-release]`
  (`inherits = "release"`, lto, strip, `panic = "abort"`) selected only by
  `app/android/scripts/build-core.sh --profile android-release`, and record in
  `core.md` that a panic aborts on Android and surfaces as an error on iOS.

Two more things are frozen for practical rather than contractual reasons:

- `[lib] name = "baybo_ffi"`. It is the uniffi namespace, the
  `uniffi_baybo_ffi_*` / `ffi_baybo_ffi_*` C symbol prefix, the
  `libbaybo_ffi.{a,dylib,so}` artefact name, the `DEBUG_TARGETS` filter string
  in `logging.rs:25`, and the name Kotlin's JNA `Native.load("baybo_ffi")`
  resolves. Renaming the *package* (`baybo-ios-ffi` → `baybo-mobile-ffi`) is
  safe; renaming the lib buys nothing.
- `uniffi.toml` `[bindings.swift] module_name = "BayboCore"`: the generated
  `BayboCoreFFI.h` / `BayboCoreFFI.modulemap` are named from it.

Two stale sentences to fix on the way: `app/ios/CLAUDE.md`'s last contract
bullet says the APNs environment reaches Rust through `ClientConfig.apnsEnv`,
but `ClientConfig` is `{ log_dir, blob_cache_dir }` (`api.rs:108-117`); since
`b64ba94a` the environment rides on `PushToken::Apns { environment }`
(`AppDelegate.swift:63`). And `crates/skills/src/builtin.rs:208` still says the
`app/ios` CI jobs are all `if: false`.

## Rust core

Only three things in the crate are iOS-specific in code: the cfg branches in
`keychain.rs`, `debug_seed_push_key` in `lib.rs`, and `build.rs`. Two hidden
blockers sit beside them.

**The silent non-iOS keychain stub.** `#[cfg(not(target_os = "ios"))] mod imp`
(`keychain.rs:265-286`) returns `Ok(())` from every store and `Ok(None)` from
every read. Cross-compiled for Android as-is, `pair_confirm` and
`direct_login` return `Ok`, `binding::active_leg()` then reads nothing back,
every later call fails with `BayboError::NotBound`, and device identity and
push key are re-minted on each call. Guard: right after the P0 spike, add
`#[cfg(target_os = "android")] compile_error!(…)` next to the stub so no `.so`
with it can ever be built; P2 deletes both.

**reqwest 0.13's `rustls-no-provider` hard-depends on
`rustls-platform-verifier`** — `app/ios/Cargo.lock` already resolves
`rustls-platform-verifier-android 0.1.1`. On Android that crate switches to a
JNI-backed verifier that needs `android::init_with_env` called from Kotlin
plus a bundled Kotlin component; without it `Client::builder().build()`
succeeds and the **first TLS handshake panics** on an `.expect(...)`. The wss
legs are unaffected (tokio-tungstenite uses baked-in webpki roots). Fix: a
`tls.rs::http_client_builder()` consumed by the one production reqwest construction site
(`direct/mod.rs:364`; the `reqwest::Client::new()` at `direct/blob.rs:343` is a
`#[cfg(test)]` fixture and stays as is); under `cfg(target_os = "android")`
it uses `ClientBuilder::tls_backend_preconfigured(rustls::ClientConfig with
webpki_roots)` — verified in reqwest 0.13.4 to set `TlsBackend::BuiltRustls`
and never touch the platform verifier. Use that name, not
`use_preconfigured_tls`, which the same source marks as a deprecated shim.
`webpki-roots` becomes a direct dependency. The cost is a product asymmetry:
Android direct mode trusts public roots only, so a self-hosted gateway behind
a private CA connects on iOS and not on Android. Record it in `core.md`'s
platform matrix; the JNI route (option A) is the follow-up for parity.

**The `SecureStore` seam.** All three critique passes converged on this shape:

- `#[uniffi::export(with_foreign)] trait SecureStore { fn get(key) ->
  Result<Option<Vec<u8>>, SecureStoreError>; fn put(key, bytes) -> Result<(),
  SecureStoreError>; fn delete(key) -> Result<(), SecureStoreError> }`.
- **`#[derive(uniffi::Error)] enum SecureStoreError { Failed { reason } }` with
  `impl From<uniffi::UnexpectedUniFFICallbackError>` is mandatory.** A foreign
  trait method that throws anything other than its declared error type goes
  through `handle_callback_unexpected_error`, whose default in uniffi_core
  0.29.5 (`ffi_converter_traits.rs:396-398`) is `panic!`. The exact behaviour
  the Kotlin store must have — throw on `KeyStoreException` or a corrupt file
  — would otherwise become a Rust panic. The `classify_read` invariant
  ("absence is not failure; never mint a new identity on failure") is only as
  good as this conversion.
- **Every non-iOS target uses the SecureStore-backed `imp`
  (`cfg(not(target_os = "ios"))`); the silent host stub is deleted.** Host
  tests install an in-memory `#[cfg(test)]` store, so the read / write /
  absent / failed paths Android actually runs are exercised by the existing
  `ios-core` nextest job. (The earlier idea — a `cfg(target_os = "android")`-only imp plus "a host
  test proves it errors without a store" — cannot work: that imp is never
  compiled on the ubuntu host.)
- The account → storage-key derivation (filesystem-safe name) lives in Rust
  (`secure_store.rs`). Kotlin receives an opaque key and stores bytes verbatim.
  One home for the rule, host-testable.
- Construction: `BayboClient::new(config, secure_store: Option<Arc<dyn
  SecureStore>>) -> Result<Arc<Self>, BayboError>`; on a non-iOS target `None`
  fails construction. `Baybo.swift` passes `nil` and gains a `try` — one line. The `Option` is
  forced by the bindings, not by convenience: `generate --library` emits one
  surface for both shells from a host build, so a `cfg`-split signature would
  generate bindings that disagree with the linked library; this is not the
  `Option`-for-tests shape the root `CLAUDE.md` forbids.
  Do not name a second constructor `with_secure_store`: in this repo `with_*`
  means an optional knob, and the store is a required dependency on Android.
  (Alternative if the Swift edit is unwelcome: keep `new(config)` and add a
  second constructor with a non-`with_` name; see decision 8.)

The rest of the core changes:

| File | Change |
|---|---|
| `Cargo.toml` (workspace) | Neutral header comment; `[workspace.dependencies]` gains `android_log-sys`, `webpki-roots`; `[profile.android-release]` added (`release` untouched) |
| `ffi/Cargo.toml` | Package rename; `[target.'cfg(target_os = "android")'.dependencies]` android_log-sys, webpki-roots; crate-type already includes cdylib |
| `ffi/build.rs` | `emit_ios_keychain_access_group()` only when `CARGO_CFG_TARGET_OS == "ios"` |
| `ffi/uniffi.toml` | `[bindings.kotlin] package_name = "com.baybo.core"`, `cdylib_name = "baybo_ffi"`, `android = true`, `generate_immutable_records = true` (keys verified in uniffi_bindgen 0.29.5 `gen_kotlin/mod.rs:63-79`) |
| `ffi/src/api.rs` | SecureStore + SecureStoreError; `PushPreview { title, body, session_id: Option, badge: Option }`. **`blob_cache_dir` stays `Option` — deferred, with the reason.** The plan's case for making it required was that Android's `temp_dir()` fallback is unwritable; P0 verified that is false on Android 13+, where the framework points `TMPDIR` at the app's cache dir. What is left is a second breaking change to the same record, plus a product decision Swift does not make today: `ServerCache.blobDirectory(in:)` (`ServerCache.swift:36-50`) returns `String?` and yields `nil` on *either* a failed `createDirectory` *or* a failed backup-exclusion flag, logging "falling back to tmp" in both. The Android shell simply always passes a real path. Revisit with that Swift bug (a good directory that falls back to tmp because only the flag failed), not on its own |
| `ffi/src/keychain.rs` | Hoist `ACCOUNT_PREFIX`; non-iOS imp over SecureStore; **drop the `#[cfg(all(debug_assertions, target_os = "ios"))]` gate on `read_push_key` (line 464)** — today it is a verify-nse.sh self-check, and `decrypt_push_preview` needs it in Android release builds |
| `ffi/src/lib.rs` | Constructor as above; export `decrypt_push_preview(bid, enc_b64, n_b64)` over `device_proto::aead::open` + `keychain::read_push_key`, tested with `device_proto::fixtures`; export `refresh_push_binding()` so a rotated FCM token is re-posted while idle (today the relay push refresh only fires on pair/connect edges: `lib.rs:217, 984, 1024, 1104`) |
| `ffi/src/logging.rs` | File writer, rotation, `DEBUG_TARGETS` untouched; the stderr mirror branch becomes `__android_log_write` under `cfg(target_os = "android")` (native fd 2 is `/dev/null` on Android). Not the `android_logger` crate — it installs its own global logger and conflicts with `set_boxed_logger` |
| `ffi/src/blob_helper.rs` | Remove the `temp_dir()` fallback from non-test builds |
| `ffi/src/direct/mod.rs` | The one production reqwest site (`:364`) calls `tls::http_client_builder()`; the `#[cfg(test)]` `Client::new()` fixture in `direct/blob.rs:343` is untouched |
| `ffi/src/stores/` | The pushed-down stores (next section) |

Verified facts about the Kotlin bindings: exported `async fn`s become
`suspend fun` (kotlinx-coroutines is a runtime dependency); a `with_foreign`
trait becomes a Kotlin `interface X` plus an `open class XImpl` in the
bindings package (so implementors avoid the `Impl` suffix, same rule as
Swift); runtime deps are `net.java.dev.jna:jna@aar` and, with `android =
true`, `androidx.annotation`; `android = true` selects
`android.system.SystemCleaner` on API ≥ 34 and falls back to the JNA cleaner
below; `generate --library` extracts metadata from the host `.dylib` exactly
as `build-core.sh` does for Swift, so no NDK is needed to generate Kotlin.
**Cancellation must be documented in `core.md`:** cancelling the calling
coroutine drops the Rust future, but every exported async fn is
`runtime::run(fut)`, which spawns onto tokio and awaits a `JoinHandle`, so the
work completes and only the result is lost. Every client call that mutates
durable state runs in an application-scoped `NonCancellable` scope, never a
lifecycle scope.

## Web bundle and its Android host

The JS ↔ native wire is already platform-neutral in shape: JS posts `{type,
…}` objects on exactly two handler names (`baybo` for the transcript and the
issue page — the latter with a `targetId` on every message — and `deck` for
the deck shell), and native drives JS through
`evaluateJavaScript("window.<baybo|deckShell|issuePage>.<fn>(json)")`,
buffered until the page posts `ready`. Platform coupling lives in a handful of
places:

- The module-scope `window.webkit?.messageHandlers` probes in `bridge.ts:138-151`
  and `deck/bridge.ts:51-62` become `nativeChannel(name)` from
  `src/native/transport.ts`: iOS posts the object to
  `webkit.messageHandlers[name]`, Android posts `JSON.stringify(obj)` to
  `window[name + 'Host']` (a `WebMessageListener` object accepts strings), dev
  logs to console. Prefer webkit when both exist so the existing vitest stubs
  keep working. Message shapes are byte-identical; `TranscriptBridge.swift`
  does not change.
- **Four** sites that hard-code the iOS `baybo-transcript:` scheme, not three:
  `htmlPreviewProtocol.ts` (`HTML_PREVIEW_URL_PREFIX` → `/html-preview/`);
  `deck/sdkCard.js` (`blobUrl` → `/blob/<id>?ct=`) — **`crates/deck/src/render.rs:38`
  compiles this file into the gateway binary with `include_str!`, so this is a
  gateway change reviewed with the deck render gate**; `deck/state.ts`
  (`CARD_CSP` is a literal whose `img-src data: baybo-transcript:` admits the
  custom scheme rather than an origin; on Android it becomes an origin-fed
  value passed by the shell, because an opaque-origin srcdoc cannot use
  `'self'`);
  and **`crates/skills/src/builtin/html-gen/SKILL.md:34`, an agent-facing
  prompt that quotes the html-preview CSP verbatim
  (`script-src 'unsafe-inline' baybo-transcript://localhost/html-lib/`), pinned
  by the root-workspace test
  `html_gen_quotes_the_preview_csp_the_ios_handler_actually_sends`
  (`crates/skills/src/builtin.rs:216`), which reads
  `TranscriptSchemeHandler.swift`.** Android's origin is
  `https://appassets.androidplatform.net`, so the skill text must state the
  policy modulo origin and the test must read every host's CSP constant (only
  Swift's in P2; the Kotlin `TranscriptAssets` constant joins it in P3). Land
  these edits as their own PR ahead of the shell, with a *gating* unit test
  (vitest or BayboTests) asserting the URLs the scheme handler serves — the
  ios-sim UI smokes are `continue-on-error` and can only be checked by hand.
- CSS: the top inset in `styles.css` / `issue.css` / `deck.css` / `cardBase.css`
  comes from `env(safe-area-inset-top)`, which on Android reflects a display
  cutout only, not the status bar. Add a native-fed `setTopInset(px)` bridge
  call and CSS variable that defaults to today's `env()` expression (iOS
  unchanged). Add `overflow-anchor: none` on the document root — Blink's scroll
  anchoring fights the transcript's hand-rolled re-pinning; Safari ignores the
  property. *Append* `system-ui, Roboto` to the deck font stacks (never
  prepend: order changes iOS glyph fallback).
- The three type-only sentinels (`wireSentinel.ts`, `restSentinel.ts`,
  `issue/issueSentinel.ts`) reach `sidecars/` and `app/web/` by relative path;
  `app/mobile/web` sits at the same depth as `app/ios/web`, so they need no
  edit. Any other depth breaks `pnpm build`.
- The bundle's `locales/{en,zh}.ts` (with `parity.test.ts`) is a separate
  table from the Swift catalog and stays that way.

The Android host mirrors the Swift trio (`TranscriptHost` /
`TranscriptSchemeHandler` / `TranscriptBridge`) one for one:

- Serve the bundle from `https://appassets.androidplatform.net/transcript/`
  through `shouldInterceptRequest`: a secure standard origin is what lets
  Vite's `<script type=module crossorigin>` load and what `WebMessageListener`
  origin allowlists key on — the same reason iOS uses a custom scheme rather
  than `file://`. The MIME map and the `/blob/`, `/html-preview/`,
  `/html-lib/` dynamic routes with their CSP / Permissions-Policy / no-store /
  nosniff headers are copied from Swift, with the html-lib `script-src`
  swapped to the platform origin.
- JS → native through `WebViewCompat.addWebMessageListener("bayboHost",
  allowedOrigins)` gated on the callback's `isMainFrame`. **Never
  `addJavascriptInterface`**: it is injected into every frame with no frame
  info, which would hand the agent-authored html-preview and deck-card iframes
  a direct line to native — the hole Swift's `frameInfo.isMainFrame` closes.
- Keyboard: keep the iOS design (the WebView never resizes; native measures
  the composer's top edge and streams `setBottomInset`) with edge-to-edge +
  `windowSoftInputMode="adjustNothing"` + a `WindowInsetsAnimation` callback,
  px = dp × density.
- Renderer death: `onRenderProcessGone` returns true and **replaces** the
  WebView (a dead one cannot be reloaded); hosts expose `recreate()` and keep
  the 3-in-30s budget.
- Settings: `textZoom=100`, no zoom, no file/content access, DOM storage off,
  long-click disabled, algorithmic darkening off.
- **A WebView version gate at startup.** The bundle's CSS uses `:has()`,
  `color-mix()`, `dvh`/`svh`; `vite build.target: es2021` transpiles JS only.
  Require Chromium ≥ 111 and show an "update Android System WebView" screen
  below it.
- Media: Android WebView's media stack does not honour Range on
  `shouldInterceptRequest` responses, so video and audio are materialised to
  `cacheDir` and handed to Media3; `/blob/` serves images only. And the
  interceptor must not `runBlocking` on `blob_bytes_for_display` over the
  2-worker tokio runtime — serve cache-first via `blob_read_cached`, return a
  failure document on miss, and kick an async prefetch that tells the page to
  retry.

## Swift shell → Kotlin shell, and what moves into Rust

The Swift shell is 29k lines (App/Core 14.6k, Screens 11k, Web 2.2k, Support
1.1k, NSE 0.3k) plus 11.7k lines of unit tests and 3.7k of XCUITests. It
splits three ways:

- **Pure UI**: everything under `Screens/` is a Compose rewrite; `PopGesture`
  and Liquid Glass are deleted (Navigation-Compose + predictive back).
- **Pure data logic**: routed by a rule written into `core.md` — a Swift store
  moves into Rust iff (a) it is Foundation-only with no UIKit / observation
  coupling, (b) its on-disk format is pinned or it is fed by gateway data the
  core already fetches, and (c) it has a suite that can become a Rust golden
  test. The root `CLAUDE.md` is explicit: *would a second caller have to copy
  this? If yes, it belongs in the domain crate now, not after the second
  caller shows up* — and Android is that second caller. So:
  - **Pushed down in P2** (all three clauses hold; ~800 lines of Swift in
    total): `RenameTitle` (already spelled twice — `RenameTitle.swift` and
    `app/web`'s `renameTitle.ts` — with the gateway's validator as a third),
    `SearchSnippet` (shared vector file), the *fold* of
    `ChatApprovals.ApprovalQueue` — only the fold: the file is `import SwiftUI`
    and `apply` is `@MainActor` because `PendingApproval.from(frame:)`
    localises the access lines through `Lang` as it reads them
    (`ChatApprovals.swift:27-29, 94`), so the formatting half stays in each
    shell,
    `OutboxStore` (persisted format + state machine + the 10 s / 3 tx / 30 s
    rules), the data half of `ModelCatalog`. Exported as uniffi objects and
    free functions; iOS keeps 50-150-line adapters; the literal fixtures in
    `Tests/PersistedFormatTests.swift` become checked-in JSON consumed first by
    Rust golden tests, then by the Swift tests through the adapters.
  - **Ported to Kotlin in P3, with the failing clause recorded**:
    `SessionIndex` (1,313 lines; welded to `@Published` and the transcript
    mirror writer), `ChatStore` (1,762 lines; welded to `TranscriptBridge`,
    `@Published` and eight `ChatStore*Tests` suites), `DraftStore` (Foundation
    only, so it passes clause (a); it fails clause (b) — the persisted
    `DraftAttachment` embeds `bookmark: Data?`, an iOS security-scoped
    bookmark, so the on-disk format is not portable as it stands,
    `DraftStore.swift:19-22`), the MVP subset of `AppStore`. Same JSON keys and file layout as iOS; the Kotlin
    `PersistedFormatTest` reads the same JSON fixtures.
  - **Retired in P5** (not "evaluated"): `SessionIndex`'s merge rules and
    mutation pump move down if the two ports drift; `ChatStore`'s connState
    driver only if they actually diverge.
- **Platform glue**, written per platform: keychain (via Rust), APNs / FCM,
  WKWebView / WebView, AVFoundation / Media3, PhotosUI / PickVisualMedia,
  VisionKit / CameraX + barcode scanning, UIPasteboard / ClipboardManager,
  scene phase / `ProcessLifecycleOwner` (`.background` →
  `relayInvalidateApiLegs` maps to `ON_STOP` and must stay synchronous).

The NSE has no Android counterpart and needs none: FCM data messages are
delivered to the app process's `FirebaseMessagingService`, decrypt runs
in-process, and the push key never crosses a process boundary. Decrypt goes
through the core's `decrypt_push_preview`, not a second ChaCha20-Poly1305 in
Kotlin. The preview-JSON rules (tolerant `badge`, required `title`/`body`,
optional `session_id`) live only in `NotificationService.swift:79-86` today,
and `NotificationServiceTests.swift` pins just the fixture decrypt,
`session_id` and the wrong-key path (nothing asserts the tolerant `badge`);
they become one JSON fixture shared by the Rust test, the Swift test and the
Android path.

Lifecycle differences that go into `app/android/docs/connection.md`:

- Two cold-start entry points (Activity launch, FCM wake).
  `Application.onCreate` builds the client and the notification channels only;
  WebView warm-up, spool sweep and `didBecomeActive` work belong to the first
  Activity.
- Process death is far more frequent than iOS jetsam. Every cold start reruns
  the full iOS launch sequence (`set_push_token`, install sinks,
  `set_blob_cache_dir`, `resumeStrandedSends`, `resumePendingSessionMutations`);
  Activity recreation without process death must not.
- iOS freezes the process in the background, so `ChatStore`'s `legLost →
  scheduleRetry` ladder never runs there; on Android the process lives for
  hours, the pump's 45 s inbound-liveness watchdog (`transport/pump.rs:28`)
  fires under Doze, and a naive port redials all night. The Kotlin
  `ChatSession` gates reconnect on foreground state (`ON_STOP` → offline,
  `ON_RESUME` → reconnect). This is the one deliberate divergence.
- Locale change: `Lang.swift` toggles in place; `setApplicationLocales`
  recreates every Activity. The shared WebView (`MutableContextWrapper`) must
  survive it and `setLanguage` must be re-sent.
- The `OutboxStore` port keeps the write-before-send order for
  `transmissions` / `lastSentAt`, or a kill after three replays loops forever.

MVP screen set: Landing, Scan, DirectLogin, PairConfirm, ChatList (swipe
archive/pin, long-press rename/resync/delete, pull refresh, unread and
approval marks), Chat (header + transcript WebView + text-only composer +
ApprovalCard + notice line + jump disc + offline glyph), Settings, and the
Confirm / Rename dialogs; a bottom bar with Chats and Settings only. Later, in
rough cost order: Archived + cron (S/M), model picker (M), attachments
composer and image/video/audio viewers (L), message index + subagents (M),
search (M), Deck (L), Projects/Issues (XL).

## Push

The protocol layer is done: `PushTarget::Fcm`, the `PROVIDER_FCM` signing
byte, gateway DTOs accepting `provider: "fcm"`, C's store summarising FCM
targets with `environment = None`, the ffi `PushToken::Fcm` normalised and
mapped to the wire type. What is missing is entirely inside C and the Android
client.

**remote-host.** `serve.rs::build_router` assembles only `ApnsProvider`, so an
FCM `/register` returns 503 ProviderUnavailable today, and the only
`FcmSender` is a `#[cfg(test)]` stub in `provider.rs:73-93`. Add `fcm.rs` /
`fcm_http.rs` / `fcm_oauth.rs` mirroring the `apns.rs` / `apns_http.rs` /
`jwt.rs` split: a mockable sender trait, a pure table-tested
`classify(status, error_code)` (404 `UNREGISTERED` and 400 `INVALID_ARGUMENT`
→ prune; 401 / 403 / 429 / 5xx → transient), a service-account JSON → RS256
assertion → access token exchange behind a trait. `PushConfig { apns: Option,
fcm: Option }`; `FCM_SERVICE_ACCOUNT_PATH` as a Docker secret plus optional
`FCM_PROJECT_ID`; the push role turns on when either is configured.
`docker-compose.yml`, `.env.example`, `.gitignore`, `DEPLOY.md` follow.
**`remote-host-push` (and `server`, `dashboard`, `edge`) has never been
compiled by CI** — `protocol`, `relay`, `admission` are root path-deps and get
compiled by the root jobs, but nothing runs the excluded workspace's own tests
— so P4 adds a `remote-host` job scoped to `-p remote-host-push` (+ server /
edge), not a whole-workspace rerun of the relay tests.

**Message shape.** Data-only — no `notification` block, or the system renders
the undecrypted payload itself:

```json
{ "message": { "token": "…",
               "android": { "priority": "HIGH", "collapse_key": "…", "ttl": "…s" },
               "data": { "enc": "…", "n": "…", "bid": "…" } } }
```

Values are already base64 strings; a 200-char preview seals to well under
FCM's 4 KB data limit. **Verify FCM's `collapse_key` limit (four distinct keys
per device at a time — *unverified*) before writing `fcm.rs`**: the gateway's
key is a 32-hex truncated SHA-256 of `device_id:session_id`
(`crates/gateway/src/push/mod.rs:156-166`), one per session per device, which
FCM would reject or coalesce across sessions once a user has more than four.
Android may need a fixed per-device key plus an in-app notification id.

**Android client.** `BayboMessagingService.onNewToken` → `setPushToken(Fcm)` +
`refreshPushBinding()`; `onMessageReceived` → `decryptPushPreview` →
`NotificationCompat` (`setNumber(badge)`, `PendingIntent` extra under the same
`baybo.sessionId` key iOS uses); suppressed while foregrounded (iOS
`willPresent → []`); `POST_NOTIFICATIONS` at API 33+. Realities that belong in
`push.md` from day one: after a force-stop FCM delivers nothing until the next
manual launch (common on CN OEMs, and the README targets CN users);
`onDeletedMessages` (>100 pending) → mark every session for resync on next
foreground; a denied notification permission makes FCM demote the app's
high-priority messages, so do not post the token to the gateway while the
permission is denied; GMS-less devices degrade to "no push" without crashing;
`android:directBootAware="false"` stated explicitly (the twin of iOS's
`AfterFirstUnlock`).

**Firebase config.** `google-services.json` is gitignored with a committed
`.example` and injected from a CI variable — the repo is public and workflow
artefacts are world-readable. The service-account JSON is a secret of the same
class as the `.p8`, held only on C as a Docker secret.

## Secure storage on Android

`KeystoreSecureStore` implements the generated `SecureStore` interface: one
AES-GCM key in `AndroidKeyStore` wrapping a ciphertext file per key under
`filesDir/baybo/secure/`. The account strings are the iOS ones
(`baybo.paired-gateway`, `baybo.device-identity`, `baybo.device-sign-key`,
`baybo.direct-credentials`, `baybo.active-binding`, `baybo.push-key.<bid>`);
Rust derives the filesystem-safe key before handing it over. What
`app/android/CLAUDE.md`'s contract must say:

- `get` returns null **only** for not-found. Keystore unavailable, corrupt
  file, transient `KeyStoreException` — all throw, and Rust lifts them to
  `SecureStoreError`. Otherwise `load_or_create_device_identity` mints over a
  real identity, the bug `classify_read` exists to prevent on iOS.
- Keystore key parameters: **no** `setUserAuthenticationRequired`, **no**
  `setUnlockedDeviceRequired` (FCM decrypt runs while locked; a device with no
  lock screen must still pair); fall back when StrongBox is absent.
- `android:allowBackup="false"`. Auto Backup restores the ciphertext files to
  a new device but Keystore keys never transfer; under the
  absence-vs-failure invariant every read is then a *failure*, i.e. a
  permanently wedged install. Define the recovery path too: detect "file
  present, key alias absent" at startup, log it, wipe `secure/`, land on
  Landing.
- Test matrix: missing file → null; corrupt file → throw; key alias gone →
  detected at startup. (JVM tests over a fake Keystore; the real one is
  instrumented.)

## Build, CI, release

**Owner's Mac** (none of it is installed today): `brew install --cask
android-commandlinetools`; `sdkmanager "platform-tools" "platforms;android-36"
"build-tools;36.0.0" "ndk;<pin>" "emulator"
"system-images;android-35;google_apis;arm64-v8a"`; `rustup target add
aarch64-linux-android x86_64-linux-android`; `cargo install cargo-ndk
--locked`; `JAVA_HOME=/opt/homebrew/opt/openjdk@17`. Apple Silicon runs
arm64-v8a emulator images while CI's emulator needs x86_64, so the scripts
default to the host's ABI (arm64 locally, both in CI) with `--all-abis` for
release — the ABI twin of the xcframework clobber trap in
`app/ios/docs/build.md`.

**NDK pin** in one place (`gradle.properties`), read by the script,
`ndkVersion` and CI's `sdkmanager`. Pick it after `ls $ANDROID_HOME/ndk` on a
runner, or every CI run downloads ~1 GB. r28+ aligns `.so` files to 16 KiB by
default; older NDKs need `-Wl,-z,max-page-size=16384`. **Run the alignment
assertion on the packaged APK** (`zipalign -c -P 16`, or every
`lib/<abi>/*.so`), not on the cargo output: JNA's `libjnidispatch.so` must be
aligned too. Android 15/16 load a 4 KiB-aligned library in a backcompat mode
with a warning; the hard requirement is Google Play policy for apps targeting
API 35+.

**Gradle.** The `buildRustCore` Exec task declares `inputs.dir(ffi src,
Cargo.lock, uniffi.toml)` / `outputs.dir(jniLibs, generated)`, or every IDE
sync reruns cargo and recompiles the multi-thousand-line generated Kotlin; and
it sets `RUSTC_WRAPPER=` itself, because the root `.cargo/config.toml` pins
sccache and an Android-Studio-launched Gradle inherits the IDE's environment,
not the shell's.

**CI (`ci.yml`).**

- P1: `IOS_DEPS` and the `ios_native` regex gain `app/mobile/`; `ios-web`'s
  four `working-directory` lines + `cache-dependency-path` (414); `ios-core`'s
  `cache-workspaces` (453) + three `working-directory` lines; `ios-sim`'s
  `cache-workspaces` (497), **`cache-dependency-path` (505)**, `hashFiles`
  (518), `cd web` (532) — all re-pathed. **Only `scripts/dev-merge-sync.sh`
  keys on job display names** (the master ruleset carries no
  required-status-checks rule, per the script's own header), so either keep
  the names or rename them and the script in the same PR.
- P3: `changes` gains an `android` output —
  `ANDROID_DEPS='^(app/android/|app/mobile/|crates/(wire|device-proto|model)/|remote-host/|docs/openapi\.json)'`,
  adjacent to `IOS_DEPS` with a comment naming each other. **If the strings
  catalog stays under `app/ios/App/Resources/`, that path must be in this
  regex too**, or an iOS-side string edit skips `android-build` and
  `gen-strings.mjs --check` goes red on the next unrelated Android PR. New
  `android-build` (ubuntu, JDK 17, `setup-rust-toolchain` with both Android
  targets, cargo-ndk, `sdkmanager` NDK install, read-only Gradle cache, an
  `actions/cache` over jniLibs + generated Kotlin keyed like ios-sim's core
  cache plus the NDK pin; `assembleDebug testDebugUnitTest lint`;
  `gen-strings.mjs --check`; **`cargo clippy -p baybo-mobile-ffi --all-targets
  --all-features --locked --target aarch64-linux-android -- -D warnings`** —
  `--all-targets` so test files are gated too, scoped to the ffi crate so the
  `cli`-featured bindgen binary is not cross-compiled). **`ios-sim` needs the
  same for `aarch64-apple-ios-sim`, and for a reason found the hard way in P2:
  the host gate cannot see a target-only warning.** Every `cfg`-gated arm is
  dead code on the platforms it is not for, so the secure-store helpers that
  the host compiles and uses were four `never used` warnings on the iOS target
  — invisible to `ios-core`, which lints the host, and to `ios-sim`, which
  builds the target without `-D warnings`. Two targets with `cfg` arms means
  two clippy runs, or the rule only holds on the third. `cache-save-if: false`:
  `changes` runs only on `pull_request`, so neither `ios-core` nor
  `android-build` ever runs on master and there is no master-warmed cache to
  share; the 10 GB budget is already tight. Optional non-gating
  `android-emulator` (`reactivecircus/android-emulator-runner`, x86_64, KVM)
  — **instrumented tests that live only in a non-gating job cannot be exit
  criteria**; either gate the emulator job on `ANDROID_DEPS` or write those
  assertions into `app/android/docs/testing.md` as an owner-run device
  checklist, as iOS does.
- **`scripts/dev-merge-sync.sh` gains `ANDROID_DEPS_PATTERN` and the
  `android-build` display name in the same P3 PR, and the remote-host
  pattern/name in P4.** Otherwise a skipped job is indistinguishable from a
  pass at merge time — the failure mode the script's header documents.
- Probe PRs as exit criteria: a PR touching only `app/mobile/ffi` must queue
  `ios-web`, `ios-core`, `ios-sim` and `android-build` (`ios-web` rides on the
  same `ios` output as `ios-core`, `ci.yml:404`) *and* the script must require
  them; a docs-only PR must skip all four.
- Three root-workspace consumers of `app/ios` paths that every architecture
  proposal missed: `crates/deck/src/render.rs:38`
  (`include_str!("../../../app/ios/web/src/deck/sdkCard.js")` — root `clippy`
  and `cargo test` go red on the move), `app/web/src/pages/chat/mathDelimiters.port.test.ts:18-22`
  (reads `../ios/web/src/{mathDelimiters.ts, mathDelimiters.test.ts,
  transcript/cursor.ts}` in the root `frontend` job), and
  `crates/skills/src/builtin.rs:219` (reads
  `../../app/ios/App/Web/TranscriptSchemeHandler.swift`; unaffected by P1 since
  Swift stays, touched in P2 with the CSP change).

**Release.** A separate dispatch-only `release-android.yml` attaches
`baybo-android-<version>.apk` (+ `.sha256`) to an **existing** release tag,
with `contents:write` on the upload job only; it never creates a tag and
never touches `SHA256SUMS` (`install.sh` fetches only `baybo-<target>.tar.gz`
and is unaffected). `versionCode` is derived from semver — the iOS
`YYYYMMDDHHMM` stamp overflows Android's ceiling. **`versionName` has four
candidate sources today**: root `Cargo.toml` 0.1.1, `app/ios/Cargo.toml`
0.1.0, `project.yml` `MARKETING_VERSION` 0.1.0 (overridden per release by
`release.mjs --version`), `app/ios/web/package.json` 0.1.0; the recommendation
is to mirror iOS and take it from the release script / dispatch input.
**Play vs sideload must be decided before P4**: Play needs an AAB, Play App
Signing, the Data safety form, a privacy-policy URL, a CAMERA justification.

**Log export.** `log_dir = filesDir/baybo/logs`; one FileProvider path plus a
Settings row that fires `ACTION_SEND`, or the first device bug report arrives
without logs.

## Docs

- New: this file; `docs/modules/mobile/core.md`;
  `docs/modules/mobile/web-bundle.md`; `app/mobile/CLAUDE.md`;
  `app/android/CLAUDE.md`; `app/android/docs/{build, testing, design-system,
  navigation, transcript, connection, sync-and-outbox, push}.md`. **The
  `app/android/CLAUDE.md` contract and the `core.md` skeleton are written in
  P0**, before any code: applicationId, secure-store key names, the
  `filesDir/baybo/servers/gateway-<key>/` namespace with the same children as
  `ServerCache.swift`, persisted JSON = the same bytes the ffi golden tests
  guard, the FCM data-only contract, `allowBackup=false`,
  `directBootAware=false`, Keystore key parameters, the WebView floor, the
  cold-start checklist, the `NonCancellable` rule.
- Lifted into the shared docs: `app/ios/docs/connection.md:1-150` (the
  supervisor design), `sync-and-outbox.md:11-35, 463-571` (sync-v2 client loop
  + outbox semantics), `transcript.md:250-275` (bridge vocabulary). The iOS
  docs keep "The Swift half" and cross-link.
- Wording: `companion.md` retitled *Mobile companion*, **lines 325-400**
  (signing boundary, build & install, troubleshooting) moved to
  `app/ios/docs/build.md`, 314-324 (Status & open items) kept and neutralised;
  `relay-push-security.md` drops its "today" qualifiers and gains FCM twin
  sections; the *Android support* entry in `roadmap.md`: *design* → *in progress* in the
  P1 PR (the legend's "partially landed on master"); `docs/modules/README.md` index;
  root `CLAUDE.md` = `AGENTS.md` (byte-identical — edit both) CI paragraph;
  `README.md` "## iOS app" and `README.zh-CN.md` "## iOS 应用" → "## Mobile
  apps" and its zh twin;
  `docs/releasing.md:40-51` lockfile path; `docs/modules/deck.md`'s claim that
  the deck shell needs no CI-filter change. About 60 files mention `app/ios`,
  ~25 of them naming `app/ios/{ffi,web}`.
- `docs/todo/cross-crate-contract-dedup.md` steps 1-2 (one push-token
  normaliser, one owner for the push/blob constants) say "while the Android
  provider work is still pre-release"; they land in P2 beside the SecureStore
  seam so the predicate at `api.rs:81` / gateway `push.rs:117` /
  `notify.rs:64` does not gain a fourth site.
- `app/ios/CLAUDE.md`'s rule is *read the area doc before changing code*.
  This plan adds: *update that doc in the same PR*. `keychain.rs` →
  `companion.md` § binding + the "what Android may touch" notes on the iOS
  contract; `bridge.ts` / `styles.css` → `app/ios/docs/transcript.md`;
  `tls.rs` → `core.md` TLS section; `sdkCard.js` → `docs/modules/deck.md`.

## Phases and exit criteria

| Phase | Scope | Size | Exit criteria |
|---|---|---|---|
| **P0 — spike + decisions** (no move, no Android code) | Install the toolchain; run `cargo ndk -t arm64-v8a build -p baybo-ios-ffi` and `uniffi-bindgen --language kotlin` on the **untouched** crate; **after** recording the result, land the `compile_error!` guard in the same PR as the `apnsEnv` doc fix; write `app/android/CLAUDE.md` and the `core.md` skeleton (with the push-down rule); take the decisions below | 1-2 days | ring / snow / rustls cross-compile under the pinned NDK; `com/baybo/core/baybo_ffi.kt` is emitted |
| **P1 — pure move** | `git mv` the five paths; three package renames (`baybo-ios-ffi`, `baybo-ios-bindgen`, `baybo-ios-transcript`); re-path five iOS scripts, `ci.yml`, `dev-merge-sync.sh`, `ios.code-workspace`, split `app/ios/.gitignore` (its `/target` and `web/*` lines move to a new `app/mobile/.gitignore`), **`crates/deck/src/render.rs`**, **`mathDelimiters.port.test.ts`**, ~60 docs/comments; job display names unchanged | 1-2 days | `Generated/BayboCore.swift`, `BayboCoreFFI.h`, modulemap byte-identical; `nm -gU libbaybo_ffi.a \| grep -E ' T _(uniffi\|ffi)_baybo_ffi_' \| sort` identical (**Rust-internal symbol hashes change with the package name — expected**); content of every moved file byte-identical except the three package names, the lockfile cargo rewrites from them, and the one `compile_error!` string that quotes the old package name (the iOS `mod imp` body, all six account literals and every `pub fn` signature unchanged, verified by hashing lines 56-263); root `clippy` / `cargo test` / `frontend` and the three iOS jobs report `pass`, not `skipping`; `git grep -nE 'app/ios/(ffi\|bindgen\|web)\|baybo-ios-' -- . ':!docs/todo/android-companion.md'` is empty (this file keeps its historical `app/ios/…` mentions on purpose) |
| **P2 — seams + small push-down** (iOS: a few adapter lines) | SecureStore + SecureStoreError + non-iOS imp + fallible constructor; hoist `ACCOUNT_PREFIX`, ungate `read_push_key`; logcat; `tls.rs`; `blob_cache_dir` required; `decrypt_push_preview` / `refresh_push_binding`; `[bindings.kotlin]`; bundle `transport.ts` + the four origin sites (incl. SKILL.md and the skills test, own PR) + `setTopInset` + `overflow-anchor`; push down RenameTitle / SearchSnippet / ApprovalQueue / OutboxStore / ModelCatalog-data with JSON fixtures as Rust golden tests and thin iOS adapters; `app/mobile/scripts/{build-core.sh, sync-web.sh}`; contract-dedup steps 1-2; `core.md` / `web-bundle.md` bodies | 1.5-2.5 weeks | `git diff` of `keychain.rs` lines 56-264 (the iOS `imp`) is exactly the `ACCOUNT_PREFIX` hoist, with the five `*_ACCOUNT` consts and every `pub fn` signature unchanged; ios-sim green (UI smoke outcome checked by hand); the `BayboCore.swift` diff is additive items + constructor signature + `blobCacheDir` type; host nextest covers the four SecureStore paths through the in-memory store and decrypts the fixture ciphertext through `decrypt_push_preview`; the pushed-down stores' Rust golden tests read the iOS literal fixtures and re-serialise the same bytes; the eight `ChatStore*` suites untouched and green |
| **P3 — Android scaffold + MVP** | Gradle project, `:core` / `:app`, cargo-ndk script + post-assemble alignment assertion, `KeystoreSecureStore` (null-vs-throw, file-without-key tests), the `BayboApp` / `MainActivity` split, WebView floor, `TranscriptHost` / `Assets` / `Bridge`, Kotlin ports of `SessionIndex` / `ChatSession` / `DraftStore` / `AppStore` subset, MVP screens, `strings.xml` generation + `--check`, `android-build` job **+ the dev-merge-sync Android pattern and check name** + probe PRs, `android.code-workspace` | 4-6 weeks | On a device and an emulator: relay scan-to-pair and direct login both bind **and survive force-stop + relaunch** (same `device_id`, no `NotBound`); a send shows echo → sent → released in the outbox file; streamed reply; approval card; offline glyph; keyboard raise/lower keeps the last row pinned; html-preview renders under the CSP; a sandboxed iframe cannot post to `bayboHost` (instrumented test or device checklist); the ffi-only probe PR queues all four jobs (`ios-web`, `ios-core`, `ios-sim`, `android-build`) and the script requires them |
| **P4 — push** | remote-host `fcm.rs` / `fcm_http.rs` / `fcm_oauth.rs` + `PushConfig` + compose / DEPLOY + the `remote-host` CI job (`-p remote-host-push`) + its dev-merge-sync entry; Android `push/` package; `POST_NOTIFICATIONS`; `push.md` covering force-stop, `onDeletedMessages`, permission demotion, GMS-less devices, `directBootAware` | 1-2 weeks | Real gateway → C → FCM → a locked device shows the decrypted title/body and the tap opens the session; `onNewToken` re-posts without a reconnect edge; a C without FCM credentials still answers 503 to an FCM register; the fixture decrypts in the Rust test and on the Android path |
| **P5 — parity, release lane, retire Swift copies** | Attachments, Archived / cron, model picker, message index / subagents, search, Deck, Projects / Issues; `release-android.yml` + `release.mjs`; optional emulator job; `SessionIndex` merge / mutation pump pushed down if the ports drift; TLS option A as the private-CA follow-up | open-ended (deck + projects ≈ 4 weeks) | Every iOS screen has an Android counterpart or a recorded deliberate omission; `release-android.yml` dry-run yields a signed APK whose `apksigner verify` matches |

## Decisions for the owner

1. **Direct-leg TLS.** Option B (Android trusts public roots only, no JNI) for
   the MVP; option A (`rustls-platform-verifier` JNI init + its Kotlin
   component) as the private-CA follow-up. This is a product decision: a
   self-hosted gateway behind a private CA will not connect from Android in
   direct mode until A lands.
2. **Distribution.** GitHub-release APK sideload or Google Play. Decides the
   `release.mjs` artefact (APK vs AAB), the signing model, and a stack of Play
   paperwork.
3. **`versionName` source** — one of the four above; the recommendation is the
   release-script / dispatch `--version` input, as iOS does. And whether the
   APK attaches to the gateway's `vX.Y.Z` release or its own tag series.
4. **Strings catalog.** Move `Localizable.xcstrings` to `app/mobile/strings/`
   (one resource-path edit in `project.yml`) or keep it under `app/ios` and add
   the path to `ANDROID_DEPS`.
5. **`minSdk 28`** (javax.crypto ChaCha20, stable Keystore AES-GCM; uniffi's
   cleaner falls back below 34 — fine).
6. **System font scale in the transcript.** iOS ignores Dynamic Type inside
   the WKWebView; `textZoom=100` on Android matches it. Decide for both shells
   together.
7. **QR library.** ML Kit (depends on Google Play Services) or ZXing — decide
   alongside the "GMS-less devices must still work" stance.
8. **Constructor shape.** One fallible `new(config, secure_store: Option<…>)`
   (one Swift line changes) or keep `new(config)` and add a second constructor
   with a non-`with_` name.
