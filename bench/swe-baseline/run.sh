#!/usr/bin/env bash
#
# SWE-bench BASELINE: run the official minimal agent `mini-swe-agent` with the
# same model as bench/swe (deepseek-v4-flash by default), grade with the SAME
# official `swebench` harness, and emit a results JSON in bench/swe's schema so
# the resolve-rate is directly comparable to the baybo agent arm.
#
# Reuses the cached `swebench/sweb.eval.x86_64.*:latest` eval images (mini picks
# them by the standard name) and bench/swe/.env for the model key.
#
# Quick start (first 5 instances, validation):
#   bench/swe-baseline/run.sh
# Full 300 + compare to an baybo run:
#   LIMIT=300 AGENT_RESULTS=../swe/results/latest-agent.json bench/swe-baseline/run.sh
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SWE_DIR="$(cd "$BENCH_DIR/../swe" && pwd)"
# shellcheck source=/dev/null
[ -f "$SWE_DIR/.env" ] && { set -a; . "$SWE_DIR/.env"; set +a; }   # model key + model/base_url
# shellcheck source=/dev/null
[ -f "$BENCH_DIR/.env" ] && { set -a; . "$BENCH_DIR/.env"; set +a; }  # local overrides win

# ---- config (override via env) --------------------------------------------
: "${DATASET_NAME:=princeton-nlp/SWE-bench_Verified}"
: "${SUBSET:=verified}"
: "${SPLIT:=test}"
: "${LIMIT:=5}"                                  # first N; INSTANCE_IDS overrides
: "${INSTANCE_IDS:=}"                            # space-separated ids -> regex filter
: "${WORKERS:=4}"                                # parallel agent instances
: "${MAX_WORKERS:=4}"                            # grader harness workers
: "${MODEL:=${BAYBO_MODEL:-deepseek/deepseek-v4-flash}}"
: "${BASE_URL:=${BAYBO_BASE_URL:-}}"              # empty => litellm provider default
: "${STEP_LIMIT:=250}"                           # mini's per-instance step budget
: "${RUN_ID:=baseline-$(date +%Y-%m-%d__%H-%M-%S)}"
: "${RUNS_DIR:=$BENCH_DIR/runs}"
: "${RESULTS_DIR:=$BENCH_DIR/results}"
: "${AGENT_RESULTS:=}"                           # baybo results JSON to compare against
: "${DOCKER:=docker}"
: "${DRY_RUN:=0}"
API_KEY="${API_KEY:-${BAYBO_API_KEY:-}}"
[ -n "$API_KEY" ] || { echo "need a model key — set BAYBO_API_KEY in bench/swe/.env (or API_KEY)" >&2; exit 1; }
# ---------------------------------------------------------------------------

# litellm reads a provider-specific key var; export our generic key under both
# the deepseek and openai names so either model prefix works.
export DEEPSEEK_API_KEY="$API_KEY" OPENAI_API_KEY="$API_KEY"
# Isolate mini's global state from the host + quiet its startup banner.
export MSWEA_GLOBAL_CONFIG_DIR="$BENCH_DIR/mini-config" MSWEA_SILENT_STARTUP=1
export MSWEA_DOCKER_EXECUTABLE="$DOCKER"

command -v uv >/dev/null || { echo "uv required (https://docs.astral.sh/uv/)" >&2; exit 1; }
mkdir -p "$RUNS_DIR/$RUN_ID" "$RESULTS_DIR" "$MSWEA_GLOBAL_CONFIG_DIR"
echo ">> uv sync (mini-swe-agent + swebench)"; uv sync --project "$BENCH_DIR" >/dev/null
PY="$BENCH_DIR/.venv/bin/python"

# Config overrides for mini. `-c` REPLACES the default set, so the builtin
# swebench.yaml must be re-listed first.
mini_cfg=(-c swebench.yaml -c "agent.step_limit=$STEP_LIMIT" -c "model.cost_tracking=ignore_errors")
[ -n "$BASE_URL" ] && mini_cfg+=(-c "model.model_kwargs.api_base=$BASE_URL")

# Instance selection: explicit ids -> anchored regex; else first $LIMIT via slice.
if [ -n "$INSTANCE_IDS" ]; then
  # shellcheck disable=SC2086
  rx="^($(echo $INSTANCE_IDS | tr ' ' '|'))$"; sel=(--filter "$rx")
else
  sel=(--slice "0:$LIMIT")
fi

echo ">> baseline: model=$MODEL subset=$SUBSET split=$SPLIT ${sel[*]} workers=$WORKERS step_limit=$STEP_LIMIT"
if [ "$DRY_RUN" = 1 ]; then echo "[dry-run] mini-extra swebench ${sel[*]} -m $MODEL ${mini_cfg[*]}"; exit 0; fi

uv run --project "$BENCH_DIR" mini-extra swebench \
  --subset "$SUBSET" --split "$SPLIT" -m "$MODEL" "${mini_cfg[@]}" \
  --workers "$WORKERS" "${sel[@]}" -o "$RUNS_DIR/$RUN_ID"

preds="$RUNS_DIR/$RUN_ID/preds.json"
[ -f "$preds" ] || { echo "no preds.json at $preds — mini produced nothing" >&2; exit 1; }

echo ">> grading with the official swebench harness (cached eval images)"
( cd "$RUNS_DIR/$RUN_ID" && "$PY" -m swebench.harness.run_evaluation \
    --dataset_name "$DATASET_NAME" --split "$SPLIT" \
    --predictions_path preds.json --run_id "$RUN_ID" \
    --max_workers "$MAX_WORKERS" --namespace swebench ) \
  || echo ">> grader exited non-zero (shaping whatever report it wrote)"

"$PY" "$BENCH_DIR/shape_results.py" \
  --run-id "$RUN_ID" --runs-dir "$RUNS_DIR" --results-dir "$RESULTS_DIR" \
  --dataset-name "$DATASET_NAME" --split "$SPLIT" --model "$MODEL" \
  ${AGENT_RESULTS:+--agent-results "$AGENT_RESULTS"}
