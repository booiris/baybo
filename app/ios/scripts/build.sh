#!/usr/bin/env bash
# One-shot build orchestration: transcript web bundle → Rust xcframework +
# bindings → xcodegen → xcodebuild. Ordering matters — the Xcode project
# references Generated/BayboCore.swift, Externals/BayboCore.xcframework, and
# App/Resources/transcript/, all produced here.
#
#   scripts/build.sh [--release] [--device|--sim] [--skip-web] [--skip-rust]
set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE_FLAG=()
CONFIGURATION=Debug
DEST="generic/platform=iOS Simulator"
SDK=iphonesimulator
XCF_FLAGS=(--sim-only)
SKIP_WEB=0
SKIP_RUST=0
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE_FLAG=(--release); CONFIGURATION=Release ;;
    --device) DEST="generic/platform=iOS"; SDK=iphoneos; XCF_FLAGS=() ;;
    --sim) ;;
    --skip-web) SKIP_WEB=1 ;;
    --skip-rust) SKIP_RUST=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

if [[ "$SKIP_WEB" != 1 ]]; then
  (cd web && pnpm install --silent && pnpm build)
  rm -rf App/Resources/transcript
  mkdir -p App/Resources/transcript
  cp -R web/dist/. App/Resources/transcript/
fi

if [[ "$SKIP_RUST" != 1 ]]; then
  scripts/build-xcframework.sh ${XCF_FLAGS[@]+"${XCF_FLAGS[@]}"} ${PROFILE_FLAG[@]+"${PROFILE_FLAG[@]}"}
fi

xcodegen generate
xcodebuild -project Baybo.xcodeproj -scheme Baybo -configuration "$CONFIGURATION" \
  -sdk "$SDK" -destination "$DEST" build | grep -E 'error|warning: |BUILD' || true
