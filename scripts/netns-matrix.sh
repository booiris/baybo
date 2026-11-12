#!/usr/bin/env bash
# The direct-carrier NAT matrix: real C, A and P processes, each in its own
# network namespace behind a NAT profile (docs/modules/mobile/direct-carriers.md
# § Testing). Builds the three binaries with `test-support`, then runs the
# `#[ignore]`d test as root. Only the finished test binary runs as root: cargo
# under sudo gets root's rustup and the repo's sccache `rustc-wrapper`, not this
# shell's toolchain or a CI override of it. Needs `ip`, `iptables`, `ip6tables`,
# `tc` (with `sch_netem` for the loss cell) and `sqlite3`.
#
#   scripts/netns-matrix.sh                         # every cell, 5 runs each
#   NETNS_CELLS="cone/cone open/symmetric" NETNS_RUNS=1 NETNS_IDLE_SECS=40 \
#     scripts/netns-matrix.sh                       # a subset
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for tool in ip iptables ip6tables tc sqlite3; do
  command -v "$tool" >/dev/null || { echo "netns-matrix: $tool not found" >&2; exit 1; }
done

BAYBO_SKIP_WEBUI=1 cargo build -p baybo --features baybo-gateway/test-support
BAYBO_SKIP_WEBUI=1 cargo build -p baybo-gateway --features test-support --example seed_relay_binding
(cd remote-host && cargo build -p remote-host --features remote-host-protocol/test-support)
(cd app/ios && cargo build -p baybo-ios-ffi --features test-support --example netns_phone)
MATRIX_BIN="$(cd app/ios && cargo test -p baybo-ios-ffi --features test-support \
  --test netns_matrix --no-run --message-format=json \
  | sed -n 's/.*"name":"netns_matrix".*"executable":"\([^"]*\)".*/\1/p' | tail -n1)"
[ -x "$MATRIX_BIN" ] || { echo "netns-matrix: test binary not found" >&2; exit 1; }

export NETNS_GATEWAY_BIN="$ROOT/target/debug/baybo"
export NETNS_SEED_BIN="$ROOT/target/debug/examples/seed_relay_binding"
export NETNS_RELAY_BIN="$ROOT/remote-host/target/debug/remote-host"
export NETNS_PHONE_BIN="$ROOT/app/ios/target/debug/examples/netns_phone"
export NETNS_SQLITE3_BIN="$(command -v sqlite3)"

# sudo on GitHub's runners refuses `-E`, so hand the test its NETNS_* knobs
# (the binaries above plus any NETNS_CELLS / NETNS_RUNS / NETNS_IDLE_SECS)
# through `env` instead.
NETNS_VARS=()
while IFS= read -r var; do NETNS_VARS+=("$var"); done < <(env | grep '^NETNS_')
cd app/ios/ffi
sudo env "PATH=$PATH" "${NETNS_VARS[@]}" "$MATRIX_BIN" --ignored --test-threads=1 --nocapture
