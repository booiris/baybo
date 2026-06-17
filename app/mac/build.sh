#!/usr/bin/env bash
#
# Build / run helper for the Aura macOS app (app/mac).
#
#   ./build.sh run          # launch the app for development (tauri dev, HMR)
#   ./build.sh build        # build the release .app bundle (+ .dmg)
#   ./build.sh build --debug # build a debug .app bundle (fast, no LTO)
#   ./build.sh web          # run only the Vite frontend (no Tauri shell)
#   ./build.sh clean        # remove dist/ and node_modules
#   ./build.sh help
#
# Default command (no args) is `build`.
set -euo pipefail

# Resolve paths regardless of the caller's cwd. This script lives in app/mac/.
APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$APP_DIR/../.." && pwd)"
PKG="aura-mac"

cd "$APP_DIR"

log() { printf '\033[1;33m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v pnpm >/dev/null 2>&1 || die "pnpm not found — install it (https://pnpm.io)."

ensure_deps() {
  # The workspace install lands node_modules at the repo root; app/mac gets a
  # symlinked node_modules. Install once if either is missing.
  if [[ ! -d "$APP_DIR/node_modules" || ! -d "$REPO_ROOT/node_modules" ]]; then
    log "Installing JS dependencies (pnpm install)…"
    (cd "$REPO_ROOT" && pnpm install)
  fi
}

cmd="${1:-build}"
[[ $# -gt 0 ]] && shift || true

case "$cmd" in
  run|dev)
    ensure_deps
    log "Launching Aura (tauri dev)…"
    exec pnpm --filter "$PKG" tauri dev "$@"
    ;;

  build|bundle)
    ensure_deps
    log "Building Aura .app bundle (tauri build)…"
    pnpm --filter "$PKG" tauri build "$@"
    # Workspace target dir lives at the repo root.
    profile="release"
    [[ " $* " == *" --debug "* ]] && profile="debug"
    out="$REPO_ROOT/target/$profile/bundle/macos/Aura.app"
    [[ -d "$out" ]] && log "Bundle: $out" || log "Build finished (see $REPO_ROOT/target/$profile/bundle/)."
    ;;

  web)
    ensure_deps
    log "Running the Vite frontend only (no Tauri shell)…"
    exec pnpm --filter "$PKG" dev "$@"
    ;;

  clean)
    log "Removing dist/ and node_modules…"
    rm -rf "$APP_DIR/dist" "$APP_DIR/node_modules"
    log "Clean."
    ;;

  help|-h|--help)
    sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    ;;

  *)
    die "unknown command '$cmd' (try: run | build | web | clean | help)"
    ;;
esac
