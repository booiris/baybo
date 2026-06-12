#!/usr/bin/env bash
#
# Run the aura memory benchmark end-to-end for one or more arms and print a
# comparison table. QA drives the REAL Aura agent (one `aura gateway` per arm +
# a concurrent `aura prompt` per question). See bench/memory/README.md.
#   noop = floor (memory off), oracle = ceiling (whole convo in prompt),
#   mem0 / openviking = real backends (an ingest pass runs first).
#
# Keys + endpoints live in bench/memory/.env (auto-loaded — copy from
# .env.example and fill in). The agent is built read-only so QA turns don't
# pollute the recall scope; this script builds `aura --features
# bench-readonly-memory` for you. By default it generates a self-contained
# config + workspace (no aura.json needed); set AURA_CONFIG to derive from yours.
#
# Quick start — fill bench/memory/.env, then (floor vs ceiling, self-contained):
#   bench/memory/run-bench.sh
#
# The real thing (OpenViking up at :1933; ingest runs automatically):
#   ARMS="noop oracle openviking" bench/memory/run-bench.sh
#
# Preview the plan without spending (no keys/config needed):
#   DRY_RUN=1 ARMS="noop oracle mem0 openviking" bench/memory/run-bench.sh
#
# Every UPPERCASE setting below is overridable from the environment.
set -euo pipefail
# shellcheck disable=SC2086  # $Q_ARG is intentionally word-split (numeric/empty)

# Auto-load this dir's .env (keys + endpoints) so you don't have to `source` it.
# `set -a` exports each var so the bench bins + spawned gateway inherit them.
BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -f "$BENCH_DIR/.env" ]; then set -a; . "$BENCH_DIR/.env"; set +a; fi

# ---- config (override via env) --------------------------------------------
: "${ARMS:=noop oracle}"            # space-separated: noop oracle mem0 openviking
: "${CONVERSATIONS:=1}"
: "${QUESTIONS:=}"                  # empty = all questions per conversation
: "${CONCURRENCY:=4}"
: "${GATEWAY_PORT:=0}"              # QA `aura gateway` port; 0 = auto-pick a free one
: "${TOP_K:=10}"                    # ingest recall depth
: "${SETTLE_TIMEOUT_SECS:=3600}"    # how long ingest waits for extraction to settle (1h)
: "${ALLOW_UNSETTLED:=0}"           # 1 = run QA even if extraction never settled
: "${OPENVIKING_ENDPOINT:=http://127.0.0.1:1933}"   # openviking server (docker port-map); override only off-default
: "${MEM0_BASE_URL:=http://127.0.0.1:8765}"   # self-hosted mem0 server (docker); empty = cloud Platform
: "${DRY_RUN:=0}"                  # 1 = print plan only (no build, no spend)
: "${OUTDIR:=$BENCH_DIR/bench-out}"   # bench scratch (gitignored): dataset + manifests
: "${DATASET:=$OUTDIR/locomo10.json}"
: "${RESULTS_DIR:=$BENCH_DIR/results}"   # per-run result JSONs (gitignored; regenerated each run)
: "${TRACE_DIR:=$BENCH_DIR/trace}"       # per-question transcripts + traces (gitignored); NO_TRACE=1 to disable
: "${WS_ROOT:=$BENCH_DIR/runs}"       # per-run aura workspaces (gitignored): config + vault + sessions + logs
: "${RUN_ID:=$(date +%Y-%m-%d__%H-%M-%S)}"
: "${RUST_LOG:=aura_bench_memory=info}"; export RUST_LOG
REPO_ROOT="$(git rev-parse --show-toplevel)"
: "${AURA_CONFIG:=}"               # base aura.json to derive from; empty = self-contained generated config
: "${AURA_BIN:=$REPO_ROOT/target/release/aura}"   # built with the bench feature below
# Self-contained answer model — the model Aura answers with (ignored when
# AURA_CONFIG is set). Defaults = DeepSeek; repoint at any provider to compare.
: "${AURA_ANSWER_MODEL:=deepseek-chat}"
: "${AURA_ANSWER_PROVIDER:=deepseek}"
: "${AURA_ANSWER_API_KEY_ENV:=DEEPSEEK_API_KEY}"   # env var holding the provider key
: "${AURA_ANSWER_BASE_URL:=}"                       # empty = provider's built-in endpoint
LOCOMO_URL="https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json"
PKG="aura-bench-memory"
# ---------------------------------------------------------------------------

mkdir -p "$OUTDIR" "$RESULTS_DIR" "$WS_ROOT"

# Point trace/latest at this run up front so it's monitorable mid-run (RUN_ID is
# known here); the bin fills trace/<RUN_ID>/ as it produces traces.
mkdir -p "$TRACE_DIR/$RUN_ID" && ln -sfn "$RUN_ID" "$TRACE_DIR/latest"

# Optional flag. QUESTIONS is numeric, so unquoted word-splitting is safe and
# an empty value expands to no argument.
Q_ARG=""
if [ -n "$QUESTIONS" ]; then Q_ARG="--questions $QUESTIONS"; fi
# Base config is optional: empty → the bench derives one from ~/.aura/config/aura.json.
CFG_ARG=""
if [ -n "$AURA_CONFIG" ]; then CFG_ARG="--aura-config $AURA_CONFIG"; fi
# Self-contained answer model → CLI flags (skipped under --aura-config: yours wins).
ANSWER_ARG=""
if [ -z "$AURA_CONFIG" ]; then
  ANSWER_ARG="--answer-model $AURA_ANSWER_MODEL --answer-provider $AURA_ANSWER_PROVIDER --answer-api-key-env $AURA_ANSWER_API_KEY_ENV"
  [ -n "$AURA_ANSWER_BASE_URL" ] && ANSWER_ARG="$ANSWER_ARG --answer-base-url $AURA_ANSWER_BASE_URL"
fi
# Optional --allow-unsettled (run QA even if extraction never settled).
UNSETTLED_ARG=""
if [ "$ALLOW_UNSETTLED" = 1 ]; then UNSETTLED_ARG="--allow-unsettled"; fi
# mem0 base URL → self-hosted OSS server (empty = managed cloud Platform).
MEM0_ARG=""
if [ -n "$MEM0_BASE_URL" ]; then MEM0_ARG="--mem0-base-url $MEM0_BASE_URL"; fi
# Disable trace export when NO_TRACE is set (default: export every question's transcript + trace).
TRACE_ARG=""
if [ -n "${NO_TRACE:-}" ]; then TRACE_ARG="--no-trace"; fi

# Warm the OpenViking embedding endpoint so the first recalls don't hit the
# model's cold-start (~20s) and time out — that alone swings the score wildly
# (10% vs 70% on a sample). No-op unless the embedding URL is set (from .env).
warm_embedding() {
  [ -n "${OPENVIKING_EMBEDDING_URL:-}" ] || return 0
  echo ">> warming embedding endpoint (${OPENVIKING_EMBEDDING_MODEL:-?})"
  for _ in 1 2 3; do
    curl -fsS --max-time 60 "$OPENVIKING_EMBEDDING_URL/embeddings" \
      -H 'content-type: application/json' \
      -d "{\"model\":\"${OPENVIKING_EMBEDDING_MODEL:-}\",\"input\":\"warmup\"}" \
      -o /dev/null 2>/dev/null || true
  done
}

# ---- preflight ------------------------------------------------------------
for arm in $ARMS; do
  case "$arm" in
    noop | oracle | mem0 | openviking) ;;
    *) echo "unknown arm '$arm' (want: noop oracle mem0 openviking)" >&2; exit 1 ;;
  esac
done
if [ "$DRY_RUN" != "1" ]; then
  : "${DEEPSEEK_API_KEY:?required — used by the judge model}"
  # Self-contained answer model needs its provider's API key present too.
  if [ -z "$AURA_CONFIG" ]; then
    answer_key="${!AURA_ANSWER_API_KEY_ENV:-}"
    : "${answer_key:?required — \$$AURA_ANSWER_API_KEY_ENV holds the $AURA_ANSWER_PROVIDER answer-model key (or set AURA_CONFIG to use your own)}"
  fi
  for arm in $ARMS; do
    case "$arm" in
      mem0)
        # Self-hosted docker server (MEM0_BASE_URL set) needs no cloud key; only
        # the managed cloud Platform (empty MEM0_BASE_URL) requires MEM0_API_KEY.
        if [ -z "${MEM0_BASE_URL:-}" ]; then
          : "${MEM0_API_KEY:?the mem0 cloud arm needs MEM0_API_KEY (use a throwaway project), or set MEM0_BASE_URL to target the self-hosted docker server}"
        fi ;;
      openviking)
        : "${OPENVIKING_API_KEY:?the openviking arm needs OPENVIKING_API_KEY (matches ov.conf root key)}" ;;
    esac
  done
fi

# ---- dataset: download once, reuse thereafter (dry-run needs it too, to count
# questions for the plan) -----------------------------------------------------
if [ ! -f "$DATASET" ]; then
  echo ">> downloading LOCOMO dataset -> $DATASET"
  curl -fL -o "$DATASET" "$LOCOMO_URL"
fi

if [ "$DRY_RUN" = "1" ]; then
  for arm in $ARMS; do
    echo ">> [dry-run] $arm"
    cargo run -q -p "$PKG" --bin run -- \
      --arm "$arm" --dataset "$DATASET" --conversations "$CONVERSATIONS" \
      --concurrency "$CONCURRENCY" $Q_ARG --dry-run
  done
  exit 0
fi

# ---- build: the bench bins + a read-only `aura` the bench drives ----------
echo ">> building $PKG bins"
cargo build -p "$PKG" --bins
echo ">> building read-only aura (--features bench-readonly-memory) -> $AURA_BIN"
cargo build --release -p aura --features bench-readonly-memory

run_arm() {
  local arm="$1"
  local results="$RESULTS_DIR/results-$arm-$RUN_ID.json"

  [ "$arm" = "openviking" ] && warm_embedding

  case "$arm" in
    mem0 | openviking)
      local manifest="$OUTDIR/manifest-$arm-$RUN_ID.json"
      # Reuse an existing manifest (same RUN_ID) so one ingest can back many QA
      # reruns; otherwise ingest fresh. Fix RUN_ID across invocations to reuse.
      if [ -f "$manifest" ]; then
        echo ">> [$arm] reusing manifest $manifest (skipping ingest)"
      else
        echo ">> [$arm] ingest -> $manifest"
        cargo run -q -p "$PKG" --bin ingest -- \
          --arm "$arm" --dataset "$DATASET" --conversations "$CONVERSATIONS" \
          --top-k "$TOP_K" --run-id "$RUN_ID" --out "$manifest" \
          --openviking-endpoint "$OPENVIKING_ENDPOINT" \
          --settle-timeout-secs "$SETTLE_TIMEOUT_SECS" $MEM0_ARG
      fi
      echo ">> [$arm] QA run -> $results"
      cargo run -q -p "$PKG" --bin run -- \
        --arm "$arm" --dataset "$DATASET" --conversations "$CONVERSATIONS" \
        --concurrency "$CONCURRENCY" --gateway-port "$GATEWAY_PORT" --run-id "$RUN_ID" $Q_ARG $UNSETTLED_ARG $MEM0_ARG \
        $CFG_ARG $ANSWER_ARG --openviking-endpoint "$OPENVIKING_ENDPOINT" \
        --aura-bin "$AURA_BIN" --workspace-root "$WS_ROOT" \
        --manifest "$manifest" --out "$results" --trace-dir "$TRACE_DIR" $TRACE_ARG
      ;;
    noop | oracle)
      echo ">> [$arm] QA run -> $results"
      cargo run -q -p "$PKG" --bin run -- \
        --arm "$arm" --dataset "$DATASET" --conversations "$CONVERSATIONS" \
        --concurrency "$CONCURRENCY" --gateway-port "$GATEWAY_PORT" --run-id "$RUN_ID" $Q_ARG \
        $CFG_ARG $ANSWER_ARG --aura-bin "$AURA_BIN" --workspace-root "$WS_ROOT" --out "$results" \
        --trace-dir "$TRACE_DIR" $TRACE_ARG
      ;;
  esac
}

for arm in $ARMS; do
  run_arm "$arm"
done

# `latest` pointers to this run so you don't scan timestamps (all gitignored).
[ -d "$TRACE_DIR/$RUN_ID" ] && ln -sfn "$RUN_ID" "$TRACE_DIR/latest"
for arm in $ARMS; do
  rf="results-$arm-$RUN_ID.json"
  [ -f "$RESULTS_DIR/$rf" ] && ln -sfn "$rf" "$RESULTS_DIR/latest-$arm.json"
done

# Co-locate each question's full result next to its trace, so a trace is
# self-describing (correct/score + judge reason) without cross-referencing results/.
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

# ---- comparison table across arms -----------------------------------------
echo
echo "=== summary (run_id=$RUN_ID) ==="
if command -v jq >/dev/null 2>&1; then
  printf '%-12s %5s %7s %8s %9s %9s %9s %11s\n' arm n acc f1 lat_ms in_tok out_tok cost$
  for arm in $ARMS; do
    f="$RESULTS_DIR/results-$arm-$RUN_ID.json"
    [ -f "$f" ] || continue
    jq -r '[.arm, .total_questions, (.overall_accuracy*100), .mean_f1, .mean_latency_ms, .mean_input_tokens, .mean_output_tokens, (.mean_cost_micro_usd/1000000)] | @tsv' "$f" \
      | awk -F'\t' '{printf "%-12s %5d %6.1f%% %8.3f %9.0f %9.0f %9.0f %11.6f\n", $1,$2,$3,$4,$5,$6,$7,$8}'
  done
else
  echo "(install jq for the comparison table; raw results listed below)"
  ls -1 "$RESULTS_DIR"/results-*-"$RUN_ID".json
fi
echo
echo "full per-question JSON: $RESULTS_DIR/results-*-$RUN_ID.json"
