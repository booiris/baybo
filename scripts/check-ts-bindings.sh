#!/usr/bin/env bash
# CI gate for ts-rs-generated TypeScript bindings.
#
# Regenerates each Rust → TS surface and fails with a non-zero exit
# code if the working tree no longer matches. Ensures a wire-type
# change always lands with the matching TS side.
#
# Surfaces:
#   sdks/channel-ts/src/generated/    ← `wire` (channel WS frames) +
#                                        `baybo-channels` (registration wire)
#   tool-src/browser/src/generated/   ← `baybo-tools::mcp` (MCP `_meta.baybo.*`)
#   bench/bench-web/web/src/generated/ ← `baybo-bench-web` (bench spine model)
#   app/mobile/src/generated/         ← `baybo-mobile-core` (Tauri IPC DTOs)
#
# Usage: scripts/check-ts-bindings.sh
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# (binding-target-dir, regen-cargo-args)
#
# The channel-ts surface is split: the `Frame` / `Message` wire types (and
# the transitively-referenced `ApprovalDecision` / `ResourceAccess`) come
# from `wire`; the registration-wire types (`PromptKind` /
# `RegisterIn` / `RegisterOut`) stay in `baybo-channels`.
SURFACES=(
    "sdks/channel-ts/src/generated|test -p wire --features ts-export --lib"
    "sdks/channel-ts/src/generated|test -p baybo-channels --features ts-export --lib register_wire"
    "tool-src/browser/src/generated|test -p baybo-tools  --features ts-export --lib mcp::access_rule"
    "bench/bench-web/web/src/generated|test -p baybo-bench-web --features ts-export --lib model"
    # The mobile app is a separate Cargo workspace; baybo-mobile-core is FFI-free
    # (no Tauri/webkit), so this regen builds on the ubuntu CI runner unchanged.
    "app/mobile/src/generated|test --manifest-path app/mobile/Cargo.toml -p baybo-mobile-core --features ts-export --lib"
)

failed=0
for surface in "${SURFACES[@]}"; do
    target_dir="${surface%|*}"
    cargo_args="${surface#*|}"
    echo "[*] Regenerating TS bindings → $target_dir ..."
    # shellcheck disable=SC2086
    cargo $cargo_args >/dev/null
    # Compare against HEAD so a developer who only staged the Rust change
    # without re-running ts-rs still gets caught. `--exit-code` returns 1
    # on any difference without touching the index.
    if ! git diff --exit-code HEAD -- "$target_dir"; then
        echo
        echo "[!] Regenerated bindings differ from HEAD — the Rust source"
        echo "    types and the checked-in TS bindings under $target_dir"
        echo "    are out of sync."
        echo "[!] Fix: re-run scripts/check-ts-bindings.sh locally and"
        echo "    commit the resulting changes under $target_dir."
        failed=1
    else
        echo "[*] $target_dir is up to date."
    fi
done

exit "$failed"
