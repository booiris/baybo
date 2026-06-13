#!/usr/bin/env bash
# One-click launcher for the bench results & trace viewer (bench-web).
#
# Default: build the embedded UI into the binary (via build.rs) and serve
# it — a single process on http://127.0.0.1:<port>.
#
#   ./run.sh                       # build + serve on :7000, root ./bench
#   ./run.sh --port 8080           # pick a port
#   ./run.sh --root /path/to/bench # point at a different bench root
#   ./run.sh --release             # optimized backend (slower first build)
#   ./run.sh --dev                 # Vite dev server (HMR) + API backend, on :5173
#
# Headless server? Forward the port over SSH, then open it locally:
#   ssh -L 7000:localhost:7000 <user>@<host>   # → http://localhost:7000
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)" # workspace root
cd "$root"

PORT=7000
BENCH_ROOT="$root/bench"
DEV=0
PROFILE=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dev) DEV=1; shift ;;
    --release) PROFILE=(--release); shift ;;
    --port) PORT="$2"; shift 2 ;;
    --root) BENCH_ROOT="$2"; shift 2 ;;
    -h | --help) awk 'NR>1 && /^#/{sub(/^# ?/,"");print;next} NR>1{exit}' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown arg: $1 (try --help)" >&2; exit 2 ;;
  esac
done

# Hydrate TS workspace deps once (build.rs also does this, but doing it
# here gives a clear error if pnpm is missing, and --dev needs it).
if [[ ! -f "$root/node_modules/.modules.yaml" ]]; then
  echo "==> pnpm install (first run)…" >&2
  pnpm install
fi

if [[ "$DEV" == "1" ]]; then
  echo "==> dev mode: starting API backend on :$PORT + Vite dev server on :5173" >&2
  cargo run "${PROFILE[@]}" -p aura-bench-web -- --root "$BENCH_ROOT" --port "$PORT" &
  backend=$!
  # Stop the backend when the dev server exits (Ctrl-C included).
  trap 'kill "$backend" 2>/dev/null || true' EXIT INT TERM
  BENCH_WEB_URL="http://127.0.0.1:$PORT" pnpm --filter aura-bench-web dev
else
  echo "==> building + serving bench-web on http://127.0.0.1:$PORT (root: $BENCH_ROOT)" >&2
  exec cargo run "${PROFILE[@]}" -p aura-bench-web -- --root "$BENCH_ROOT" --port "$PORT"
fi
