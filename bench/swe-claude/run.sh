#!/usr/bin/env bash
#
# SWE-bench CLAUDE arm: a Claude-style agent (Anthropic Messages API + tool_use,
# via the `anthropic` SDK) driving deepseek-v4-flash through a litellm
# `/v1/messages` -> deepseek proxy, graded by the SAME official `swebench`
# harness as the baybo + mini arms. Apples-to-apples third scaffold (same model,
# same eval images, same grader).
#
# Quick start (first 3 instances):           bench/swe-claude/run.sh
# Full 300 + compare to an baybo run:
#   LIMIT=300 AGENT_RESULTS=../swe/results/latest-agent.json bench/swe-claude/run.sh
set -euo pipefail

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SWE_DIR="$(cd "$BENCH_DIR/../swe" && pwd)"
# shellcheck source=/dev/null
[ -f "$SWE_DIR/.env" ] && { set -a; . "$SWE_DIR/.env"; set +a; }
# shellcheck source=/dev/null
[ -f "$BENCH_DIR/.env" ] && { set -a; . "$BENCH_DIR/.env"; set +a; }

# ---- config (override via env) --------------------------------------------
: "${DATASET_NAME:=princeton-nlp/SWE-bench_Verified}"
: "${SPLIT:=test}"
: "${LIMIT:=3}"                                   # first N; INSTANCE_IDS/SLICE override
: "${INSTANCE_IDS:=}"                             # space-separated ids
: "${SLICE:=}"                                    # start:end
: "${WORKERS:=4}"                                 # parallel agent instances
: "${MAX_WORKERS:=4}"                             # grader harness workers
: "${MODEL:=${BAYBO_MODEL:-deepseek/deepseek-v4-flash}}"
: "${MAX_TURNS:=120}"                            # Claude Code agent turn budget per instance
: "${CLAUDE_BIN:=$(readlink -f "$HOME/.local/bin/claude" 2>/dev/null)}"  # native claude binary
: "${PROMPT_TIMEOUT:=1800}"                       # per-instance wall ceiling (s)
: "${PROXY_PORT:=4000}"
: "${RUN_ID:=claude-$(date +%Y-%m-%d__%H-%M-%S)}"
: "${RUNS_DIR:=$BENCH_DIR/runs}"
: "${RESULTS_DIR:=$BENCH_DIR/results}"
: "${TRACE_DIR:=$BENCH_DIR/trace}"               # per-instance .messages.json transcripts
: "${AGENT_RESULTS:=}"                            # baybo results JSON to compare against
: "${DOCKER:=docker}"
API_KEY="${API_KEY:-${BAYBO_API_KEY:-}}"
[ -n "$API_KEY" ] || { echo "need the model key in BAYBO_API_KEY (bench/swe/.env) or API_KEY" >&2; exit 1; }
export DEEPSEEK_API_KEY="$API_KEY" OPENAI_API_KEY="$API_KEY"
[ -x "$CLAUDE_BIN" ] || { echo "native claude binary not found/executable at '$CLAUDE_BIN' — install Claude Code (curl -fsSL https://claude.ai/install.sh | bash) or set CLAUDE_BIN" >&2; exit 1; }
# ---------------------------------------------------------------------------

command -v uv >/dev/null || { echo "uv required (https://docs.astral.sh/uv/)" >&2; exit 1; }
$DOCKER info >/dev/null 2>&1 || { echo "Docker daemon not reachable" >&2; exit 1; }
mkdir -p "$RUNS_DIR/$RUN_ID" "$RESULTS_DIR" "$BENCH_DIR/proxy" "$TRACE_DIR/$RUN_ID"
echo ">> uv sync (anthropic + litellm[proxy] + swebench)"; uv sync --project "$BENCH_DIR" >/dev/null
PY="$BENCH_DIR/.venv/bin/python"

# ---- litellm proxy: Anthropic /v1/messages -> deepseek --------------------
PROXY_LOG="$BENCH_DIR/proxy/proxy-$RUN_ID.log"
echo ">> starting litellm proxy ($MODEL) on 127.0.0.1:$PROXY_PORT"
"$BENCH_DIR/.venv/bin/litellm" --model "$MODEL" --host 127.0.0.1 --port "$PROXY_PORT" \
  >"$PROXY_LOG" 2>&1 &
PROXY_PID=$!
cleanup() { kill "$PROXY_PID" 2>/dev/null || true; }
trap cleanup EXIT
for i in $(seq 1 40); do
  curl -s -o /dev/null "http://127.0.0.1:$PROXY_PORT/health/liveliness" 2>/dev/null && break
  sleep 1
  [ "$i" = 40 ] && { echo "proxy never came up; see $PROXY_LOG" >&2; exit 1; }
done
echo ">> proxy up (pid $PROXY_PID)"

# ---- instance selection ----------------------------------------------------
sel=()
if [ -n "$INSTANCE_IDS" ]; then
  # shellcheck disable=SC2206
  sel=(--instance-ids $INSTANCE_IDS)
elif [ -n "$SLICE" ]; then sel=(--slice "$SLICE")
else sel=(--limit "$LIMIT"); fi

echo ">> running claude-CLI arm: ${sel[*]} workers=$WORKERS max_turns=$MAX_TURNS (binary: $CLAUDE_BIN)"
uv run --project "$BENCH_DIR" "$PY" "$BENCH_DIR/run_claude.py" \
  --dataset-name "$DATASET_NAME" --split "$SPLIT" "${sel[@]}" \
  --workers "$WORKERS" --model "$MODEL" --base-url "http://127.0.0.1:$PROXY_PORT" \
  --claude-bin "$CLAUDE_BIN" --max-turns "$MAX_TURNS" --prompt-timeout "$PROMPT_TIMEOUT" \
  --run-id "$RUN_ID" --out "$RUNS_DIR/$RUN_ID" --trace-dir "$TRACE_DIR"

preds="$RUNS_DIR/$RUN_ID/preds.json"
[ -f "$preds" ] || { echo "no preds.json — agent produced nothing" >&2; exit 1; }

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
