#!/usr/bin/env bash
# Build the shared Rust core and emit one shell's bindings.
#
#   scripts/build-core.sh --lang swift|kotlin --out-dir <dir> \
#                         [--target <triple>]... [--release] [--no-format]
#
# What it owns is the part neither shell should be spelling twice: the host
# cdylib the bindings are extracted from, and the `uniffi-bindgen` invocation —
# including the `--config` flag whose absence is silent (see below).
#
# What it does NOT own is packaging. `app/ios/scripts/build-core.sh` wraps this
# and adds the xcframework + codesign; the Android script adds `cargo ndk` and
# the 16 KiB alignment assertion. Cross-compiling differs enough between them
# (`cargo build --target` versus `cargo ndk -t`, which sets the NDK's linker and
# CC itself) that `--target` here is optional: a caller that has already built
# its own slices just asks for bindings.
set -euo pipefail

MOBILE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$MOBILE_DIR/Cargo.toml"

# Cargo may inherit a global `RUSTC_WRAPPER=sccache`. In an Xcode build phase
# that wrapper can fail before rustc starts ("Operation not permitted"), and an
# Android Studio-launched Gradle inherits the IDE's environment rather than the
# shell's, so neither caller can be trusted to have cleared it.
export RUSTC_WRAPPER=

PROFILE=debug
CARGO_FLAGS=()
TARGETS=()
LANG=""
OUT_DIR=""
FORMAT_FLAG=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --release) PROFILE=release; CARGO_FLAGS+=(--release); shift ;;
    --target) TARGETS+=("$2"); shift 2 ;;
    --lang) LANG="$2"; shift 2 ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --no-format) FORMAT_FLAG+=(--no-format); shift ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

[[ -n "$LANG" ]] || { echo "--lang swift|kotlin is required" >&2; exit 2; }
[[ -n "$OUT_DIR" ]] || { echo "--out-dir <dir> is required" >&2; exit 2; }
case "$LANG" in swift|kotlin) ;; *) echo "unsupported --lang: $LANG" >&2; exit 2 ;; esac

for t in ${TARGETS[@]+"${TARGETS[@]}"}; do
  cargo build --manifest-path "$MANIFEST" -p baybo-mobile-ffi --target "$t" \
    ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}
done

# Bindings come from a HOST build, not from any cross-compiled slice: uniffi
# reads interface metadata out of the library, and that metadata is the same for
# every target. It is also why one cdylib can serve both shells, and therefore
# why the exported signature cannot be `cfg`-split per platform.
cargo build --manifest-path "$MANIFEST" -p baybo-mobile-ffi ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

case "$(uname -s)" in
  Darwin) HOST_LIB="$MOBILE_DIR/target/$PROFILE/libbaybo_ffi.dylib" ;;
  *) HOST_LIB="$MOBILE_DIR/target/$PROFILE/libbaybo_ffi.so" ;;
esac

mkdir -p "$OUT_DIR"
# `--config` is not optional, and its absence is SILENT. In library mode
# uniffi-bindgen finds each crate's `uniffi.toml` by running `cargo metadata` in
# the CURRENT directory — and every caller of this script runs from its own
# shell directory, which has no Cargo.toml. Without it the swift
# `module_name = "BayboCore"` and the kotlin `package_name` are never read, and
# the bindings come out under their default names: a green build that produces
# files neither shell references.
cargo run -q --manifest-path "$MANIFEST" -p baybo-mobile-bindgen --bin uniffi-bindgen -- \
  generate \
  --library "$HOST_LIB" \
  --config "$MOBILE_DIR/ffi/uniffi.toml" \
  --language "$LANG" \
  --out-dir "$OUT_DIR" \
  ${FORMAT_FLAG[@]+"${FORMAT_FLAG[@]}"}

echo "OK: $LANG bindings in $OUT_DIR ($PROFILE${TARGETS[0]+, targets: ${TARGETS[*]}})"
