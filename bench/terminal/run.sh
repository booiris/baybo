#!/usr/bin/env bash
# One-shot runner for the aura <-> Terminal-Bench adapter. Builds the
# musl binary if missing, then runs the harness in the uv-managed
# env with .env loaded. Any extra args pass through to `tb run`:
#
#   ./run.sh                          # defaults: gemini-2.5-flash, core 0.1.1, all tasks
#   ./run.sh -t fix-permissions       # a single task
#   ./run.sh --n-tasks 5 -m openai/gpt-4o
#   AURA_REBUILD=1 ./run.sh           # force a fresh binary build first
#   TB_TEST_TIMEOUT_SEC=600 ./run.sh  # raise the per-task test-phase budget (default 300)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="$root/target/x86_64-unknown-linux-musl/release/aura"

# Build the musl binary when it's missing (or AURA_REBUILD
# is set). An AURA_BIN override points at your own binary and skips this.
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
# Load key/model/base_url; uv and the adapter inherit them.
set -a; . "$here/.env"; set +a

# Pre-bake the test-phase toolchain (curl + uv + warmed pytest cache) into the
# base images so each task's grader bootstrap runs from local cache instead of
# re-downloading curl/uv/pytest over the slow network every task (idempotent;
# first time ~2-3 min). SKIP_BASE_PREP=1 bypasses it.
if [[ -z "${SKIP_BASE_PREP:-}" ]]; then
  "$here/prepare-bases.sh"
fi

cd "$here"
# Each task's grader (tests/setup-uv-pytest.sh) installs curl + uv + pytest at
# TEST time; on a slow network that bootstrap overruns tb's stock 60s
# max_test_timeout_sec, so a task the agent SOLVED still scores `test_timeout`
# (is_resolved: null). Raise the test-phase budget by default so the grader can
# bootstrap — the pytest logic itself is untouched. Tune via TB_TEST_TIMEOUT_SEC.
test_timeout_sec="${TB_TEST_TIMEOUT_SEC:-300}"

# `tb` keeps the last value when an option repeats, so anything in "$@"
# (e.g. -m / -d / --n-tasks / -t / --global-test-timeout-sec) overrides these
# defaults. tb owns the full run output under runs/<ts>/ (casts, logs, results.json).
uv run tb run \
  --agent-import-path tb_adapter.aura_agent:AuraAgent \
  --model "${AURA_MODEL:-gemini/gemini-2.5-flash}" \
  -d terminal-bench-core==0.1.1 \
  --global-test-timeout-sec "$test_timeout_sec" \
  --output-path runs \
  "$@"
rc=$?

# Surface tb's report into results/ so it lands where swe/memory put theirs (the
# full run output stays under runs/). tb writes a fresh runs/<ts>/ per run — the
# newest dir is the one we just produced.
latest="$(ls -dt runs/*/ 2>/dev/null | head -1)"
if [ -n "$latest" ] && [ -f "${latest}results.json" ]; then
  base="$(basename "$latest")"
  mkdir -p results
  cp "${latest}results.json" "results/results-$base.json"
  # `latest` pointers to the newest run so you don't scan timestamps (gitignored).
  ln -sfn "$base" runs/latest
  ln -sfn "results-$base.json" results/latest.json
  [ -d "trace/$base" ] && ln -sfn "$base" trace/latest
  echo "==> report: results/results-$base.json  (runs/latest · results/latest.json · trace/latest)" >&2
fi
exit "$rc"
