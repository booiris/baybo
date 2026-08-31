# Building the iOS app

*How the app is built and installed — governs `app/ios/scripts/build-app.sh`, `app/ios/scripts/build-core.sh`, `app/ios/scripts/install.mjs`, `app/ios/scripts/release.mjs`, `app/ios/scripts/ExportOptions-AppStore.plist`, `app/ios/project.yml`, and the build products under `app/ios/Generated/` and `app/ios/Externals/`.*

```bash
scripts/build-app.sh             # web → rust xcframework → xcodegen → sim build
scripts/build-app.sh --device --release
node scripts/install.mjs         # archive + export + devicectl install (USB)
node scripts/release.mjs --version 0.1.0            # archive, export, verify
node scripts/release.mjs --version 0.1.0 --upload   # …and deliver to App Store Connect
cargo clippy --workspace --all-targets --all-features   # zero warnings
cargo nextest run --workspace    # ffi host tests
(cd web && pnpm build)           # tsc --noEmit + vite build
```

`Debug` versus `Release` controls optimization, not the APNs service. Local
device builds and `install.mjs` are development-signed, so both use the default
`BAYBO_APNS_ENVIRONMENT=development` and register sandbox tokens. `Distribution`
inherits Release optimization but overrides that setting to `production`; the
Baybo scheme's Archive action uses `Distribution`. The setting feeds both the
requested `aps-environment` entitlement and the app's runtime
`BayboApnsEnvironment`, so the two can never be configured apart.

They can still *end up* apart, and that is not a configuration bug: the
entitlement actually carried by a signed artifact is whichever one the
provisioning profile granted at signing time. An archive whose Info key says
`production` can be signed by a development profile that grants
`aps-environment: development` — which is exactly what happens when the
distribution certificate has expired and automatic signing falls back. Nothing
before the export can see this, which is what
[The App Store gate](#the-app-store-gate) is for.

`release.mjs` is the supported App Store path. It requires an explicit marketing
version, rebuilds the transcript and the Rust core with `--release`, regenerates
the project, creates a fresh archive, then exports and verifies it. `--upload` is
what delivers; without it you get a verified archive and ipa on disk. The gate it
enforces is described in [The App Store gate](#the-app-store-gate).

`--build-number` defaults to a local `YYYYMMDDHHMM` stamp, which is the
convention every uploaded build already follows. App Store Connect rejects any
build whose number is not higher than the last one it accepted, and nothing
in-tree records what that was — the stamp is what keeps the sequence monotonic
without a ledger that would immediately go stale. Pass the flag only to override.

The main app declares `ITSAppUsesNonExemptEncryption=false`: its Rust core ships
industry-standard Noise and rustls algorithms, and the initial App Store
availability excludes France, so this release does not require encryption
documentation in App Store Connect. Reassess the declaration before enabling
France or adding proprietary/non-standard cryptography; exempt encryption may
still carry separate government reporting obligations.

The last three lines are the check/test entry points; the four test tiers and how
they map onto CI live in [testing.md](testing.md).

## The App Store gate

**An archive built under automatic signing is development-signed, and that is
normal.** `CODE_SIGN_STYLE: Automatic` gives every configuration an
`Apple Development` identity; the distribution identity, the App Store profile
and the entitlements that come with it only exist after `-exportArchive`
re-signs. So `validateArchive` — runtime `BayboApnsEnvironment`, app and
extension versions, absence of the debug push-key seed symbol — can only speak
for what the archive *claims*. It has nothing to say about how the artifact is
signed, and adding a signature assertion there would reject every legitimate
archive.

That is why the export runs in two stages. The first exports locally: the
tracked `ExportOptions-AppStore.plist` is copied into the build directory with
`destination` set to `export`, so the ipa lands on disk with the same team,
method and signing style the upload will use. `verifyExport` unpacks it and
asserts, on the app and on the notification extension:

- `get-task-allow` is not true — App Store profiles carry it as `false` rather
  than dropping the key, so the invariant is "not debuggable", not "absent"
- the signing authority is an `Apple Distribution` identity
- the embedded profile has no `ProvisionedDevices` — the one structural marker
  that separates a store profile from a development or ad-hoc one, and unlike
  the profile's display name it is not Xcode-naming-dependent
- on the app only, the signed `aps-environment` is `production`. The
  extension's store profile carries no `aps-environment` at all — only the host
  app routes push — so asserting one there would reject every build
- marketing version, build number and `BayboApnsEnvironment` match what was
  asked for

Only then does `--upload` run the second stage, `destination: upload`, against
the same archive. Note what that does and does not prove: the uploaded ipa is a
second export of the same archive with the same certificate and the same
profile, so its entitlements necessarily match the verified one, but it is not
literally the bytes that were checked. Making it literally the same artifact
means uploading the verified ipa with `xcrun altool --upload-app`, which needs
its own App Store Connect credentials (an API key or an app-specific password)
rather than the Xcode account `xcodebuild` already uses.

The signing half of all this needs an unlocked login keychain, so the same rule
as [below](#device-builds-need-the-device-slice-and-a-signed-xcframework)
applies: run a release from an interactive Terminal in your GUI login session,
not over ssh.

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
