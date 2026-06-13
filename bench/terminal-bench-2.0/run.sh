#!/usr/bin/env bash
# One-shot runner for the aura <-> Harbor (Terminal-Bench 2.0) adapter. Builds the
# musl aura binary if missing, loads .env, then runs Harbor with the aura
# installed-agent against the terminal-bench-2 dataset. Extra args pass through:
#
#   ./run.sh                              # all TB 2.0 tasks
#   ./run.sh -i terminal-bench/fix-git    # a single named task (org-prefixed name)
#   ./run.sh -l 1                         # just the first task (quick smoke test)
#   ./run.sh -n 4                         # 4 concurrent trials
#   AURA_MODEL=openai/gpt-4o ./run.sh
#   AURA_REBUILD=1 ./run.sh               # force a fresh binary build first
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
bin="$root/target/x86_64-unknown-linux-musl/release/aura"
# shellcheck source=../progress.sh
. "$here/../progress.sh"

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

# Harbor writes every trial under runs/<ts>/<trial>/ (agent/{trace,messages}.json +
# verifier/reward.txt + a per-trial result.json). Mirror that into the trace/ +
# results/ convention the other benches use (swe, terminal-bench-1.0) so the score
# and call-tree are discoverable without spelunking into <trial>/agent/ and
# verifier/reward.txt. The full Harbor output stays under runs/.
mkdir -p runs results trace

# sync_outcomes <run-id>: (re)build trace/<id>/<task>/<trial>/ (agent-logs/ +
# result.json) and results/results-<id>.json from runs/<id>/. Idempotent — safe to
# call repeatedly while harbor grades trials one at a time.
sync_outcomes() {
  python3 - "$1" <<'PY'
import json, os, sys, glob, shutil
base = sys.argv[1]
job = f"runs/{base}"
if not os.path.isdir(job):
    sys.exit(0)
results = []
for td in sorted(glob.glob(f"{job}/*/")):
    trial = os.path.basename(td.rstrip("/"))
    reward_f = os.path.join(td, "verifier", "reward.txt")
    if not os.path.exists(reward_f):
        continue  # a job-level file, or a trial harbor hasn't finished grading
    try:
        reward = float(open(reward_f).read().strip())
    except Exception:
        reward = None
    # task name from the per-trial result.json (strip the dataset org prefix).
    # result.json lands ~tens of seconds after verifier/reward.txt, so the live
    # monitor often syncs a trial before it exists — fall back to the trial name
    # minus its __<id> suffix (== the clean task name) so both syncs target the
    # same trace/<base>/<task>/ dir instead of leaving a stale __<id> duplicate.
    task = trial.rsplit("__", 1)[0]
    rj = os.path.join(td, "result.json")
    if os.path.exists(rj):
        try:
            task = (json.load(open(rj)).get("task_name") or task).split("/")[-1]
        except Exception:
            pass
    # parser_results: per-test status from the verifier's CTRF report
    parser = {}
    ctrf = os.path.join(td, "verifier", "ctrf.json")
    if os.path.exists(ctrf):
        try:
            for t in json.load(open(ctrf)).get("results", {}).get("tests", []):
                parser[t.get("name", "?")] = t.get("status", "?")
        except Exception:
            pass
    resolved = reward is not None and reward >= 1.0
    failure_mode = "unset" if resolved else ("test_failed" if reward is not None else "unknown")
    # mirror the agent transcript + call-tree into trace/<base>/<task>/<trial>/
    dst = f"trace/{base}/{task}/{trial}"
    logs = os.path.join(dst, "agent-logs")
    os.makedirs(logs, exist_ok=True)
    for fn in ("trace.json", "messages.json"):
        src, out = os.path.join(td, "agent", fn), os.path.join(logs, fn)
        if os.path.exists(src) and not os.path.exists(out):
            try:
                shutil.copyfile(src, out)
            except Exception:
                pass
    outcome = {"task_id": task, "trial_name": trial, "is_resolved": resolved,
               "reward": reward, "failure_mode": failure_mode, "parser_results": parser}
    json.dump(outcome, open(os.path.join(dst, "result.json"), "w"), indent=2)
    results.append({**outcome,
                    "trace_path": f"{base}/{task}/{trial}/agent-logs/trace.json"})
json.dump({"id": base, "results": results},
          open(f"results/results-{base}.json", "w"), indent=2)
PY
}

# --- live monitor (background): point latest at the fresh job dir + sync each
# trial's outcome into trace/ + results/ as harbor grades it, so a long run is
# monitorable mid-flight (harbor names the job dir by timestamp → detect it here).
prev_job="$(ls -dt runs/*/ 2>/dev/null | grep -v latest | head -1)"
mon_start="$(date +%s)"
( linked=0
  while :; do
    cur="$(ls -dt runs/*/ 2>/dev/null | grep -v latest | head -1)"
    if [ -n "$cur" ] && [ "$cur" != "$prev_job" ]; then
      b="$(basename "$cur")"
      if [ "$linked" = 0 ]; then
        ln -sfn "$b" runs/latest
        ln -sfn "$b" trace/latest
        ln -sfn "results-$b.json" results/latest.json
        linked=1
      fi
      sync_outcomes "$b"
      # Harbor renders its own UI; this is a coarse graded-trial count (its total
      # isn't known to the shell, so count + elapsed, not a %bar).
      progress_render harbor 0 "$(find "runs/$b" -name reward.txt 2>/dev/null | wc -l)" "$mon_start"
    fi
    sleep 15
  done ) &
monitor_pid=$!

# Defaults; anything in "$@" (e.g. -i / -l / -n) is appended.
set +e
uv run harbor run \
  --dataset terminal-bench/terminal-bench-2 \
  --agent-import-path harbor_adapter.aura_agent:AuraAgent \
  --model "${AURA_MODEL:-deepseek/deepseek-v4-flash}" \
  --env docker \
  --jobs-dir runs \
  --yes \
  "$@"
rc=$?
set -e
kill "$monitor_pid" 2>/dev/null || true

# Final authoritative sync + latest pointers at the job we just produced.
job="$(ls -dt runs/*/ 2>/dev/null | grep -v latest | head -1)"
if [ -n "$job" ]; then
  base="$(basename "$job")"
  sync_outcomes "$base"
  ln -sfn "$base" runs/latest
  [ -f "results/results-$base.json" ] && ln -sfn "results-$base.json" results/latest.json
  [ -d "trace/$base" ] && ln -sfn "$base" trace/latest
  echo "==> report: results/results-$base.json  (runs/latest · results/latest.json · trace/latest)" >&2
  # Fold every run (incl. this one) into one cross-run scoreboard. Best-effort:
  # a merge hiccup must not mask harbor's exit code below.
  ./consolidate.sh || echo "==> consolidate failed (non-fatal); rerun ./consolidate.sh by hand" >&2
fi
exit "$rc"
