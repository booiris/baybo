# Mobile shared core

> **Status:** P0 skeleton, written before any of the code it describes.
> The sections, the binding rules, and the platform matrix are settled here;
> the sections marked **stub** carry a one-line note about what fills them and
> are written out in P2 as the seams land. The plan this implements is
> [`../../todo/android-companion.md`](../../todo/android-companion.md). Where
> that plan and the code disagree, the code wins and this file records the
> code. Paths are written as they are **today** (`app/mobile/ffi/…`); P1 moves the
> shared halves to `app/mobile/ffi/…` without changing a line of what is
> described below.

The shared mobile core is the Rust half of the phone app: the two chat
transport legs (relay Noise E2E and direct raw-msgpack), scan-to-pair, secure
storage for the device identity and both credential sets, blob upload and
download, and the push-token state machine — all exported over UniFFI to two
shells, the SwiftUI one in `app/ios/App/` and the Kotlin/Compose one in
`app/android/` (P3). One `BayboClient` instance owns the live legs, the
in-flight pairing sessions, and the push state, and the pumps it spawns keep
running between calls (`app/mobile/ffi/src/lib.rs:70-91`).

It is **not** where the protocol lives. Frames, the `Noise_XXpsk0` pairing
handshake, the Ed25519 push delegation and the preview AEAD are in
`crates/wire` and `crates/device-proto` — the same crates the gateway
compiles. Interop is therefore guaranteed by construction rather than by a
second implementation kept in sync by hand, and that is why an Android shell
needs no new crypto: `decrypt_push_preview` goes through
`device_proto::aead::open`, never a second ChaCha20-Poly1305 written in
Kotlin.

**It is its own cargo workspace** (`app/mobile/Cargo.toml:10-12`), and stays one
after the move. The header comment states the first reason — the root
`cargo clippy --all --all-features` gate must never build a phone-configured
cdylib, and uniffi's `cli` feature must never unify into the lib build, which
is why `bindgen/` is a separate member (`app/mobile/Cargo.toml:1-9`). The reason
with the sharpest teeth is TLS feature unification. The core pins reqwest with
`default-features = false` plus `rustls-no-provider`
(`app/mobile/Cargo.toml:32-36`) and rustls with exactly one provider, `ring`
(`:43`), and installs it once in the constructor (`lib.rs:1623-1626`); the
root workspace pins reqwest with its defaults plus `json`/`socks`
(`Cargo.toml:202`) and rustls under `aws_lc_rs` (`Cargo.toml:208`). The
*versions* already match — it is Cargo's per-crate feature unification that
would break, because one workspace means one feature set per crate per build,
and the phone would silently start linking `aws-lc-rs`.

Not `crates/mobile-core/` for the same reason. Not `app/core/`, because "core"
already means the gateway core, the `baybo_ffi::core` module, and the Swift
`BayboCore` module; the subdirectory stays `ffi/`.

## The UniFFI surface

Three kinds of thing cross the boundary, and nothing else.

**One object.** `BayboClient` (`lib.rs:70`, `#[derive(uniffi::Object)]`), whose
exported `impl` block (`lib.rs:95`) carries 82 `pub async fn`s plus the
synchronous setters and getters — pairing, direct login, the chat leg, session
list, deck, projects, blobs, models. `gateway_client()` is deliberately
outside that block (`lib.rs:83-93`): uniffi would try to lower its return type
over the FFI.

**Free functions.** `parse_pair_qr` (`lib.rs:53-56`), `new_chat_session_id`
(`:58-62`), and `active_server_cache_key` (`:65-68`) — the last of which is
the only place the durable cache namespace `gateway-<hex(static pubkey)>` is
computed (`server_cache.rs:19-21`); a shell only joins the path
(`app/ios/App/Core/ServerCache.swift:27-34`).

**Foreign-trait callback interfaces** — `#[uniffi::export(with_foreign)]`, the
core calling *out* on its own tokio workers, so every shell implementation
must be thread-safe and hop to its UI thread before touching UI:

| Trait | `api.rs` | What it carries |
|---|---|---|
| `BlobProgress` | 629 | Byte progress for one `blob_download_bytes`, rate-limited by the core so a 100 MiB download does not cross the FFI thousands of times |
| `FrameSink` | 642 | One subscribed session's inbound `wire::Frame` as JSON, plus unsolicited leg death |
| `SessionListSink` | 663 | Connection-global session pings: activity, title, approval-pending, and the `on_list_stale` refetch nudge |
| `DeckSink` | 710 | Session-less deck broadcasts — card data and structure changes |
| `PairAbortListener` | 724 | Gateway-side cancellation of an in-flight pairing while the confirm screen is up |
| `ProjectSink` | 731 | Session-less board invalidations and the gap-driven refetch |

Android adds exactly one: `SecureStore` (next section but one). Note the
naming trap already recorded at `api.rs:708-710` — uniffi generates a class
named `<Trait>Impl` for every `with_foreign` trait, so a shell implementor may
not take that name. It bites Kotlin the same way it bites Swift.

**Bindings for both languages come from ONE host cdylib.**
`scripts/build-core.sh:47-55` builds `libbaybo_ffi.dylib` for the *host* and
runs `uniffi-bindgen generate --library target/$PROFILE/libbaybo_ffi.dylib
--language swift`; P2 adds `--language kotlin` against the same artefact, so
generating Kotlin needs no NDK. That has a consequence with teeth: the
exported signature **cannot be `cfg`-split per platform**. A signature that
differs between the iOS build and the host build would generate bindings that
disagree with the library the shell actually links. Everything the platform
matrix below marks as differing does so *inside* a function body, never in its
signature.

`[lib] name = "baybo_ffi"` (`ffi/Cargo.toml:12-14`) is frozen for practical
reasons: it is the uniffi namespace, the `uniffi_baybo_ffi_*` / `ffi_baybo_ffi_*`
C symbol prefix, the `libbaybo_ffi.{a,dylib,so}` artefact name, the
`DEBUG_TARGETS` filter string in `logging.rs:25`, and the name Kotlin's JNA
`Native.load("baybo_ffi")` resolves. The *package* name may change; the lib
name buys nothing by changing.

## ClientConfig and construction

Today (`api.rs:107-117`):

```rust
pub struct ClientConfig {
    pub log_dir: Option<String>,
    pub blob_cache_dir: Option<String>,
}
```

and `#[uniffi::constructor] pub fn new(config: ClientConfig) -> Arc<Self>`
(`lib.rs:100-118`) — infallible, and it does four things in order: install the
rustls `ring` provider (without it the first `wss://` dial panics), install
the log bridge, seed the blob cache dir, and on an iOS debug build seed the
NSE self-check push key (`lib.rs:108`).

P2 changes two things.

**`blob_cache_dir` becomes `String` (required).** The `Option` today buys a
`std::env::temp_dir()` fallback (`blob_helper.rs:74`), which iOS purges under
storage pressure — wrong for a file the user asked to keep. That fallback
leaves non-test builds with the field.

**The secure store becomes a constructor argument:**

```rust
#[uniffi::constructor]
pub fn new(
    config: ClientConfig,
    secure_store: Option<Arc<dyn SecureStore>>,
) -> Result<Arc<Self>, BayboError>
```

On a non-iOS target `None` fails construction. Swift passes `nil` and gains a
`try` — one line in `App/Core/Baybo.swift:51-54`.

**That `Option` is forced by the bindings, not by convenience, and is
therefore not the shape the root `CLAUDE.md` forbids.** The rule there is
"don't make a field `Option<T>` to accommodate tests" — a field that is
`Option` only so a fixture can skip wiring it. This one is `Option` because
`generate --library` emits *one* surface for *both* shells from a host build
(see above): a `cfg`-split signature would generate bindings that disagree
with the linked library. iOS legitimately has no store to pass, because its
implementation is compiled into the core.

**A second constructor may not be named `with_secure_store`.** In this repo
`with_*` means a genuine config knob with a real default that some callers
rationally leave alone. The store is a required dependency on Android; naming
it `with_*` would tell every future reader the opposite of the truth. If the
one-line Swift edit is unwelcome, the alternative is `new(config)` plus a
second constructor with a non-`with_` name — not a setter.

## The secure-store seam

Six accounts, all `kSecClassGenericPassword` with **no `kSecAttrService`**,
and all frozen by the iOS continuity contract (`app/ios/CLAUDE.md`
§ Continuity contract):

| Account | `keychain.rs` | Holds | iOS class |
|---|---|---|---|
| `baybo.push-key.<bid>` | 93 (`ACCOUNT_PREFIX`) | Per-device push-preview AEAD key | shared group, AfterFirstUnlock |
| `baybo.paired-gateway` | 314 | The relay `PairedRecord` | ThisDeviceOnly |
| `baybo.device-identity` | 338 | Noise static `secret ‖ public` | ThisDeviceOnly |
| `baybo.device-sign-key` | 372 | Ed25519 push-delegation seed (**never deleted**) | ThisDeviceOnly |
| `baybo.direct-credentials` | 413 | `{base_url, token, server_key}` | ThisDeviceOnly |
| `baybo.active-binding` | 437 | `"direct"` / `"relay"` tie-breaker | ThisDeviceOnly |

Two implementations behind one module-level API (`keychain.rs:288-460`, whose
`pub fn` signatures do not change):

- **iOS**: `#[cfg(target_os = "ios")] mod imp` (`keychain.rs:56-263`) calls
  `SecItemAdd` / `SecItemCopyMatching` / `SecItemDelete` from Rust over
  hand-declared Security-framework symbols. Its body is frozen — every input
  to item *identity* (account names, the absent service attribute, the access
  group, the accessibility class) is what an already-shipped install uses to
  find its own items. The single deliberate edit is hoisting `ACCOUNT_PREFIX`
  from `imp` (`:93`) to module level so the non-iOS `imp` can use it; it
  changes no identity input.
- **Android**: a `#[uniffi::export(with_foreign)] trait SecureStore` with
  `get`/`put`/`delete` over `Vec<u8>`, implemented in Kotlin over one AES-GCM
  key in `AndroidKeyStore` wrapping a ciphertext file per key. Every non-iOS
  target — the ubuntu host included — routes through it, so the paths Android
  actually runs are exercised by the existing `ios-core` nextest job via an
  in-memory `#[cfg(test)]` store.

**The silent stub is deleted, not extended.** `#[cfg(not(target_os = "ios"))]
mod imp` today (`keychain.rs:265-286`) returns `Ok(())` from every store and
`Ok(None)` from every read. Cross-compiled for Android as-is, `pair_confirm`
and `direct_login` return `Ok`, `binding::active_leg()` then reads nothing
back, every later call fails with `BayboError::NotBound`, and the device
identity and push key are re-minted on each call. A
`#[cfg(target_os = "android")] compile_error!` lands next to it right after
the P0 spike so no `.so` carrying it can ever be built.

**Absence and failure must never collapse into each other.** This is the
invariant `classify_read` exists for (`keychain.rs:32-53`): `Ok(None)` is what
sends `load_or_create_device_sign_key` and the device-identity loader down
their mint-and-PERSIST branch. Report a transient failure as absence and the
app quietly mints a fresh identity over the stored one — the phone's
`device_id` and its `baybo.push-key.<id>` entry stop matching the gateway's,
push dies, and the "never deleted" device key is gone with no error anyone
sees. The two host tests at `keychain.rs:473-494` are what stands in the way.
The Kotlin store owes the same distinction: `get` returns null **only** for
not-found, and throws on a Keystore failure or a corrupt file.

**The error type is not optional.** A foreign-trait method that throws
anything other than its declared error type reaches
`handle_callback_unexpected_error`, whose default implementation in uniffi_core
0.29.5 is `panic!("Callback interface failure: {e}")`
(`~/.cargo/registry/src/*/uniffi_core-0.29.5/src/ffi_converter_traits.rs:396-398`).
So `#[derive(uniffi::Error)] enum SecureStoreError { Failed { reason } }` with
`impl From<uniffi::UnexpectedUniFFICallbackError>` is **mandatory**: the exact
behaviour the Kotlin store must have — throwing on `KeyStoreException` — is
otherwise a Rust panic. The absence-vs-failure invariant is only as good as
that conversion.

**The account → storage-key derivation lives in Rust**, in a new
`ffi/src/secure_store.rs` alongside the `OnceLock<Arc<dyn SecureStore>>`.
Kotlin receives an opaque, filesystem-safe key and stores the bytes verbatim.
One home for the rule, and a host-testable one — the alternative puts the
naming rule in Kotlin where no Rust test can reach it.

> **Stub (P2).** The Kotlin-side contract — Keystore key parameters
> (no `setUserAuthenticationRequired`, no `setUnlockedDeviceRequired`, because
> FCM decrypt runs while locked), `android:allowBackup="false"` and the
> "ciphertext restored without its key alias" recovery path — belongs in
> `app/android/CLAUDE.md`, written in P0. This section gains the Rust-side
> derivation rules and the in-memory test store when they land.

## Logging

`logging.rs` installs a `log` bridge (`:141-149`, idempotent, matching the
process-global `log` facade) that writes a rotating on-device file:
`<log_dir>/baybo.log` (`:19`), 2 MiB per file (`:20`), two rotated siblings
kept as `baybo.log.1` / `baybo.log.2` (`:22`), shifted oldest-out on overflow
(`:51-63`). Each line is
`YYYY-MM-DDTHH:MM:SSZ [LEVEL][target] message` (`:83-89`), the timestamp being
seconds-precision UTC computed in-crate (`:117-137`) rather than pulling a
time crate into the phone. Levels: Warn globally, Debug for `baybo_ffi` and
`baybo_ffi::core` (`:24-25`, `:67-77`).

**The file format is fixed on purpose.** The bundle is what a user exports and
attaches to a bug report, and the whole point of exporting it is to line a
session up against gateway and relay logs. Change the field order or the
timestamp shape and two exports from two builds stop being comparable — which
is the one thing the artefact exists to be.

One platform difference, and it is inside the body, not the signature:
`logging.rs:90` mirrors every line to stderr with `eprint!`, which is where
Xcode's console reads it on a debug run and where the host tests read it. On
Android native fd 2 is `/dev/null` under the zygote, so that branch becomes
`__android_log_write` under `cfg(target_os = "android")`. Not the
`android_logger` crate: it installs its own global logger and conflicts with
the `set_boxed_logger` call at `logging.rs:146`.

Android's `log_dir` is `filesDir/baybo/logs`, and it needs a FileProvider path
plus a Settings row firing `ACTION_SEND` — or the first device bug report
arrives without logs.

## Direct-leg TLS

**The problem.** reqwest 0.13's `rustls-no-provider` feature — the one the
core pins (`app/mobile/Cargo.toml:32-36`) — hard-depends on
`rustls-platform-verifier`; `app/mobile/Cargo.lock:1586-1589` already resolves
`rustls-platform-verifier-android 0.1.1` in the graph. On Android that crate
switches to a JNI-backed verifier that needs `android::init_with_env` called
from Kotlin plus a bundled Kotlin component. Without it,
`Client::builder().build()` **succeeds** and the *first TLS handshake* panics
on an `.expect(...)` — a failure that appears nowhere until a user tries to
connect.

**The decision (MVP): preconfigured rustls over webpki-roots.** A new
`ffi/src/tls.rs` exposes `http_client_builder()`, consumed by the one
production reqwest construction site, `DirectHttpCache::new`
(`direct/mod.rs:364`). The `reqwest::Client::new()` at `direct/blob.rs:343` is
inside `#[cfg(test)] mod tests` (`direct/blob.rs:213`) and stays as it is.
Under `cfg(target_os = "android")` the builder calls
`ClientBuilder::tls_backend_preconfigured` with a `rustls::ClientConfig` built
over `webpki-roots`; that method sets `TlsBackend::BuiltRustls`
(`~/.cargo/registry/src/*/reqwest-0.13.4/src/async_impl/client.rs:2192-2209`)
and never touches the platform verifier. Use that name and not
`use_preconfigured_tls`, which the same file marks as a deprecated shim
delegating to it (`:2220-2223`). `webpki-roots` becomes a direct dependency.

**The platform consequence, stated plainly.** Android direct mode then trusts
**public roots only**. A self-hosted gateway behind a private CA — a real and
supported iOS configuration, where the system trust store carries the
enterprise root — connects in direct mode on iOS and **not** on Android. That
is a product asymmetry, not an implementation detail, and it stands until the
JNI route lands as the private-CA follow-up.

**The wss legs are unaffected.** Both chat legs dial through
`tokio-tungstenite`, pinned with `default-features = false` plus
`rustls-tls-webpki-roots` (`app/mobile/Cargo.toml:38`): they already carry baked-in
webpki roots and never consult a platform verifier on any target. Only the
direct leg's REST/blob HTTP client goes through reqwest, so only it is in
scope here. The same manifest comment (`:39-43`) records why rustls is pulled
directly with exactly `ring` and installed in the constructor — tokio-tungstenite
selects no provider, so the first `wss://` dial would otherwise panic building
its `ClientConfig`.

## Async and cancellation

Every exported `async fn` becomes a Swift `async` method and a Kotlin
`suspend fun` (kotlinx-coroutines is a runtime dependency of the generated
Kotlin).

**The rule: every exported async body is `runtime::run(fut)`.** All 82 of them
are, today. `runtime::run` (`runtime.rs:32-41`) spawns `fut` onto the owned
multi-thread runtime (`runtime.rs:18-28`, two worker threads) and awaits the
runtime-independent `JoinHandle`; uniffi polls exported async fns on its own
FFI machinery with no ambient tokio context, and the transport uses
`tokio::spawn` / `tokio::time` / `tokio::net` throughout, so this is not
optional plumbing. Detached tasks the transport spawns — the chat pump, the
pairing pump — inherit the runtime through that spawn and keep running after
the exported call returns (`runtime.rs:1-7`).

**The consequence:** cancelling the *caller* drops only the `JoinHandle`, not
the spawned task. The work completes; only the result is lost. Swift's
structured concurrency and Kotlin's coroutine cancellation both cancel the
awaiting side, and neither can reach past the handle.

**So on Android, every client call that mutates durable state runs in an
application-scoped `NonCancellable` scope, never a lifecycle scope.**

The concrete failure this prevents: the user taps send, `chat_send` spawns,
and an Activity recreation (rotation, a locale change via
`setApplicationLocales`, a configuration change) cancels the coroutine that
was awaiting it. The gateway receives the message and echoes it. The shell
never observes the return value, so the outbox entry stays in `sending` until
the echo timeout retires it — a visibly stuck send for a message that was
delivered on the first try. Nothing in the core can detect this; only the
scope choice on the Kotlin side prevents it.

## What lives in the core, and what stays in a shell

**The push-down rule.** A Swift store moves into Rust **iff all three** hold:

1. it is pure data logic — Foundation-only, with no UI-framework import and no
   observation coupling (`@Published` / `ObservableObject`);
2. its on-disk format is pinned, or it is fed by gateway data the core already
   fetches;
3. it has a suite that can become a Rust golden test.

**Why the rule exists, in the root `CLAUDE.md`'s own words:** *"would a second
caller have to copy this? If yes, it belongs in the domain crate now, not
after the second caller shows up."* Android **is** that second caller, and the
failure mode is already in the tree twice — `RenameTitle.swift` is the third
spelling of one validator (`app/web`'s `renameTitle.ts` and the gateway's
`validate_session_title` are the other two, `RenameTitle.swift:3-5`), and
`SearchSnippet.swift` is a second port held to the first only by a shared
vector file (`SearchSnippet.swift:5-11`). A Kotlin port makes each of those a
fourth and a third site. The root rule's other half applies as directly: one
home per rule, predicates included — if two places answer "what will the
gateway store for this title", one of them is already wrong.

The verdicts. LOC is `wc -l` on master:

| Store | LOC | Verdict | Deciding clause |
|---|---|---|---|
| `App/Core/RenameTitle.swift` | 56 | **core** (P2) | all three; already spelled three times |
| `App/Core/SearchSnippet.swift` | 135 | **core** (P2) | all three; the shared vector file is already the contract |
| `App/Core/ChatApprovals.swift` — `ApprovalQueue` only (`:88-135`) | 135 (file) | **core** (P2), the fold only | `ApprovalQueue.apply` is a pure frame fold; `PendingApproval.from` is `@MainActor` for `Lang` (`:27-29`), so the localized access lines stay in the shell — clause (a) splits the file, not the rule |
| `App/Core/OutboxStore.swift` | 272 | **core** (P2) | all three: `import Foundation` only, `Codable` entries (`:24-54`), and `OutboxStoreTests` + `PersistedFormatTests` become the golden vectors |
| `App/Core/ModelCatalog.swift` | 180 | **core** (P2), data half | clause (a) fails for the class — `@MainActor final class … ObservableObject` with `@Published` (`:13-20`); the `Codable` `Mirror` and its merge (`:111-118`) move, the observable wrapper does not |
| `App/Core/SessionIndex.swift` | 1313 | **shell** — Kotlin port (P3) | (a): `@Published rows` / `listStaleEpoch` (`:277`, `:282`) plus the transcript-mirror writer are welded together; retire in P5 only if the two ports actually drift |
| `App/Core/ChatStore.swift` | 1762 | **shell** — Kotlin port (P3) | (a): imports SwiftUI, UIKit and AVFoundation (`:1-4`), owns ~12 `@Published` slots and `TranscriptBridge`, and is pinned by eight `ChatStore*Tests` suites |
| `App/Core/DraftStore.swift` | 226 | **shell** — Kotlin port (P3) | (b): the persisted record embeds an iOS security-scoped `bookmark: Data?` (`:19-22`), so the on-disk format is not portable even though the file is Foundation-only |
| `App/Core/Composer/ComposerStaging.swift` | 523 | **glue** | (a): ImageIO / PhotosUI / SwiftUI (`:1-4`) |
| `App/Core/TranscriptMedia.swift` | 542 | **glue** | (a): AVFoundation / UIKit (`:1-4`); Android's counterpart is Media3 |
| `App/Core/AudioPlayerCenter.swift` | 322 | **glue** | (a): AVFoundation / MediaPlayer (`:1-3`) — a now-playing centre has no portable half |

The P2 push-downs total roughly 800 lines of Swift and are exported as uniffi
objects and free functions; iOS keeps 50-150-line adapters over them. The
literal fixtures in `Tests/PersistedFormatTests.swift` become checked-in JSON,
consumed first by the Rust golden tests and then by the Swift tests through
those adapters — so one file, not two, decides what the bytes are.

The P3 Kotlin ports keep the same JSON keys and the same file layout as iOS,
and the Kotlin `PersistedFormatTest` reads the same fixtures. `AppStore`
(1,759 lines) ports as an MVP subset by the same reasoning as `ChatStore`.

**Two things the core must keep, whatever moves.** The `PairedRecord` /
`DirectCredentials` serde field names **are** the on-keychain byte format,
shared with every already-shipped install; the literal-text golden tests at
`relay/pairing.rs:631` and `direct/mod.rs:567` are what stands in the way of a
rename, and they become the guard for both platforms. And the cache namespace
`baybo/servers/gateway-<hex(static pubkey)>/` is computed in Rust
(`server_cache.rs:19-21`) precisely so no shell re-derives it.

## Platform matrix

| Concern | iOS | Android |
|---|---|---|
| Secure store | Security.framework called from Rust (`keychain.rs:56-263`); items carry no `kSecAttrService` | Kotlin `KeystoreSecureStore` over the `SecureStore` foreign trait; AES-GCM key in `AndroidKeyStore`, one ciphertext file per key under `filesDir/baybo/secure/` |
| Push consumer | Out-of-process Notification Service Extension; the push key crosses a process boundary through the shared access group | In-process `FirebaseMessagingService`; decrypt runs in the app process via the core's `decrypt_push_preview`, key never leaves it. No NSE counterpart is needed |
| Push provider | APNs — `PushToken::Apns { token, environment }` (`api.rs:67-75`), env from the `BayboApnsEnvironment` Info key (`App/Core/Baybo.swift:20-34`, `App/AppDelegate.swift:61-63`) | FCM — `PushToken::Fcm { token }`, `environment = None` on the wire; data-only messages carrying exactly `enc` / `n` / `bid` |
| WebView host and origin | WKWebView, custom scheme `baybo-transcript://localhost/` (`App/Web/TranscriptSchemeHandler.swift:16-17`) | Android WebView, `https://appassets.androidplatform.net/transcript/` through `shouldInterceptRequest` — a secure standard origin, for the same reason iOS uses a scheme handler rather than `file://` |
| JS → native transport | `window.webkit.messageHandlers.baybo` (`web/src/bridge.ts:149`), gated on `frameInfo.isMainFrame` | `WebViewCompat.addWebMessageListener("bayboHost", allowedOrigins)`, gated on `isMainFrame`. **Never `addJavascriptInterface`** — it is injected into every frame with no frame info, which would hand the agent-authored html-preview and deck-card iframes a direct line to native |
| Foreground signal | `scenePhase`; `.background` calls `relayInvalidateApiLegs()` synchronously (`App/BayboApp.swift:41-42`) | `ProcessLifecycleOwner`: `ON_STOP` maps to that same call and must stay synchronous; `ON_RESUME` reconnects |
| Background / process death | The process is frozen in the background, so `ChatStore`'s `legLost → scheduleRetry` ladder never runs there; jetsam is rare | The process lives for hours, so the pump's 45 s inbound-liveness watchdog (`transport/pump.rs:28`) fires under Doze and a naive port redials all night — reconnect is gated on foreground state. Process death is far more frequent; every cold start reruns the full launch sequence, an Activity recreation must not |
| Direct-leg TLS trust | reqwest's default path (`direct/mod.rs:364`), platform trust store — a private CA works | Preconfigured rustls over webpki-roots; **public roots only**, private CA does not connect until the JNI route lands |
| Log mirror | `eprint!` to stderr (`logging.rs:90`), read by Xcode's console | `__android_log_write` under `cfg(target_os = "android")`; native fd 2 is `/dev/null` under the zygote |
| Bindings language | Swift, module `BayboCore` (`ffi/uniffi.toml:1-3`) | Kotlin, package `com.baybo.core`, `android = true`; a `with_foreign` trait becomes an `interface` plus a generated `XImpl` class |
| Build artefact | `BayboCore.xcframework` — device + simulator `libbaybo_ffi.a` with headers (`scripts/build-core.sh:62-66`), codesigned for device builds | `libbaybo_ffi.so` per ABI in `jniLibs`, loaded through JNA (`net.java.dev.jna:jna@aar`); 16 KiB alignment asserted on the packaged APK, not on the cargo output |
| Panic behaviour | Unwinds. `app/mobile/Cargo.toml` has **no** `[profile.release]`, so uniffi's `rust_call` turns a Rust panic into an FFI error through `catch_unwind` (`uniffi_core-0.29.5/src/ffi/rustcalls.rs:177`) | Unwinds today. If Android wants `panic = "abort"` it goes in a separate `[profile.android-release]` selected only by the Android build script — never in `[profile.release]`, which would make an iOS panic kill the app instead of surfacing as an error |

## Sections to be filled in P2

Two bodies of design currently live under `app/ios/docs/` and are
platform-neutral in substance. They move here in P2; the iOS docs keep their
"The Swift half" sections and cross-link.

- **Connection lifecycle** — one supervisor task per leg registry owns every
  connection-lifecycle decision, reached only through one unbounded message
  queue; the `LegDialer` / `FrameCodec` / `SessionLeg` seams
  (`ffi/src/transport/mod.rs:149`, `:184`, `:413`) are what keep relay and
  direct sharing that loop without sharing their protocols. Lives today at
  [`app/ios/docs/connection.md`](../../../app/ios/docs/connection.md), whose
  `## The shape` (`:17`), `## The invariants (each one is a scar)` (`:53`) and
  `## What stays OUTSIDE the supervisor` (`:129`) are the halves that move;
  `## The Swift half` (`:152`) stays. Nearly every sentence is scar tissue
  from the 2026-08-16 cold-start send black hole — sends returned `Ok` into a
  leg the gateway had silently dropped — so it moves verbatim, not rewritten.

- **Sync-v2 client loop and outbox semantics** — the single sync loop that
  replaced the seven-cell hydration matrix (REPAIR / REPLACE / APPEND and the
  rule that every REPLACE keeps the rows it predates), and the persisted send
  outbox with its two-stage echo-then-`sent` confirmation and its 10 s / 3 tx
  / 30 s rules. Lives today at
  [`app/ios/docs/sync-and-outbox.md`](../../../app/ios/docs/sync-and-outbox.md)
  `## Transcript sync (sync-protocol v2)` (`:11-35`) and
  `## Send outbox (sync-v2)` (`:463-571`). Read
  [`docs/sync-protocol.md`](../../sync-protocol.md) first — this is the
  client-side companion to it, not a replacement. The outbox half arrives here
  with the P2 push-down of `OutboxStore`, so the doc and the code move
  together.

A third lift — the native ⇄ web bridge vocabulary
(`app/ios/docs/transcript.md:250-275`) — goes to
[`web-bundle.md`](web-bundle.md), not here: it is the JS contract, and the
core never sees it.
