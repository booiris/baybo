#!/usr/bin/env bash
# CI gate for ts-rs-generated TypeScript bindings.
#
# Regenerates each Rust → TS surface and fails with a non-zero exit
# code if the working tree no longer matches. Ensures a wire-type
# change always lands with the matching TS side.
#
# Every generated `*.ts` under each target dir is deleted before regen, so a
# renamed or removed type shows up as a deleted file. The gate fails when:
#   - a surface's cargo filter ran zero ts-rs `export_bindings_*` tests (a
#     stale filter regenerates nothing and would otherwise pass silently);
#   - a target dir is missing or empty after regen;
#   - a tracked file differs from HEAD (including deletions), or regen left
#     an untracked file (a newly exported type that was never committed).
#
# Surfaces:
#   sidecars/sdk/channel-ts/src/generated/    ← `wire` (channel WS frames) +
#                                        `baybo-channels` (registration wire) +
#                                        `baybo-model` (approval types)
#   sidecars/tool/browser/src/generated/ ← `baybo-browser-view` (browser view link wire)
#   bench/bench-web/web/src/generated/ ← `baybo-bench-web` (bench spine model)
#
# NOT covered here: app/web/src/api/schema.d.ts, which openapi-typescript
# generates from docs/openapi.json rather than from ts-rs. It needs pnpm, which
# this job does not install, so its drift check lives in the `frontend` CI job
# right after the build that regenerates it. Locally:
#   pnpm --filter baybo-web gen:api && git diff --exit-code -- app/web/src/api/schema.d.ts
#
# Usage: scripts/check-ts-bindings.sh
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# (binding-target-dir, regen-cargo-args)
#
# The channel-ts surface is split: the `Frame` / `Message` wire types come
# from `wire`; the registration-wire types (`PromptKind` / `RegisterIn` /
# `RegisterOut`) stay in `baybo-channels`; the approval types
# (`ApprovalDecision` / `ResourceAccess` / `ApprovalResolution`) are exported
# by `baybo-model` itself, since a dependent crate's run does not reliably
# re-emit them.
SURFACES=(
    "sidecars/sdk/channel-ts/src/generated|test -p wire --features ts-export --lib"
    "sidecars/sdk/channel-ts/src/generated|test -p baybo-channels --features ts-export --lib register_wire"
    "sidecars/sdk/channel-ts/src/generated|test -p baybo-model --features ts-export --lib approval::export_bindings"
    "sidecars/tool/browser/src/generated|test -p baybo-browser-view --features ts-export --lib wire::export_bindings"
    "bench/bench-web/web/src/generated|test -p baybo-bench-web --features ts-export --lib model"
)

# A dir can be fed by several surfaces, so wipe every dir once up front and
# check drift only after all of them have regenerated.
TARGET_DIRS=()
for surface in "${SURFACES[@]}"; do
    target_dir="${surface%|*}"
    case " ${TARGET_DIRS[*]} " in
        *" $target_dir "*) ;;
        *) TARGET_DIRS+=("$target_dir") ;;
    esac
done
for target_dir in "${TARGET_DIRS[@]}"; do
    if [ -d "$target_dir" ]; then
        find "$target_dir" -type f -name '*.ts' -delete
    fi
done

failed=0
for surface in "${SURFACES[@]}"; do
    target_dir="${surface%|*}"
    cargo_args="${surface#*|}"
    echo "[*] Regenerating TS bindings → $target_dir ..."
    # shellcheck disable=SC2086
    if ! cargo_out="$(cargo $cargo_args 2>&1)"; then
        printf '%s\n' "$cargo_out"
        echo "[!] cargo $cargo_args failed."
        failed=1
        continue
    fi
    exported="$(printf '%s\n' "$cargo_out" \
        | grep -c '^test .*export_bindings_.* \.\.\. ok$' || true)"
    if [ "$exported" -eq 0 ]; then
        echo "[!] cargo $cargo_args ran 0 export_bindings tests — the filter"
        echo "    is stale and regenerated nothing under $target_dir."
        failed=1
        continue
    fi
    echo "[*] $exported bindings exported by: cargo $cargo_args"
done

for target_dir in "${TARGET_DIRS[@]}"; do
    if [ -z "$(find "$target_dir" -type f -print -quit 2>/dev/null)" ]; then
        echo "[!] $target_dir is missing or empty after regen."
        failed=1
        continue
    fi
    # Compare against HEAD so a developer who only staged the Rust change
    # without re-running ts-rs still gets caught. `git diff` is blind to
    # untracked files, so a freshly exported type is caught by `git status`.
    drift=0
    if ! git diff --exit-code HEAD -- "$target_dir"; then
        drift=1
    fi
    untracked="$(git status --porcelain --untracked-files=all -- "$target_dir")"
    if [ -n "$untracked" ]; then
        printf '%s\n' "$untracked"
        drift=1
    fi
    if [ "$drift" -ne 0 ]; then
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
