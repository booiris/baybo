#!/usr/bin/env bash
# CI gate for the ts-rs-generated TypeScript bindings under
# `sdks/channel-ts/src/generated/`.
#
# Regenerates the files from Rust (`cargo test -p aura-channels
# --features ts-export`) and fails with a non-zero exit code if the
# working tree no longer matches. Ensures a wire-type change always
# lands with the matching TS side.
#
# Usage: scripts/check-ts-bindings.sh
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
GENERATED_DIR="sdks/channel-ts/src/generated"

cd "$REPO_ROOT"

echo "[*] Regenerating TS bindings from Rust (aura-channels ts-export) ..."
cargo test -p aura-channels --features ts-export --lib wire >/dev/null

# Compare against HEAD so a developer who only staged the Rust change
# without re-running ts-rs still gets caught. `--exit-code` returns 1
# on any difference without touching the index.
if ! git diff --exit-code HEAD -- "$GENERATED_DIR"; then
    echo
    echo "[!] Regenerated bindings differ from HEAD — the Rust wire types"
    echo "    and the checked-in TS bindings are out of sync."
    echo "[!] Fix: run 'pnpm --filter @aura/channel-sdk gen:types' and"
    echo "    commit the resulting changes under $GENERATED_DIR."
    exit 1
fi

echo "[*] sdks/channel-ts/src/generated/ is up to date."
