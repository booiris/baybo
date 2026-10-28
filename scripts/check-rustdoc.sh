#!/usr/bin/env bash
# Rustdoc gate: every intra-doc link resolves, no stray HTML, no redundant
# explicit link targets. Extra cargo flags (e.g. `--locked`) pass through.
#
# Private items are documented on purpose — these docs are for maintainers,
# and most links point at `pub(crate)` items — so the one rustdoc lint that
# would complain about that, `private_intra_doc_links`, is allowed. `--lib`
# keeps the bench crates' identically named `run` bins from colliding in the
# output directory; bins carry no doc surface worth gating.
set -euo pipefail
cd "$(dirname "$0")/.."

RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" \
  exec cargo doc --workspace --no-deps --lib --all-features --document-private-items "$@"
