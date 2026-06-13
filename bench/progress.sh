#!/usr/bin/env bash
# Tiny cross-bench progress indicator. Source it, then:
#
#   progress_render <label> <total> <done> <start_epoch>   # print one line; total=0 -> count-only
#   progress_start  <label> <total> <count_cmd>            # background poller; count_cmd echoes an int
#   progress_stop                                          # stop the poller started above
#
# It re-prints a fresh ASCII-bar LINE every PROGRESS_INTERVAL seconds (no \r redraw),
# so it coexists with the bins' tracing logs and the external harness UIs instead of
# being clobbered by them. Output is on stderr and gated on an interactive stderr, so
# piped / CI runs stay quiet. Self-hosted benches that know their total get a real
# percentage bar; harness benches (tb/Harbor) that don't get a count + elapsed.
: "${PROGRESS_INTERVAL:=15}"
_PROGRESS_PID=""

progress_render() {  # <label> <total> <done> <start_epoch>
  [ -t 2 ] || return 0
  local label="$1" total="$2" done="${3:-0}" start="$4" el pct fill bar
  done=${done//[^0-9]/}; done=${done:-0}
  el=$(( $(date +%s) - start ))
  if [ "${total:-0}" -gt 0 ] 2>/dev/null; then
    pct=$(( done * 100 / total )); pct=$(( pct > 100 ? 100 : pct ))
    fill=$(( pct * 24 / 100 ))
    bar="$(printf '%*s' "$fill" '' | tr ' ' '#')$(printf '%*s' $(( 24 - fill )) '' | tr ' ' '.')"
    printf '>> [%s] [%s] %d/%d %d%% · %dm%02ds\n' \
      "$label" "$bar" "$done" "$total" "$pct" $((el/60)) $((el%60)) >&2
  else
    printf '>> [%s] %d done · %dm%02ds\n' "$label" "$done" $((el/60)) $((el%60)) >&2
  fi
}

progress_start() {  # <label> <total> <count_cmd>
  [ -t 2 ] || return 0
  local label="$1" total="$2" count_cmd="$3" start
  start=$(date +%s)
  (
    while :; do
      d=$(eval "$count_cmd" 2>/dev/null || echo 0)
      progress_render "$label" "$total" "$d" "$start"
      sleep "$PROGRESS_INTERVAL"
    done
  ) &
  _PROGRESS_PID=$!
}

progress_stop() {
  [ -n "$_PROGRESS_PID" ] || return 0
  kill "$_PROGRESS_PID" 2>/dev/null || true
  wait "$_PROGRESS_PID" 2>/dev/null || true
  _PROGRESS_PID=""
}
