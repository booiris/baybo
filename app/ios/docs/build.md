# Building the iOS app

*How the app is built and installed — governs `app/ios/scripts/build-app.sh`, `app/ios/scripts/build-core.sh`, `app/ios/scripts/install.mjs`, `app/ios/scripts/release.mjs`, `app/ios/scripts/ExportOptions-AppStore.plist`, `app/ios/project.yml`, and the build products under `app/ios/Generated/` and `app/ios/Externals/`.*

```bash
scripts/build-app.sh             # web → rust xcframework → xcodegen → sim build
scripts/build-app.sh --device --release
node scripts/install.mjs         # archive + export + devicectl install (USB)
node scripts/release.mjs --version 0.1.0 --build-number 1
node scripts/release.mjs --version 0.1.0 --build-number 1 --upload
cargo clippy --workspace --all-targets --all-features   # zero warnings
cargo nextest run --workspace    # ffi host tests
(cd web && pnpm build)           # tsc --noEmit + vite build
```

`Debug` versus `Release` controls optimization, not the APNs service. Local
device builds and `install.mjs` are development-signed, so both use the default
`BAYBO_APNS_ENVIRONMENT=development` and register sandbox tokens. `Distribution`
inherits Release optimization but overrides that setting to `production`; the
Baybo scheme's Archive action uses `Distribution`. The setting feeds both the
signed `aps-environment` entitlement and the app's runtime
`BayboApnsEnvironment`, so they cannot disagree.

`release.mjs` is the supported App Store path. It requires an explicit marketing
version and monotonically increasing build number, rebuilds the transcript and
the Rust core with `--release`, regenerates the project, then creates a fresh
archive. Before succeeding it checks the archived runtime APNs value, app and
notification-extension versions, and absence of the debug push-key seed symbol.
It only uploads when passed `--upload`, using the tracked App Store export
options.

The last three lines are the check/test entry points; the four test tiers and how
they map onto CI live in [testing.md](testing.md).

## Build order

The Rust core is built **OUTSIDE Xcode (no shell build phase)**: `build-app.sh`
runs `build-core.sh` (cargo per-target + uniffi-bindgen + create-xcframework)
before `xcodegen generate`, so the project always references fresh products.

`generate_context!`-style staleness does not exist here, but the ORDER still
matters:

1. web bundle
2. `App/Resources/transcript/`
3. Rust core + Swift bindings + device xcframework
4. xcodegen
5. xcodebuild

## Device builds need the device slice AND a signed xcframework

`build-app.sh` defaults to sim-only (`XCF_FLAGS=(--sim-only)`), so a plain run
produces a sim-only `BayboCore.xcframework`; switching Xcode's destination to a
physical device then fails with *"no library for this platform was found."* Pass
`--device` (or run `build-core.sh` with no flags) to add the `ios-arm64` slice.

Xcode 15+/26 also rejects any xcframework referenced by a device build that isn't
code-signed (*"The Framework … is unsigned"*) — `-create-xcframework` emits an
unsigned bundle, so `build-core.sh` now `codesign`s it for non-sim-only builds
(identity via `BAYBO_IOS_CODESIGN_IDENTITY`, default `Apple Development`).

Run the signing build from an **interactive Terminal in your GUI login session**:
codesign needs the unlocked login keychain, and a headless shell fails with
`errSecInternalComponent` / "User interaction is not allowed."

## Sim loops must not clobber the device xcframework

**Sim-verification loops must not clobber the device xcframework.** A plain
`build-app.sh` run overwrites `Externals/BayboCore.xcframework` with a sim-only
unsigned bundle, and the next Xcode device Run fails with exactly the two errors
above.

- When iterating on Swift/web only (no `ffi/` changes), pass `--skip-rust`.
- After a full run does clobber it, restore with `scripts/build-core.sh` (no flags).

Check the result with `codesign --verify` — a failed headless sign
(`errSecInternalComponent`) still leaves a fresh `_CodeSignature/` dir in the
bundle, so its presence proves nothing.
