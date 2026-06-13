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
# shellcheck source=../progress.sh
. "$here/../progress.sh"
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

# --- live run monitor (background; runs while `tb` runs) ---------------------
# tb writes each task's results.json as it grades; mirror that outcome
# (resolved + failure_mode + parser results) into the task's trace dir as
# result.json, so a task's trace is self-describing. Also point `latest` at the
# in-flight run so it's monitorable mid-run (run.sh otherwise wires latest only
# at the end). tb picks the run-id, so the fresh run dir is detected in the bg.
sync_outcomes() {  # $1 = run-id; mirror each graded task's outcome into its trace dir
  python3 - "$1" <<'PY'
import json, sys, glob, os
b = sys.argv[1]
try:
    rl = json.load(open(f"runs/{b}/results.json")).get("results", [])
except Exception:
    rl = []
for r in rl:
    t = r.get("task_id")
    if not t:
        continue
    for td in glob.glob(f"trace/{b}/{t}/*/"):
        out = os.path.join(td, "result.json")
        if os.path.exists(out):
            continue
        try:
            json.dump({k: r.get(k) for k in
                       ("task_id", "is_resolved", "failure_mode", "parser_results")},
                      open(out, "w"), indent=2)
        except Exception:
            pass
PY
}

prev_run="$(ls -dt runs/*/ 2>/dev/null | grep -v latest | head -1)"
mon_start="$(date +%s)"
( linked=0
  while :; do
    cur="$(ls -dt runs/*/ 2>/dev/null | grep -v latest | head -1)"
    if [ -n "$cur" ] && [ "$cur" != "$prev_run" ]; then
      b="$(basename "$cur")"
      if [ "$linked" = 0 ]; then
        ln -sfn "$b" runs/latest; mkdir -p trace; ln -sfn "$b" trace/latest; linked=1
      fi
      sync_outcomes "$b"
      # tb renders its own UI; this is a coarse count of per-task dirs it has created
      # (total isn't known to the shell, so count + elapsed, not a %bar).
      progress_render tb 0 "$(find "runs/$b" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)" "$mon_start"
    fi
    sleep 15
  done ) &
monitor_pid=$!

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
kill "$monitor_pid" 2>/dev/null || true

# Surface tb's report into results/ so it lands where swe/memory put theirs (the
# full run output stays under runs/). tb writes a fresh runs/<ts>/ per run — the
# newest dir is the one we just produced.
latest="$(ls -dt runs/*/ 2>/dev/null | grep -v latest | head -1)"
if [ -n "$latest" ] && [ -f "${latest}results.json" ]; then
  base="$(basename "$latest")"
  sync_outcomes "$base"
  mkdir -p results
  cp "${latest}results.json" "results/results-$base.json"
  # tb reports 0 tokens for installed agents; recover real usage (incl. cached
  # input) from each task's agent trace and inject it into the results file.
  python3 - "$base" <<'PY'
import json, os, sys
b = sys.argv[1]
rf = f"results/results-{b}.json"
try:
    data = json.load(open(rf))
except Exception:
    sys.exit(0)
def sums(task, trial):
    tj = f"trace/{b}/{task}/{trial}/agent-logs/trace.json"
    tin = tout = tcache = 0
    if task and trial and os.path.exists(tj):
        try:
            for job in json.load(open(tj)).get("jobs", []):
                for step in job.get("steps", []):
                    for span in step.get("spans", []):
                        k = span.get("kind", {})
                        if k.get("kind") == "llm_call":
                            r = k.get("result") or {}
                            tin += r.get("input_tokens") or 0
                            tout += r.get("output_tokens") or 0
                            tcache += r.get("cached_input_tokens") or 0
        except Exception:
            pass
    return tin, tout, tcache
for it in data.get("results", []):
    tin, tout, tcache = sums(it.get("task_id"), it.get("trial_name"))
    if tin or tout:
        it["total_input_tokens"] = tin
        it["total_output_tokens"] = tout
        it["total_cached_input_tokens"] = tcache
json.dump(data, open(rf, "w"), indent=2)
PY
  # `latest` pointers to the newest run so you don't scan timestamps (gitignored).
  ln -sfn "$base" runs/latest
  ln -sfn "results-$base.json" results/latest.json
  [ -d "trace/$base" ] && ln -sfn "$base" trace/latest
  echo "==> report: results/results-$base.json  (runs/latest · results/latest.json · trace/latest)" >&2
  # Fold every run (incl. this one) into one cross-run scoreboard. Best-effort:
  # a merge hiccup must not mask tb's exit code.
  ./consolidate.sh || echo "==> consolidate failed (non-fatal); rerun ./consolidate.sh by hand" >&2
fi
exit "$rc"
