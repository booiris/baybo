#!/usr/bin/env bash
# Build the transcript bundle and place its dist where a shell wants it.
#
#   scripts/sync-web.sh <dest> [--frozen-lockfile]
#
# Both shells copy the SAME dist — the bundle is one artefact rendered by two
# WebViews — so the build and the copy live here rather than being spelled once
# per shell and drifting in the details that matter (whether the lockfile is
# frozen, whether the destination is cleared first).
#
# The destination is emptied before the copy. A stale `assets/<hash>.js` left
# behind by a previous build is not inert: the entry HTML is replaced, so
# nothing references it, but it still ships inside the app bundle and shows up
# in every size measurement as if it were live.
set -euo pipefail

DEST="${1:-}"
[[ -n "$DEST" ]] || { echo "usage: sync-web.sh <dest> [--frozen-lockfile]" >&2; exit 2; }
shift

INSTALL_FLAGS=(--silent)
for arg in "$@"; do
  case "$arg" in
    # CI installs frozen so a lockfile that drifted from package.json fails the
    # job instead of being silently resolved on the runner.
    --frozen-lockfile) INSTALL_FLAGS=(--frozen-lockfile) ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

WEB_DIR="$(cd "$(dirname "$0")/../web" && pwd)"
# Resolve before creating: `cd` into a path that does not exist yet fails, and a
# relative destination has to be read against the CALLER's directory, not this
# script's.
mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"

(cd "$WEB_DIR" && pnpm install "${INSTALL_FLAGS[@]}" && pnpm build)

rm -rf "$DEST"
mkdir -p "$DEST"
cp -R "$WEB_DIR/dist/." "$DEST/"

echo "OK: transcript bundle -> $DEST"
