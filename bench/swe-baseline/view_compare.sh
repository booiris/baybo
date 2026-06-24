#!/usr/bin/env bash
#
# One-click: regenerate the baybo-vs-mini-swe-agent trajectory comparison and
# (optionally) serve it. Runs all three exporters in order:
#   ../swe/export_baybo_trajs.py   baybo trace .messages.json  -> per-instance .md
#   ./export_trajs.py             mini .traj.json            -> per-instance .md
#   ./compare_arms.py             both .md sets              -> _compare/compare.html
#
# Run ids default to whatever the latest-*.json result symlinks point at; the
# generated .md/.html land in already-gitignored scratch dirs.
#
#   bench/swe-baseline/view_compare.sh                 # latest runs, serve on :8091
#   PORT=9000 bench/swe-baseline/view_compare.sh       # different port
#   SERVE=0   bench/swe-baseline/view_compare.sh       # just regenerate, don't serve
#   BAYBO_RID=full300-… MINI_RID=baseline-full300-… bench/swe-baseline/view_compare.sh  # pin runs
set -euo pipefail

BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SWE_DIR="$(cd "$BASE_DIR/../swe" && pwd)"
: "${PORT:=8091}"
: "${SERVE:=1}"
PY="${PY:-python3}"   # all three scripts are stdlib-only

# resolve run ids from the latest-* result symlinks (override via env) ---------
derive() { local t; t="$(basename "$(readlink -f "$1")")"; t="${t#"$2"}"; echo "${t%.json}"; }
: "${BAYBO_RID:=$(derive "$SWE_DIR/results/latest-agent.json" results-agent-)}"
: "${MINI_RID:=$(derive "$BASE_DIR/results/latest-baseline.json" results-baseline-)}"
echo ">> baybo run: $BAYBO_RID"
echo ">> mini run: $MINI_RID"

echo ">> export baybo trajectories"
( cd "$SWE_DIR" && "$PY" export_baybo_trajs.py --run-id "$BAYBO_RID" )
echo ">> export mini trajectories"
( cd "$BASE_DIR" && "$PY" export_trajs.py --run-id "$MINI_RID" )
echo ">> build side-by-side compare.html"
( cd "$BASE_DIR" && "$PY" compare_arms.py \
    --baybo-export "$SWE_DIR/trace/$BAYBO_RID/_export" \
    --mini-export "runs/$MINI_RID/_export" \
    --baybo-results "$SWE_DIR/results/results-agent-$BAYBO_RID.json" \
    --mini-results "results/results-baseline-$MINI_RID.json" \
    --out _compare/compare.html )

OUT="$BASE_DIR/_compare/compare.html"
echo ">> wrote $OUT"
[ "$SERVE" = 1 ] || { echo "(SERVE=0; not serving)"; exit 0; }

# serve on localhost; if a server is already up on the port the file is updated
# in place (http.server reads from disk per request) so a refresh is enough.
if curl -s -o /dev/null "http://127.0.0.1:$PORT/compare.html" 2>/dev/null; then
  echo ">> already serving on :$PORT — just refresh the page"
else
  ( cd "$BASE_DIR/_compare" && nohup "$PY" -m http.server "$PORT" --bind 127.0.0.1 \
      >"/tmp/compare_http.$PORT.log" 2>&1 & )
  sleep 1
  echo ">> serving on 127.0.0.1:$PORT"
fi
cat <<EOF

  view:  ssh -L $PORT:localhost:$PORT <this-host>   then open
         http://localhost:$PORT/compare.html
  stop:  pkill -f "http.server $PORT"
EOF
