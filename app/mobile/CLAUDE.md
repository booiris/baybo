# Baybo mobile core

The half of the phone apps that is not a shell: the UniFFI Rust core both
`app/ios` and `app/android` link, the `uniffi-bindgen` CLI that emits their
bindings, and the Vite/React bundle both render in a WebView. The protocol and
crypto stay in the shared crates (`crates/wire`, `crates/device-proto`), so
interop with the gateway is guaranteed by construction rather than by testing.

Design docs: [`docs/modules/mobile/core.md`](../../docs/modules/mobile/core.md)
owns the core's architecture and the rule for what belongs here versus in a
shell; [`docs/modules/mobile/companion.md`](../../docs/modules/mobile/companion.md)
owns the pairing/push wiring. The relocation that created this directory, and
the phases still ahead of it, are
[`docs/todo/android-companion.md`](../../docs/todo/android-companion.md).

## Layout

```
Cargo.toml / Cargo.lock  — its OWN cargo workspace (the root one does not
                           include it), members = ["bindgen", "ffi"]
ffi/                     — the UniFFI core: transport legs, pairing, secure
                           store, blobs. Exports BayboClient + parse_pair_qr.
bindgen/                 — the uniffi-bindgen CLI, a separate member so the
                           `cli` feature never unifies into the lib build
web/                     — the transcript / deck / issue bundle, its OWN pnpm
                           project (`packages: []` stops it adopting the root
                           workspace)
scripts/                 — build-core.sh, sync-web.sh: the cargo+bindgen and
                           pnpm+copy steps both shells call
target/                  — build products (gitignored)
```

Build **outputs** do not live here. `Generated/BayboCore.swift`,
`Externals/BayboCore.xcframework` and `App/Resources/transcript/` are written
into `app/ios/`, and the Android equivalents into `app/android/`; each shell's
own `scripts/build-core.sh` wraps the shared one and adds its platform's
packaging step.

## Build & test

```bash
cargo nextest run --workspace                            # core host tests
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all --check
(cd web && pnpm install --frozen-lockfile && pnpm lint && pnpm test && pnpm build)
```

Run them from `app/mobile`. This is a separate workspace from the root, so the
root `cargo test --workspace` and the root `frontend` job cover none of it; CI
reaches it through the two Linux jobs behind the `ios` path filter (and, once
P3 lands, the `android-build` job behind `ANDROID_DEPS`). `pnpm build` is
`tsc --noEmit && vite build`, and the typecheck is the point: it is what
evaluates the three drift sentinels, which pin the bundle's types to
`sidecars/sdk/channel-ts`'s generated wire types and to `app/web`'s generated
OpenAPI schema by relative path. **Those relative paths are why this directory
sits at `app/mobile`, the same depth as `app/ios`** — one level deeper or
shallower and every sentinel import, plus the four `../../crates/*` path
dependencies in `Cargo.toml`, resolves somewhere else.

## Frozen — changing one of these breaks a shipped install

The iOS continuity contract in [`../ios/CLAUDE.md`](../ios/CLAUDE.md) is the
authority and explains each consequence; this is the subset that lives in *this*
directory, so it is what a change here can break. `app/android/CLAUDE.md` holds
the Android twin.

- **`[lib] name = "baybo_ffi"`** in `ffi/Cargo.toml`. The package name is free
  (it was renamed to `baybo-mobile-ffi` when this directory was created); the
  lib name is not. It is the uniffi namespace, the `uniffi_baybo_ffi_*` /
  `ffi_baybo_ffi_*` C symbol prefix, the `libbaybo_ffi.{a,dylib,so}` artefact
  names the build scripts and the xcframework reference, the `DEBUG_TARGETS`
  filter in `ffi/src/logging.rs`, and the name Kotlin's JNA resolves at
  `Native.load("baybo_ffi")`.
- **`[bindings.swift] module_name = "BayboCore"`** in `ffi/uniffi.toml`. The
  generated `BayboCoreFFI.h` and `BayboCoreFFI.modulemap` are named from it, and
  every Swift file in the shell imports it.
- **The keychain account literals and the iOS `mod imp`** in
  `ffi/src/keychain.rs`. Account names, the absent `kSecAttrService`, the access
  group and the accessibility class are the inputs to keychain item *identity*:
  change one and an existing install stops finding its own items.
- **The `PairedRecord` / `DirectCredentials` serde field names.** They are the
  on-keychain byte format shared with every shipped install. The golden-JSON
  tests in `ffi/src/relay/pairing.rs` and `ffi/src/direct/mod.rs` assert the
  literal text, not a round-trip, and are what stands in the way of a rename.
- **The server-cache key scheme** in `ffi/src/server_cache.rs` — `"gateway-"`
  plus the hex gateway Noise static public key. Both shells join it into a
  durable cache path, and both relay and direct bindings of one gateway must
  resolve to the same string.
- **The `baybo.log` bundle format** in `ffi/src/logging.rs` (2 MiB, two rotated
  files, the line format). An exported log bundle has to stay comparable across
  builds and across platforms.
- **The JS↔native bridge message shapes** in `web/src/bridge.ts`,
  `web/src/deck/bridge.ts` and `web/src/issue/bridge.ts`. Two native hosts parse
  them; the transport underneath is per-platform, the shapes are not.

One more thing lives outside this tree and breaks if the bundle moves or its
files are renamed: `crates/deck/src/render.rs` pulls `web/src/deck/sdkCard.js`
into the gateway binary with `include_str!`, deliberately, so the deck render
gate can never check cards against an SDK the client no longer ships. Editing
that file is a gateway change.
