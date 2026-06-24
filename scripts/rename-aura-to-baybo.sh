#!/usr/bin/env bash
#
# Deterministic, idempotent product rename: aura -> baybo (case-preserving).
#
#   AURA -> BAYBO   Aura -> Baybo   aura -> baybo
#
# Designed so an in-flight branch can rebase onto the pre-rename commit, re-run
# `scripts/rename-aura-to-baybo.sh sweep`, and rebase onto renamed master with
# near-zero manual conflict resolution. Hard break: no backward-compat shims;
# existing ~/.aura state must be moved by the operator (mv ~/.aura ~/.baybo).
#
# Phases:
#   sweep     content text-sweep (3 case variants) + git mv of 6 aura-named paths
#   regen     regenerate every derived artifact from the renamed source
#   validate  zero-residual grep + cargo/clippy/test + pnpm build + ts-bindings
#   all       sweep -> regen -> validate   (default)
#
# Byte-level perl substitution is UTF-8 safe: ASCII "aura" can never appear
# inside a multibyte sequence, so files with CJK/emoji/box-drawing are handled
# without an encoding layer (git flags 23 such source files "binary").
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"
export RUSTC_WRAPPER=   # bypass the sccache shim (breaks with "Operation not permitted")

SELF="scripts/rename-aura-to-baybo.sh"

# ---------------------------------------------------------------------------
# sweep
# ---------------------------------------------------------------------------
do_sweep() {
  echo "[sweep] text-sweeping tracked source files ..."
  local swept=0
  while IFS= read -r -d '' f; do
    [ -L "$f" ] && continue                       # never rewrite symlinks
    case "$f" in
      "$SELF") continue ;;                         # never rewrite this script
      pnpm-lock.yaml) continue ;;                  # regenerated (pnpm install)
      web/src/api/schema.d.ts) continue ;;         # regenerated (openapi-typescript)
      docs/openapi.json) continue ;;               # regenerated (UPDATE_OPENAPI)
      */generated/*) continue ;;                   # regenerated (ts-rs ts-export)
      *.lock|*.snap) continue ;;                   # regenerated (cargo / UPDATE_CHAT_SNAPSHOT)
    esac
    case "$f" in
      *.rs|*.toml|*.ts|*.tsx|*.js|*.jsx|*.mjs|*.cjs|*.py|*.md|*.sh|*.j2|\
*.json|*.yaml|*.yml|*.html|*.gitignore|*.env.example|Dockerfile|*/Dockerfile)
        if grep -qi aura "$f" 2>/dev/null; then
          perl -i -pe 's/AURA/BAYBO/g; s/Aura/Baybo/g; s/aura/baybo/g' "$f"
          swept=$((swept + 1))
        fi
        ;;
    esac
  done < <(git ls-files -z)
  echo "[sweep] rewrote $swept files"

  echo "[sweep] git mv of aura-named paths ..."
  mv_if() { [ -e "$1" ] && git mv "$1" "$2" || true; }
  mv_if aura.example.json baybo.example.json
  mv_if crates/skills/src/builtin/aura-cli crates/skills/src/builtin/baybo-cli
  mv_if bench/terminal-bench-1.0/tb_adapter/aura_agent.py    bench/terminal-bench-1.0/tb_adapter/baybo_agent.py
  mv_if bench/terminal-bench-1.0/tb_adapter/aura-setup.sh.j2 bench/terminal-bench-1.0/tb_adapter/baybo-setup.sh.j2
  mv_if bench/terminal-bench-2.0/harbor_adapter/aura_agent.py bench/terminal-bench-2.0/harbor_adapter/baybo_agent.py
  mv_if bench/swe/export_aura_trajs.py bench/swe/export_baybo_trajs.py

  # Length-pinned literals: "aura"(4)->"baybo"(5) grows fixed-size test keys by
  # one byte. EncryptionKey::new requires exactly 32 bytes; `[u8; 32] = *b"..."`
  # must be exactly 32. Trim each back to 32 bytes.
  echo "[sweep] fixing length-pinned 32-byte literals ..."
  git grep -l 'baybo-it-master-key-32-bytes!!!!!' | while IFS= read -r f; do
    perl -i -pe 's/baybo-it-master-key-32-bytes!!!!!/baybo-it-master-key-32-bytes!!!!/g' "$f"
  done
  [ -f crates/security/fuzz/fuzz_targets/fuzz_placeholder.rs ] && \
    perl -i -pe 's/baybo-fuzz-placeholder-master-key/baybo-fuzz-placeholder-masterkey/g' \
      crates/security/fuzz/fuzz_targets/fuzz_placeholder.rs

  # "baybo" is one byte longer than "aura", so some lines now exceed rustfmt's
  # width; reflow them (the rustfmt CI job / pre-commit hook would reject otherwise).
  echo "[sweep] cargo fmt (absorb width reflow) ..."
  cargo fmt --all
  echo "[sweep] done"
}

# ---------------------------------------------------------------------------
# regen  (order matters: cargo build must succeed before codegen reads source)
# ---------------------------------------------------------------------------
do_regen() {
  echo "[regen] cargo build (Cargo.lock + compile gate) ..."
  cargo build
  echo "[regen] fuzz lockfile ..."
  ( cd crates/security/fuzz && cargo generate-lockfile )
  echo "[regen] ts-rs generated dirs ..."
  cargo test -p baybo-channels  --features ts-export --lib wire
  cargo test -p baybo-tools     --features ts-export --lib mcp::access_rule
  cargo test -p baybo-bench-web --features ts-export --lib model
  # ApprovalDecision/ResourceAccess live in baybo-model and are NOT pulled in by
  # the channels --lib wire filter; export them explicitly (check-ts-bindings.sh
  # does not cover this surface).
  cargo test -p baybo-model     --features ts-export export_bindings
  echo "[regen] OpenAPI spec + schema.d.ts ..."
  UPDATE_OPENAPI=1 cargo test -p baybo-gateway --test all openapi_json_is_in_sync
  echo "[regen] TUI snapshots ..."
  UPDATE_CHAT_SNAPSHOT=1 cargo test -p baybo-tui --test chat_render
  echo "[regen] pnpm install + gen:api ..."
  pnpm install
  pnpm --filter baybo-web run gen:api
  echo "[regen] uv locks ..."
  local d
  for d in swe swe-baseline swe-claude terminal-bench-1.0 terminal-bench-2.0; do
    [ -f "bench/$d/pyproject.toml" ] && uv lock --project "bench/$d"
  done
  echo "[regen] done"
}

# ---------------------------------------------------------------------------
# validate
# ---------------------------------------------------------------------------
do_validate() {
  echo "[validate] residual 'aura' grep (case-insensitive, binary-as-text) ..."
  local residual
  residual="$(git grep -i -a -l aura -- . ":(exclude)$SELF" || true)"
  if [ -n "$residual" ]; then
    echo "[validate] FAIL — residual aura in:"; echo "$residual"; return 1
  fi
  echo "[validate] zero residual OK"
  echo "[validate] cargo build / clippy / test ..."
  cargo build
  cargo clippy --all --benches --tests --examples --all-features
  cargo test
  echo "[validate] pnpm build + ts-bindings ..."
  pnpm -r build
  scripts/check-ts-bindings.sh
  echo "[validate] all gates green"
}

case "${1:-all}" in
  sweep)    do_sweep ;;
  regen)    do_regen ;;
  validate) do_validate ;;
  all)      do_sweep; do_regen; do_validate ;;
  *) echo "usage: $0 {sweep|regen|validate|all}"; exit 2 ;;
esac
