#!/usr/bin/env bash
set -euo pipefail

violations="$({
  git grep -nE '\.spawn[[:space:]]*\(\)' -- 'crates/**/*.rs' 'bench/**/*.rs' ':!crates/process/**' || true
  git grep -nF '.kill_on_drop(' -- 'crates/**/*.rs' 'bench/**/*.rs' ':!crates/process/**' || true
} | sort -u)"

if [[ -n "$violations" ]]; then
  printf '%s\n' "Raw subprocess ownership bypasses baybo-process:" "$violations" >&2
  exit 1
fi
