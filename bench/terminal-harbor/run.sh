#!/usr/bin/env bash
# One-shot runner for the aura <-> Harbor (Terminal-Bench 2.0) adapter. Builds the
# musl aura binary if missing, loads .env, then runs Harbor with the aura
# installed-agent against the terminal-bench-2 dataset. Extra args pass through:
#
#   ./run.sh                      # all TB 2.0 tasks
#   ./run.sh -t hello-world       # a single named task
#   ./run.sh -l 1                 # just the first task (quick smoke test)
#   ./run.sh -n 4                 # 4 concurrent trials
#   AURA_MODEL=openai/gpt-4o ./run.sh
#   AURA_REBUILD=1 ./run.sh       # force a fresh binary build first
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="$root/target/x86_64-unknown-linux-musl/release/aura"

# Build the static-musl binary when missing (or AURA_REBUILD set). AURA_BIN
# points at your own binary and skips this.
if [[ -z "${AURA_BIN:-}" ]] && { [[ -n "${AURA_REBUILD:-}" ]] || [[ ! -x "$bin" ]]; }; then
  echo "==> building musl aura (--features bench-bash; first build ~3 min)…" >&2
  ( cd "$root" && cargo build --release --target x86_64-unknown-linux-musl \
      --features bench-bash -p aura )
fi

if [[ ! -f "$here/.env" ]]; then
  echo "missing $here/.env — create it:" >&2
  echo "    cp '$here/.env.example' '$here/.env' && \$EDITOR '$here/.env'" >&2
  exit 1
fi
# Load key/model/base_url into the env harbor (and the adapter) inherit.
set -a; . "$here/.env"; set +a

cd "$here"
# Defaults; anything in "$@" (e.g. -t / -l / -n) is appended.
uv run harbor run \
  --dataset terminal-bench/terminal-bench-2 \
  --agent-import-path harbor_adapter.aura_agent:AuraAgent \
  --model "${AURA_MODEL:-deepseek/deepseek-v4-flash}" \
  --env docker \
  --jobs-dir runs \
  --yes \
  "$@"
