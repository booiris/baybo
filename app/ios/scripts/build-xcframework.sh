#!/usr/bin/env bash
# Build the Rust core into BayboCore.xcframework + generated Swift bindings.
#
#   scripts/build-xcframework.sh [--release] [--sim-only]
#
# Produces:
#   Generated/BayboCore.swift          — the UniFFI Swift bindings (add to the app target)
#   Externals/BayboCore.xcframework    — device + simulator static libs with headers
#
# The keychain access group is baked into the staticlib at compile time
# (ffi/build.rs); default it here explicitly so a plain shell build matches the
# Xcode-signed entitlement (`$(AppIdentifierPrefix)com.baybo.app`).
set -euo pipefail

cd "$(dirname "$0")/.."

# Cargo may inherit a global `RUSTC_WRAPPER=sccache`; in iOS build contexts that
# wrapper can fail before rustc starts with "Operation not permitted".
export RUSTC_WRAPPER=

PROFILE=debug
CARGO_FLAGS=()
SIM_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE=release; CARGO_FLAGS+=(--release) ;;
    --sim-only) SIM_ONLY=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

: "${BAYBO_IOS_TEAM_ID:=KLK5BP5YS6}"
# Match the app's deployment target so ld doesn't warn about newer objects.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-26.0}"
export BAYBO_IOS_KEYCHAIN_ACCESS_GROUP="${BAYBO_IOS_KEYCHAIN_ACCESS_GROUP:-$BAYBO_IOS_TEAM_ID.com.baybo.app}"
echo "keychain access group: $BAYBO_IOS_KEYCHAIN_ACCESS_GROUP"

TARGETS=(aarch64-apple-ios-sim)
if [[ "$SIM_ONLY" != 1 ]]; then
  TARGETS+=(aarch64-apple-ios)
fi

for t in "${TARGETS[@]}"; do
  cargo build -p baybo-ios-ffi --target "$t" ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}
done

# Bindings are extracted from a host cdylib build (same interface metadata).
cargo build -p baybo-ios-ffi ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

rm -rf Generated Externals/headers Externals/BayboCore.xcframework
mkdir -p Generated Externals/headers
cargo run -q -p baybo-ios-bindgen --bin uniffi-bindgen -- generate \
  --library "target/$PROFILE/libbaybo_ffi.dylib" \
  --language swift \
  --out-dir Generated

# xcframework layout: the modulemap must be named module.modulemap inside each
# headers dir; uniffi emits <namespace>FFI.modulemap.
mv Generated/*.h Externals/headers/
mv Generated/*.modulemap Externals/headers/module.modulemap

XCF_ARGS=()
for t in "${TARGETS[@]}"; do
  XCF_ARGS+=(-library "target/$t/$PROFILE/libbaybo_ffi.a" -headers Externals/headers)
done
xcodebuild -create-xcframework ${XCF_ARGS[@]+"${XCF_ARGS[@]}"} -output Externals/BayboCore.xcframework | cat

# Device builds (Xcode 15+/26) reject any referenced xcframework that isn't
# code-signed, and `-create-xcframework` emits an unsigned bundle. Sign it here
# so a device build links cleanly. Sim-only bundles are skipped — the simulator
# SDK never enforces the signature.
if [[ "$SIM_ONLY" != 1 ]]; then
  : "${BAYBO_IOS_CODESIGN_IDENTITY:=Apple Development}"
  codesign --force --timestamp=none --sign "$BAYBO_IOS_CODESIGN_IDENTITY" \
    Externals/BayboCore.xcframework
  echo "signed xcframework with: $BAYBO_IOS_CODESIGN_IDENTITY"
fi

echo "OK: Generated/$(ls Generated) + Externals/BayboCore.xcframework ($PROFILE, targets: ${TARGETS[*]})"
