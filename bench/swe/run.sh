#!/usr/bin/env bash
#
# Run the aura SWE-bench benchmark for one or more arms and print a comparison
# table. Grading always uses the OFFICIAL `swebench` harness (Docker), so:
#   noop   = floor (empty patches → ~0%)
#   oracle = ceiling (gold patches → ~100%)         [no aura, no keys]
#   agent  = the real aura agent run inside each eval image (needs a key + musl)
#
# noop/oracle validate the whole Docker + grader pipeline offline before the
# agent arm spends anything. See bench/swe/README.md.
#
# Requirements: a running Docker daemon and `pip install swebench`. The agent arm
# additionally needs a model key and a static-musl `aura` (built for you here).
#
# Quick start — offline floor vs ceiling on the first SWE-bench_Lite instance:
#   bench/swe/run.sh
#
# A few specific instances, all three arms (agent needs AURA_API_KEY):
#   ARMS="noop oracle agent" INSTANCE_IDS="sympy__sympy-20590" bench/swe/run.sh
#
# Preview the plan (still needs swebench to resolve image keys, but no Docker/spend):
#   DRY_RUN=1 ARMS="noop oracle agent" bench/swe/run.sh
#
# Every UPPERCASE setting below is overridable from the environment.
set -euo pipefail
# shellcheck disable=SC2086  # several *_ARGS / id lists are intentionally split.

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$BENCH_DIR" rev-parse --show-toplevel)"
# shellcheck source=../progress.sh
. "$BENCH_DIR/../progress.sh"
# Auto-load this dir's .env (model key + overrides) — `set -a` exports each var.
if [ -f "$BENCH_DIR/.env" ]; then set -a; . "$BENCH_DIR/.env"; set +a; fi

# ---- config (override via env) --------------------------------------------
: "${ARMS:=noop oracle}"                       # space-separated: noop oracle agent
: "${DATASET_NAME:=princeton-nlp/SWE-bench_Lite}"
: "${SPLIT:=test}"
: "${NAMESPACE:=swebench}"                      # swebench => pull prebuilt Hub images; none => local build
: "${INSTANCE_IDS:=}"                           # space-separated; empty => first $LIMIT
: "${LIMIT:=1}"                                 # how many instances when INSTANCE_IDS empty
: "${CONCURRENCY:=4}"                           # agent eval containers in parallel
: "${MAX_WORKERS:=4}"                           # grader harness workers
: "${PROMPT_TIMEOUT:=1800}"                     # per-instance aura prompt ceiling (s)
: "${AURA_MODEL:=deepseek/deepseek-v4-flash}"  # agent LLM as <provider>/<model>
: "${AURA_BASE_URL:=}"                         # empty => provider default endpoint
: "${PYTHON:=}"                                # empty => the uv-managed bench/swe/.venv
: "${DOCKER:=docker}"
: "${MUSL_TARGET:=x86_64-unknown-linux-musl}"
: "${AURA_BIN:=}"                              # agent arm; empty => build static musl here
: "${AURA_RG_BIN:=}"                           # agent arm; empty => fetch static-musl rg to bench-out/rg
: "${RG_VERSION:=14.1.1}"                       # ripgrep release bundled into eval images
: "${OUTDIR:=$BENCH_DIR/bench-out}"            # scratch: instances.json (gitignored)
: "${RESULTS_DIR:=$BENCH_DIR/results}"         # the final results JSON report only
: "${RUNS_DIR:=$BENCH_DIR/runs}"               # run working artifacts: predictions, swebench report + logs/
: "${TRACE_DIR:=$BENCH_DIR/trace}"             # per-run transcripts + traces (gitignored); NO_TRACE=1 to disable
: "${RUN_ID:=$(date +%Y-%m-%d__%H-%M-%S)}"
: "${DRY_RUN:=0}"
: "${RUST_LOG:=aura_bench_swe=info}"; export RUST_LOG
INSTANCES="$OUTDIR/instances.json"
PKG="aura-bench-swe"
# ---------------------------------------------------------------------------

mkdir -p "$OUTDIR" "$RESULTS_DIR" "$RUNS_DIR"

# Point trace/latest at this run up front so it's monitorable mid-run (RUN_ID is
# known here, unlike tb which picks its own); the bin fills trace/<RUN_ID>/ as it
# produces traces.
mkdir -p "$TRACE_DIR/$RUN_ID" && ln -sfn "$RUN_ID" "$TRACE_DIR/latest"

# ---- preflight ------------------------------------------------------------
for arm in $ARMS; do
  case "$arm" in
    noop | oracle | agent) ;;
    *) echo "unknown arm '$arm' (want: noop oracle agent)" >&2; exit 1 ;;
  esac
done
want_agent=0; for a in $ARMS; do [ "$a" = agent ] && want_agent=1; done

# Python: a persistent uv-managed venv (bench/swe/.venv, pinned CPython + swebench)
# unless PYTHON is overridden to your own interpreter. `uv sync` is a fast no-op
# after the first run, so the env is built once and reused.
if [ -z "$PYTHON" ]; then
  command -v uv >/dev/null 2>&1 || {
    echo "uv is required to manage the Python env (https://docs.astral.sh/uv/), \
or set PYTHON to an interpreter that has swebench" >&2; exit 1; }
  echo ">> syncing uv python env (bench/swe/.venv)"
  uv sync --project "$BENCH_DIR"
  PYTHON="$BENCH_DIR/.venv/bin/python"
fi
if ! "$PYTHON" -c "import swebench" >/dev/null 2>&1; then
  echo "swebench not importable via '$PYTHON' (uv sync failed, or a PYTHON override lacks it)" >&2
  exit 1
fi
if [ "$DRY_RUN" != "1" ]; then
  $DOCKER info >/dev/null 2>&1 || { echo "Docker daemon not reachable ($DOCKER info failed)" >&2; exit 1; }
  if [ "$want_agent" = 1 ]; then
    : "${AURA_API_KEY:?the agent arm needs the model key in \$AURA_API_KEY (set it in .env)}"
  fi
fi

# ---- dataset export: instances.json (ids + canonical image keys) ----------
EXPORT_ARGS="--dataset-name $DATASET_NAME --split $SPLIT --namespace $NAMESPACE --out $INSTANCES"
if [ -n "$INSTANCE_IDS" ]; then
  EXPORT_ARGS="$EXPORT_ARGS --instance-ids $INSTANCE_IDS"
else
  EXPORT_ARGS="$EXPORT_ARGS --limit $LIMIT"
fi
echo ">> exporting instances -> $INSTANCES"
$PYTHON "$BENCH_DIR/swe_export.py" $EXPORT_ARGS
IDS="$($PYTHON -c "import json;print(' '.join(i['instance_id'] for i in json.load(open('$INSTANCES'))))")"
[ -n "$IDS" ] || { echo "no instances exported — check DATASET_NAME/SPLIT/INSTANCE_IDS" >&2; exit 1; }
echo ">> instances: $IDS"

# ---- dry run: just print each arm's plan ----------------------------------
if [ "$DRY_RUN" = "1" ]; then
  for arm in $ARMS; do
    echo ">> [dry-run] $arm"
    cargo run -q -p "$PKG" --bin run -- \
      --arm "$arm" --instances-json "$INSTANCES" \
      --dataset-name "$DATASET_NAME" --split "$SPLIT" --dry-run
  done
  exit 0
fi

# ---- build the bench bin (+ the musl aura and images for the agent arm) ----
echo ">> building $PKG bin"
cargo build -p "$PKG" --bins

if [ "$want_agent" = 1 ]; then
  if [ -z "$AURA_BIN" ]; then
    rustup target list --installed 2>/dev/null | grep -q "$MUSL_TARGET" || {
      echo "musl target missing — run: rustup target add $MUSL_TARGET" >&2; exit 1; }
    musl_cc="$(command -v x86_64-linux-musl-gcc || command -v musl-gcc || true)"
    [ -n "$musl_cc" ] || {
      echo "musl C compiler missing (Arch: sudo pacman -S musl) — libsql's bundled SQLite needs it" >&2
      exit 1; }
    echo ">> building static-musl aura (--features bench-bash)"
    CC_x86_64_unknown_linux_musl="$musl_cc" \
      cargo build --release --target "$MUSL_TARGET" --features bench-bash
    AURA_BIN="$REPO_ROOT/target/$MUSL_TARGET/release/aura"
  fi
  [ -x "$AURA_BIN" ] || { echo "aura binary not executable: $AURA_BIN" >&2; exit 1; }
  # Bundle a static-musl ripgrep — the eval images don't ship `rg`, which aura's
  # Grep tool needs (a glibc host rg won't load in the older-glibc images, same
  # as aura). Cached in bench-out/ across runs.
  if [ -z "$AURA_RG_BIN" ]; then
    AURA_RG_BIN="$OUTDIR/rg"
    if [ ! -x "$AURA_RG_BIN" ]; then
      echo ">> fetching static-musl ripgrep $RG_VERSION -> $AURA_RG_BIN"
      rg_url="https://github.com/BurntSushi/ripgrep/releases/download/${RG_VERSION}/ripgrep-${RG_VERSION}-x86_64-unknown-linux-musl.tar.gz"
      rg_tmp="$(mktemp -d)"
      if curl -fsSL --max-time 180 "$rg_url" -o "$rg_tmp/rg.tgz"; then
        tar -xzf "$rg_tmp/rg.tgz" -C "$rg_tmp"
        cp "$rg_tmp"/ripgrep-*/rg "$AURA_RG_BIN" && chmod +x "$AURA_RG_BIN"
      else
        echo "failed to download ripgrep $RG_VERSION; set AURA_RG_BIN to a static-musl rg" >&2
        rm -rf "$rg_tmp"; exit 1
      fi
      rm -rf "$rg_tmp"
    fi
  fi
  [ -x "$AURA_RG_BIN" ] || { echo "rg binary not executable: $AURA_RG_BIN" >&2; exit 1; }
  if [ "$NAMESPACE" = none ]; then
    echo ">> pre-building eval images locally (prepare_images; --namespace none)"
    $PYTHON -m swebench.harness.prepare_images \
      --dataset_name "$DATASET_NAME" --split "$SPLIT" \
      --instance_ids $IDS --max_workers "$MAX_WORKERS"
  else
    echo ">> namespace=$NAMESPACE: using prebuilt Hub images (the run bin pulls on demand)"
  fi
fi

# ---- run each arm ---------------------------------------------------------
run_arm() {
  local arm="$1"
  echo ">> [$arm] run -> $RESULTS_DIR/results-$arm-$RUN_ID.json"
  local common=(
    --arm "$arm" --instances-json "$INSTANCES"
    --dataset-name "$DATASET_NAME" --split "$SPLIT"
    --run-id "$RUN_ID" --results-dir "$RESULTS_DIR" --runs-dir "$RUNS_DIR" --namespace "$NAMESPACE"
    --max-workers "$MAX_WORKERS" --python-bin "$PYTHON" --docker-bin "$DOCKER"
    --concurrency "$CONCURRENCY"
  )
  if [ "$arm" = agent ]; then
    local agent_args=(
      --aura-bin "$AURA_BIN" --rg-bin "$AURA_RG_BIN" --model "$AURA_MODEL"
      --prompt-timeout "$PROMPT_TIMEOUT" --trace-dir "$TRACE_DIR"
    )
    [ -n "$AURA_BASE_URL" ] && agent_args+=(--base-url "$AURA_BASE_URL")
    [ -n "${NO_TRACE:-}" ] && agent_args+=(--no-trace)
    # live %bar: completed traces / total instances (the bin writes one per instance)
    local n_inst; n_inst=$(echo $IDS | wc -w)
    progress_start "$arm" "$n_inst" "ls '$TRACE_DIR/$RUN_ID/$arm'/*.trace.json 2>/dev/null | wc -l"
    cargo run -q -p "$PKG" --bin run -- "${common[@]}" "${agent_args[@]}"
    progress_stop
  else
    cargo run -q -p "$PKG" --bin run -- "${common[@]}"
  fi
}

for arm in $ARMS; do run_arm "$arm"; done

# `latest` pointers to this run so you don't scan timestamps (all gitignored).
[ -d "$TRACE_DIR/$RUN_ID" ] && ln -sfn "$RUN_ID" "$TRACE_DIR/latest"
for arm in $ARMS; do
  rf="results-$arm-$RUN_ID.json"
  [ -f "$RESULTS_DIR/$rf" ] && ln -sfn "$rf" "$RESULTS_DIR/latest-$arm.json"
done

# Co-locate each instance's full result next to its trace, so a trace is
# self-describing (pass/fail + details) without cross-referencing results/.
for arm in $ARMS; do
  rep="$RESULTS_DIR/results-$arm-$RUN_ID.json"
  [ -f "$rep" ] && python3 - "$rep" "$TRACE_DIR/$RUN_ID" <<'PY'
import json, sys, os, glob
rep, td = sys.argv[1], sys.argv[2]
try:
    res = json.load(open(rep)).get("results", [])
except Exception:
    res = []
where = {}
for f in glob.glob(os.path.join(td, "**", "*.trace.json"), recursive=True):
    where[os.path.basename(f)[: -len(".trace.json")]] = os.path.dirname(f)
for r in res:
    iid = next((str(r[k]) for k in
                ("instance_id", "question_id", "id", "qid", "task_id") if r.get(k)), None)
    d = where.get(iid) if iid else None
    if d:
        json.dump(r, open(os.path.join(d, f"{iid}.result.json"), "w"), indent=2)
PY
done

# ---- comparison table -----------------------------------------------------
echo
echo "=== summary (run_id=$RUN_ID) ==="
if command -v jq >/dev/null 2>&1; then
  printf '%-8s %5s %9s %8s %9s %11s\n' arm n resolved rate lat_ms cost$
  for arm in $ARMS; do
    f="$RESULTS_DIR/results-$arm-$RUN_ID.json"
    [ -f "$f" ] || continue
    jq -r '[.arm,.total_instances,.resolved,(.resolved_rate*100),.mean_latency_ms,(.total_cost_micro_usd/1000000)]|@tsv' "$f" \
      | awk -F'\t' '{printf "%-8s %5d %9d %7.1f%% %9.0f %11.6f\n",$1,$2,$3,$4,$5,$6}'
  done
else
  echo "(install jq for the table; raw results in $RESULTS_DIR/results-*-$RUN_ID.json)"
fi
echo
echo "full per-instance JSON: $RESULTS_DIR/results-*-$RUN_ID.json"

# Fold every run (incl. this one) into one cross-run scoreboard per arm. Best-effort:
# a merge hiccup must not fail the bench.
"$BENCH_DIR/consolidate.sh" || echo ">> consolidate failed (non-fatal); rerun $BENCH_DIR/consolidate.sh by hand"
