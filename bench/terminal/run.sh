#!/usr/bin/env bash
# One-shot runner for the aura <-> Terminal-Bench adapter. Builds the
# bench-passthrough binary if missing, then runs the harness in the uv-managed
# env with .env loaded. Any extra args pass through to `tb run`:
#
#   ./run.sh                          # defaults: gemini-2.5-flash, core 0.1.1, all tasks
#   ./run.sh -t fix-permissions       # a single task
#   ./run.sh --n-tasks 5 -m openai/gpt-4o
#   AURA_TB_REBUILD=1 ./run.sh        # force a fresh binary build first
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="$root/target/x86_64-unknown-linux-musl/release/aura"

# Build the bench-passthrough musl binary when it's missing (or AURA_TB_REBUILD
# is set). An AURA_TB_BINARY override points at your own binary and skips this.
if [[ -z "${AURA_TB_BINARY:-}" ]] && { [[ -n "${AURA_TB_REBUILD:-}" ]] || [[ ! -x "$bin" ]]; }; then
  echo "==> building bench-passthrough musl aura (cargo; first build ~3 min)…" >&2
  ( cd "$root" && cargo build --release --target x86_64-unknown-linux-musl \
      --features bench-passthrough -p aura )
fi

if [[ ! -f "$here/.env" ]]; then
  echo "missing $here/.env — create it:" >&2
  echo "    cp '$here/.env.example' '$here/.env' && \$EDITOR '$here/.env'" >&2
  exit 1
fi
# Load key/model/base_url; uv and the adapter inherit them.
set -a; . "$here/.env"; set +a

cd "$here"
# `tb` keeps the last value when an option repeats, so anything in "$@"
# (e.g. -m / -d / --n-tasks / -t) overrides these defaults.
exec uv run tb run \
  --agent-import-path tb_adapter.aura_agent:AuraAgent \
  --model "${AURA_TB_MODEL:-gemini/gemini-2.5-flash}" \
  -d terminal-bench-core==0.1.1 \
  --output-path runs \
  "$@"
