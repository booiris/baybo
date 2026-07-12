#!/usr/bin/env bash
# One-shot build orchestration: transcript web bundle → Rust xcframework +
# bindings → xcodegen → xcodebuild. Ordering matters — the Xcode project
# references Generated/BayboCore.swift, Externals/BayboCore.xcframework, and
# App/Resources/transcript/, all produced here.
#
#   scripts/build-app.sh [--release] [--device|--sim] [--skip-web] [--skip-rust]
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
  scripts/build-core.sh ${XCF_FLAGS[@]+"${XCF_FLAGS[@]}"} ${PROFILE_FLAG[@]+"${PROFILE_FLAG[@]}"}
fi

xcodegen generate
# Pin products inside the repo (build/DerivedData) — the default DerivedData
# lives outside it, and "install the .app xcodebuild just wrote" then grabs a
# stale bundle from whichever path happened to exist.
#
# The filter must not eat the exit status: `| grep … || true` forced a 0 even
# when xcodebuild failed (the `||` discards pipefail's status), so a broken
# build reported success. Take xcodebuild's own status out of PIPESTATUS, and
# keep the unfiltered log for the failure case.
mkdir -p build
set +e
xcodebuild -project Baybo.xcodeproj -scheme Baybo -configuration "$CONFIGURATION" \
  -sdk "$SDK" -destination "$DEST" -derivedDataPath build/DerivedData build \
  | tee build/xcodebuild.log | grep -E 'error|warning: |BUILD'
status=${PIPESTATUS[0]}
set -e
if (( status != 0 )); then
  echo "xcodebuild failed (exit $status) — full log: app/ios/build/xcodebuild.log" >&2
  exit "$status"
fi
