# Baybo Android

A **Kotlin/Compose shell** whose screens, header, and composer are native — so
the Android IME never touches web content — with **only the chat transcript**
rendered in a WebView, over the same UniFFI Rust core the iOS app runs
(`app/mobile/ffi` once the P1 relocation lands; `app/mobile/ffi` until then).

For app behavior the root [`/CLAUDE.md`](../../CLAUDE.md) applies. The plan this
shell implements — the move, the three platform seams, the phases, and the
decisions still open — is
[`docs/todo/android-companion.md`](../../docs/todo/android-companion.md); read
it before starting any phase.

**Nothing under `app/android/` exists yet except this file.** P3 builds the
shell. This document is written first on purpose: the contract in
[Continuity contract](#continuity-contract-do-not-change--the-first-shipped-install-freezes-this)
is the part the first shipped APK freezes, and freezing it costs nothing today
and a re-pair for every user later.

## Layout

The tree P3 creates. Paths marked *(generated)* are build products and belong in
`.gitignore`, not in a commit.

```
CLAUDE.md                   — this file: the contract, written before the code
settings.gradle.kts         — :core and :app
build.gradle.kts            — root plugins + versions
gradle.properties           — the NDK pin, read by the script AND by ndkVersion
gradle/libs.versions.toml   — the version catalog
gradlew / gradle/wrapper/   — committed wrapper
core/                       — Android library: GENERATED artefacts only,
                              nothing hand-written
  build.gradle.kts          — kotlin.srcDirs += build/generated/uniffi;
                              jniLibs; jna@aar; the buildRustCore Exec task
  src/main/jniLibs/<abi>/libbaybo_ffi.so          (generated — cargo-ndk)
  build/generated/uniffi/com/baybo/core/*.kt      (generated — uniffi-bindgen)
app/
  build.gradle.kts          — applicationId, minSdk/targetSdk, abiFilters
  google-services.json      (gitignored; .example committed, real one from CI)
  src/main/assets/transcript/                     (generated — sync-web.sh)
  src/main/res/values{,-zh-rCN}/strings.xml       (generated — gen-strings.mjs)
  src/main/kotlin/com/baybo/app/
    BayboApp.kt             — Application: client + notification channels ONLY
    MainActivity.kt         — one Activity, edge-to-edge, NavHost, onNewIntent
    secure/                 — KeystoreSecureStore
    state/                  — SessionIndex, ChatSession, DraftStore, AppStore
    platform/web/           — TranscriptHost / TranscriptAssets /
                              TranscriptBridge / WebMediaSeam
    platform/               — Lang, Theme, Haptics, Clipboard, QrScanner, …
    push/                   — BayboMessagingService, PushPayloadKeys,
                              NotificationChannels
    ui/                     — Compose screens
scripts/                    — build-core.sh, build-app.sh, install.mjs,
                              release.mjs, gen-strings.mjs
docs/                       — the subsystem docs indexed below
```

`core/` holds no hand-written Kotlin by rule. Anything you are tempted to add
there is either a Rust change in the ffi crate or an adapter in `app/`; a
hand-edited file next to generated output is a file the next `buildRustCore`
silently outranks.

## Build & test

None of these scripts exist yet — P3 writes them, mirroring
`app/ios/scripts/`.

```bash
scripts/build-core.sh            # cargo-ndk → jniLibs + uniffi-bindgen --language kotlin
scripts/build-app.sh             # web → core → gradle assembleDebug
node scripts/install.mjs         # assemble + adb install

./gradlew testDebugUnitTest lint          # JVM tiers
./gradlew connectedDebugAndroidTest       # instrumented (device or emulator)
cargo clippy -p <ffi crate> --all-targets --all-features --locked \
    --target aarch64-linux-android -- -D warnings
```

Like the xcframework clobber trap in
[`../ios/docs/build.md`](../ios/docs/build.md), the ABI is the thing that costs
a loop: Apple Silicon runs `arm64-v8a` emulator images while CI's emulator is
`x86_64`, so the scripts default to the host's ABI and take `--all-abis` for a
release build. A debug APK assembled for the wrong ABI installs and then dies at
`Native.load("baybo_ffi")`.

**The owner's Mac — this machine's paths, not a portable recipe.** CI installs
its own SDK and pins its own NDK; nothing below should be hard-coded into a
script or a workflow.

- `ANDROID_HOME=/Volumes/data/android-sdk` — the SDK lives on the **external
  drive**, not `~/Library/Android/sdk`. It was installed by unzipping Google's
  `commandlinetools-mac_arm64-16111833_latest.zip` into
  `$ANDROID_HOME/cmdline-tools/latest/` (`Pkg.Revision=23.0`), **not** through
  Homebrew — no `android-commandlinetools` cask is involved, so do not `brew
  upgrade` anything expecting it to move.
- `JAVA_HOME=/opt/homebrew/opt/openjdk@17` (17.0.16). Gradle and AGP both read
  it; an Android-Studio-launched Gradle inherits the IDE's environment instead,
  which is also why `buildRustCore` must set `RUSTC_WRAPPER=` itself rather than
  trust the root `.cargo/config.toml`'s sccache pin.
- **NDK pin `28.2.13676358`** (r28c), already installed under
  `$ANDROID_HOME/ndk/`. r28+ aligns `.so` files to 16 KiB by default; older NDKs
  need `-Wl,-z,max-page-size=16384`. r29.0.14206865 is stable too but newer than
  this toolchain needs, so it buys nothing. The pin lives in
  `gradle.properties` and is read from there by the build script, by
  `ndkVersion`, and by CI's package install — one home, or the three drift and a
  CI run re-downloads ~1 GB.
- cmdline-tools 23.0 prints, on every `sdkmanager` invocation: *"The SDK Manager
  CLI tool (sdkmanager) is deprecated. Android CLI will be used instead."* The
  replacement is the `android` binary in the same `bin/` directory —
  `android sdk list` / `android sdk install`. **The two spell package paths
  differently**: listings use `/` (`ndk/28.2.13676358`), while
  `sdkmanager --install` still takes `;` (`ndk;28.2.13676358`). Copying a path
  out of a listing into an install command fails with an unhelpful message.
- Still missing on this machine, and needed before the first Gradle build:
  `platforms;android-36`, `build-tools;36.0.0`, `emulator`, a
  `system-images;…;arm64-v8a`, the two Rust targets (`rustup target add
  aarch64-linux-android x86_64-linux-android`). `platform-tools` and
  `cargo-ndk` are already installed.

`app/android` will be its own Gradle project the way `app/ios` is its own Cargo
workspace and pnpm project: the root `cargo test --workspace` and the root
`frontend` job cover none of it. Its CI is the `android-build` job behind an
`ANDROID_DEPS` path filter, plus its entry in `scripts/dev-merge-sync.sh` — and
that entry is not optional. Without it a skipped job is indistinguishable from a
passing one at merge time.

## Continuity contract (do not change — the first shipped install freezes this)

**Who this protects.** Nobody, yet. Android has **no installed base**, and this
is the one and only moment every name below is free to design. The moment the
first APK leaves a laptop, each bullet carries exactly the force its iOS twin in
[`../ios/CLAUDE.md`](../ios/CLAUDE.md) carries: breaking one silently loses a
real user's device identity, pairing, and push key — no error they can act on,
no way back but a re-pair. Choose deliberately now; after that, treat this
section as read-only.

- **`applicationId` is `com.baybo.app`** — the same string as the iOS bundle id
  (`../ios/project.yml:90`). On Play it is the app's *only* identity: it keys
  the listing, the upload certificate, `filesDir`, and the FCM app registration.
  It can never change. A rename does not migrate anything; it publishes a
  second, unrelated app and strands every install on the old one.
- **The notification service class is `com.baybo.app.push.BayboMessagingService`**,
  declared in the manifest. The class name and its `<service>` entry move
  together, so a rename is a refactor and not a break. What is **not**
  refactorable is the **notification channel id**: Android keys the user's
  per-channel importance, sound, and "block" choices to that string and carries
  them across upgrades, so a changed id silently discards every setting the user
  made — including un-muting a channel they muted. Pick the channel ids in P3
  and write them into this bullet in the same PR.
- **Secure-store slot names are the iOS keychain account strings, verbatim.**
  Reusing them is what lets `keychain.rs` keep one set of module-level consts
  instead of growing a per-platform name table — one home for the rule, per the
  root `CLAUDE.md`'s crate-boundary section. Five are module-level consts today
  and one is assembled from a prefix:

  | Slot | Literal | Where |
  |---|---|---|
  | paired-gateway record | `baybo.paired-gateway` | `../mobile/ffi/src/keychain.rs:314` |
  | Noise static identity | `baybo.device-identity` | `../mobile/ffi/src/keychain.rs:338` |
  | Ed25519 push-signing seed | `baybo.device-sign-key` | `../mobile/ffi/src/keychain.rs:372` |
  | direct credentials | `baybo.direct-credentials` | `../mobile/ffi/src/keychain.rs:413` |
  | active-binding marker | `baybo.active-binding` | `../mobile/ffi/src/keychain.rs:437` |
  | per-device push key | `baybo.push-key.` + `bid` | `ACCOUNT_PREFIX`, `../mobile/ffi/src/keychain.rs:93` |

  `ACCOUNT_PREFIX` is private to the iOS `imp` today; P2 hoists it to module
  level so the non-iOS `imp` shares it. That hoist is the *one* line exempted
  from P1's empty-diff gate on `keychain.rs`, and it changes no identity input.
  `baybo.device-sign-key` is never deleted — not by logout, not by unpair —
  because its public half **is** the `device_id`, and the gateway's
  `baybo.push-key.<device_id>` row is addressed by it.
- **Rust derives the on-disk storage key from the account; Kotlin never sees the
  account string.** `ffi/src/secure_store.rs` owns account → filesystem-safe key;
  `KeystoreSecureStore` receives an opaque key and stores the bytes verbatim
  under it. The key is a lowercase hex SHA-256 of the account, and **that
  output is the on-disk name, so a different hash or encoding orphans every
  stored item.** The reason is not sanitisation of a hostile input: the device
  id in `baybo.push-key.<bid>` is derived locally
  (`device_proto::delegation::device_id_for` — `device-` plus 64 hex chars) and
  was never a filesystem hazard. The reason is that "what characters are safe
  here" is a rule, and a rule answered on both sides of an FFI is answered
  differently on the second side eventually — surfacing as an install that
  cannot find its own pairing. One total function, one home, host-testable.
- **`get` returns null ONLY for not-found.** Keystore unavailable, a corrupt
  ciphertext file, a transient `KeyStoreException`, a decrypt that fails its tag
  — every one of those **throws**, and Rust lifts it to `SecureStoreError`
  rather than to `Ok(None)`. Report a failure as absence and
  `load_or_create_device_sign_key` (`../mobile/ffi/src/keychain.rs:398-406`) and
  the device-identity loader take their mint-and-**persist** branch: the phone
  rotates its identity out from under a paired install, its `device_id` stops
  matching the gateway's, push dies, and the never-deleted signing key is gone
  with it. That is precisely the bug `classify_read`
  (`../mobile/ffi/src/keychain.rs:32-53`) exists to prevent on iOS; Android gets
  the same invariant through the throw, not through a second classifier. The
  `SecureStoreError` conversion is what makes it real — a foreign-trait method
  that throws an undeclared type is turned into a Rust `panic!` by uniffi's
  default unexpected-callback-error handler, so `impl
  From<uniffi::UnexpectedUniFFICallbackError>` is mandatory, not tidy.
- **Keystore key parameters: no `setUserAuthenticationRequired`, no
  `setUnlockedDeviceRequired`; fall back when StrongBox is absent.** FCM decrypt
  runs while the device is locked — that is the whole point of a lock-screen
  preview — and either flag makes the push key unreadable exactly then, which by
  the rule above is a *failure*, not an absence. And a phone with no lock screen
  configured has no user authentication to require, so requiring it would make
  pairing impossible on a device that is otherwise fine. StrongBox is a hardware
  nicety; treating its absence as fatal bricks the app on every device without
  it.
- **`android:allowBackup="false"` and `android:directBootAware="false"`, both
  stated explicitly in the manifest.** Auto Backup would restore the ciphertext
  files under `secure/` to a new device, but Keystore keys never transfer — so
  every read on the restored device fails, and by the null-only-for-not-found
  rule a failure is never absence, which means the install is *permanently
  wedged* with no self-heal. Even with backup off, define the recovery path and
  run it at startup: **ciphertext file present + key alias absent → log it, wipe
  `secure/`, land on Landing.** `directBootAware="false"` is the twin of iOS's
  `kSecAttrAccessibleAfterFirstUnlock`: before the first unlock after boot the
  credential-encrypted store is not available, and a component that runs anyway
  reads nothing and calls it absence.
- **The persisted JSON field names are the on-disk byte format, shared with
  iOS.** `PairedRecord` (`device_id`, `auth_token`, `gateway_static_pubkey`,
  `relay_node_id`, `relay_url`, `remote_api_key`, `noise_secret`,
  `noise_public`) and `DirectCredentials` (`base_url`, `token`, `server_key` —
  `../mobile/ffi/src/direct/mod.rs:53-58`). Renaming one is not a refactor; it
  silently loses the gateway binding of every install that upgrades. The
  golden-JSON tests are what stands in the way, and they assert the literal
  text, not a round-trip: `GOLDEN_RECORD_JSON` at
  `../mobile/ffi/src/relay/pairing.rs:631` with its suite at
  `../mobile/ffi/src/relay/pairing.rs:662-775`, and `GOLDEN_CREDENTIALS_JSON` at
  `../mobile/ffi/src/direct/mod.rs:567` with its suite at
  `../mobile/ffi/src/direct/mod.rs:616-661`. The 32-byte keys are JSON **arrays of
  numbers**; "cleaning them up" into hex reads back as a type error on every
  existing install. `server_key` is required — a record without it is signed out
  and must log in again rather than entering a non-canonical cache namespace.
- **The cache namespace is `filesDir/baybo/servers/gateway-<gateway-static-public-key>/`**,
  with the same children the iOS shell writes under `Application
  Support/baybo/servers/…`. The key comes from Rust — `active_server_cache_key()`
  (`../mobile/ffi/src/lib.rs:66`) over `gateway_key()`
  (`../mobile/ffi/src/server_cache.rs:19-21`), which is `"gateway-"` plus the hex
  gateway Noise static public key; relay and direct bindings of one gateway
  resolve to the same string (`../mobile/ffi/src/server_cache.rs:29-32`). Kotlin
  only joins the path, and applies the same component guard iOS does
  (`../ios/App/Core/ServerCache.swift:52-57`: non-empty, ≤128 chars, lowercase
  hex and `-` only). `None`, or a key that fails the guard, falls back to the
  literal `unbound` (`../ios/App/Core/ServerCache.swift:5,28`). **Logout unloads
  the namespace but never deletes it**; binding the same gateway restores it.
  The children, as the Swift shell writes them today:

  ```
  blobs/                                  ServerCache.swift:36-50
  sessions.json                           SessionIndex.swift:203
  session-mutations.json                  SessionIndex.swift:204
  transcripts/<sessionId>.json            SessionIndex.swift:1240,1250
  outbox/<sessionId>.json                 OutboxStore.swift:239,246
  drafts/<id>/                            ComposerHost.swift:17, DraftStore.swift:189
  card-drafts/<id>/                       ComposerHost.swift:18, DraftStore.swift:189
  models.json                             ModelCatalog.swift:39
  deck.json                               DeckStore.swift:230
  deck-bundles/<cardId>.json              DeckStore.swift:857,861
  projects.json                           ProjectsStore.swift:171
  board-<projectId>.json                  ProjectsStore.swift:118
  issue-<projectId>-<number>.json         IssueStore.swift:238
  issue-comment-outbox/<p>-<n>.json       IssueCommentOutbox.swift:111,124
  project-recency.json                    ProjectRecency.swift:9
  ```

  The MVP writes only the first six; the rest arrive with the features that own
  them (P5). A session id is never trusted as a path component on either shell —
  iOS replaces `/` with `_` (`SessionIndex.swift:1246-1248`,
  `OutboxStore.swift:245`) and Kotlin must sanitize identically, or the same
  session addresses two different files across platforms.
- **`log_dir = filesDir/baybo/logs`** — beside `servers/`, not inside it, so a
  logout or a gateway switch never takes the logs with it (iOS puts them in
  `Library/Logs`: `../ios/App/Core/Baybo.swift:46-53`). The format is fixed,
  because an exported log bundle has to stay comparable across builds and across
  platforms: `baybo.log`, rotated at **2 MiB** into `baybo.log.1` and
  `baybo.log.2` and no further (`../mobile/ffi/src/logging.rs:19-22`, rotation at
  `:50-63`) — three files, 6 MiB ceiling. Levels are Warn globally and Debug for
  `baybo_ffi` (`../mobile/ffi/src/logging.rs:25`). Rust owns the writer; the shell
  contributes one FileProvider path and one Settings row that fires
  `ACTION_SEND`, or the first device bug report arrives without logs.
- **FCM messages are data-only and carry exactly `enc`, `n`, `bid`.** No
  `notification` block — one would make the system render the *undecrypted*
  payload on the lock screen, which is the entire threat model inverted. Those
  three keys are the same ones the iOS NSE reads
  (`../ios/NotificationExtension/NotificationService.swift:117-119`).
  **Decryption happens in-process through the core** (`decrypt_push_preview`,
  P2), never a second ChaCha20-Poly1305 written in Kotlin: a second
  implementation of an AEAD is a second place to get a nonce wrong, and the push
  key would have to cross into Kotlin to feed it. The decrypted preview is
  `{title, body, session_id?, badge?}` with `badge` tolerated when malformed
  (`../ios/NotificationExtension/NotificationService.swift:78-93`).
  **Tap routing uses the same intent-extra key string iOS uses:
  `baybo.sessionId`** (`../ios/NotificationExtension/PushPayloadKeys.swift:9`) —
  one string, one fixture, one behavior to verify on both shells.
- **`versionCode` is derived from semver.** Never the iOS `YYYYMMDDHHMM` build
  stamp (`../ios/scripts/release.mjs:32`): Android's `versionCode` ceiling is
  2100000000 and a twelve-digit stamp is roughly a hundred times past it, so the
  first upload that used one would be the last one that could ever be
  superseded. `versionCode` only ever increases; a decrease is unpublishable.
- **A foreign-trait implementor must not be named `<Trait>Impl`.** UniFFI's
  Kotlin generator emits an `open class <Trait>Impl` in the bindings package for
  every `#[uniffi::export(with_foreign)]` trait, so a Kotlin class of that name
  collides. The traits this applies to today are `BlobProgress`, `FrameSink`,
  `SessionListSink`, `DeckSink`, `PairAbortListener`, `ProjectSink`
  (`../mobile/ffi/src/api.rs:629-738`) and, from P2, `SecureStore`. Swift hit this
  first and dodges it by name — `SessionActivityHandler`
  (`../ios/App/Core/SessionIndex.swift:1167-1171`), and the warning is written
  into the Rust doc comment at `../mobile/ffi/src/api.rs:707-709`. Name the Kotlin
  side for what it does (`KeystoreSecureStore`, not `SecureStoreImpl`).

## Docs

**None of these exist yet** — each is written in the phase that first needs it.
The rule they inherit from [`../ios/CLAUDE.md`](../ios/CLAUDE.md) is: read the
doc for the area you are about to change, before you change it. The plan adds a
second half — *update that doc in the same PR*.

- `docs/build.md` — the cargo-ndk → `jniLibs` → uniffi-bindgen → Gradle order,
  the ABI default and `--all-abis`, the `buildRustCore` input/output declaration
  and its `RUSTC_WRAPPER=` reset, and the 16 KiB alignment assertion run on the
  **packaged APK** (JNA's `libjnidispatch.so` has to be aligned too).
- `docs/testing.md` — the JVM and instrumented tiers, what `android-build`
  gates, and the owner-run device checklist for everything the non-gating
  emulator job cannot be an exit criterion for.
- `docs/design-system.md` — the Compose half of the monochrome soft-line system,
  and which tokens are shared with `web/src/styles.css` rather than restated.
- `docs/navigation.md` — the single-Activity NavHost, predictive back, and
  edge-to-edge insets.
- `docs/transcript.md` — `TranscriptHost` / `TranscriptAssets` /
  `TranscriptBridge`, the `https://appassets.androidplatform.net/transcript/`
  origin and why it is not `file://`, the `WebMessageListener` +
  `isMainFrame` gate (**never `addJavascriptInterface`**), the keyboard inset
  stream, `onRenderProcessGone` and its recreate budget, and the media seam.
- `docs/connection.md` — the two cold-start entry points, what only a real cold
  start may rerun, and the one deliberate divergence from iOS: reconnect is
  gated on foreground state, because the process lives for hours where iOS is
  frozen.
- `docs/sync-and-outbox.md` — the Kotlin side of sync-protocol v2 and the
  outbox's write-before-send order. Read
  [`../../docs/sync-protocol.md`](../../docs/sync-protocol.md) first.
- `docs/push.md` — the FCM realities that are not optional to know: after a
  force-stop nothing is delivered until the next manual launch (common on CN
  OEMs, and the README targets CN users), `onDeletedMessages` means mark every
  session for resync, a denied notification permission makes FCM demote
  high-priority messages so the token must not be posted while it is denied, and
  a GMS-less device degrades to "no push" without crashing.

## Known gaps / follow-ups

- **The shell does not exist.** Every path above is planned. Until P1 moves the
  shared halves to `app/mobile/`, the ffi crate is `app/mobile/ffi` and this
  document's citations point there.
- **The non-iOS `keychain.rs` stub is live and silently succeeds**
  (`../mobile/ffi/src/keychain.rs:265-286`): every store returns `Ok(())` and every
  read `Ok(None)`. Cross-compiled for Android as-is, pairing "succeeds",
  nothing reads back, and the identity is re-minted on every call. The guard is
  a `#[cfg(target_os = "android")] compile_error!` landed right after the P0
  spike, so no `.so` carrying it can be built; P2 deletes both the stub and the
  guard.
- **Decisions still open** (the plan's § *Decisions for the owner*, unchanged):
  direct-leg TLS — option B (public roots only, no JNI) for the MVP means a
  self-hosted gateway behind a private CA connects on iOS and **not** on
  Android; distribution (sideloaded APK vs Play, which decides APK-vs-AAB, the
  signing model, and a stack of Play paperwork); the `versionName` source, of
  which there are four candidates today; whether the strings catalog moves to
  `app/mobile/strings/` or stays under `app/ios` and joins `ANDROID_DEPS`;
  `minSdk 28`; whether the transcript honors the system font scale on either
  shell; and the QR library (ML Kit depends on Play Services, which contradicts
  "GMS-less devices must still work").
- **WebView floor: Chromium ≥ 111**, checked at startup with an "update Android
  System WebView" screen below it. The bundle's CSS uses `color-mix()` (111),
  `:has()` (105) and `dvh`/`svh` (108) — all three in `web/src/styles.css` —
  and `vite build.target: es2021` (`../mobile/web/vite.config.ts:13`) transpiles
  JS only, never CSS. 111 is the binding constraint, and it comes from
  `color-mix()`.
- **The notification channel ids are not chosen.** They are frozen the moment
  they ship (see the contract), and the plan does not name them. Choose them in
  P3 and record them here in the same PR.
- **FCM's `collapse_key` limit is unverified.** The gateway's key is a 32-hex
  truncated SHA-256 of `device_id:session_id`
  (`crates/gateway/src/push/mod.rs`), i.e. one per session per device. If FCM
  really caps distinct keys per device at four, that coalesces across sessions
  for any user with more than four; Android may need a fixed per-device key plus
  an in-app notification id. Settle it before writing `fcm.rs`.
- **`panic = "abort"` must not reach the shared `[profile.release]`.** iOS
  release builds unwind, and uniffi's `rust_call` turns a Rust panic into an FFI
  error through `catch_unwind`; abort would kill the iOS app instead. If Android
  wants it, it goes in a separate `[profile.android-release]` selected only by
  `scripts/build-core.sh --profile android-release`.
- **`app/ios/CLAUDE.md`'s last contract bullet is stale** and should be fixed on
  the way past: it says the APNs environment reaches Rust through
  `ClientConfig.apnsEnv`, but `ClientConfig` is `{log_dir, blob_cache_dir}`
  (`../mobile/ffi/src/api.rs:107-117`) and the environment rides on
  `PushToken::Apns { environment }`.
