#!/usr/bin/env bash
#
# Build a release `baybo` inside a manylinux_2_28 container and emit a tarball.
#
# WHY A CONTAINER AND NOT THE RUNNER. The glibc floor of a Rust binary is a
# property of the machine that linked it, and it is invisible to every gate this
# repo has: it is not a compile error, not a clippy lint, not a test failure.
# The binary simply fails to load, in the dynamic linker, before `main` — so no
# panic hook, log line or error path in this codebase ever sees it. Measured
# floors: a build on `ubuntu-latest` (24.04) needs GLIBC 2.39 and fails to load
# on RHEL 9, Amazon Linux 2023, Debian 12 and Ubuntu 22.04; `rust:bookworm`
# needs 2.36 and still excludes the whole 2.34 cluster (RHEL/Rocky/Alma 9,
# AL2023) plus Ubuntu 22.04. manylinux_2_28 lands at 2.28 and loads on every
# distro tested. The release workflow re-checks this by executing the artifact
# in a ubi9 container; do not remove that step.
#
# Runs as root inside the container with the repo bind-mounted at $PWD.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT_DIR="${OUT_DIR:-$REPO_ROOT/dist}"

# The JS toolchain pins live in deploy/docker/Dockerfile and are read from it, so
# the shipped container and the shipped tarball cannot drift onto different
# versions of the thing that bakes the dashboard and the sidecars in. The Rust
# pin is not here because rust-toolchain.toml is already its one home — rustup
# reads it on the first cargo invocation.
dockerfile_arg() {
    local name="$1" value
    value="$(sed -n "s/^ARG ${name}=\(.*\)\$/\1/p" deploy/docker/Dockerfile | head -n1)"
    if [ -z "$value" ]; then
        echo "release-build: ARG ${name} not found in deploy/docker/Dockerfile" >&2
        exit 1
    fi
    printf '%s\n' "$value"
}

NODE_VERSION="$(dockerfile_arg NODE_VERSION)"
BUN_VERSION="$(dockerfile_arg BUN_VERSION)"
PNPM_VERSION="$(dockerfile_arg PNPM_VERSION)"

# Resolving the pins is the one thing here that can break from an edit to a
# FILE THIS SCRIPT DOES NOT OWN, and it would break at release time, 25 minutes
# into a build. `--print-pins` lets CI prove the coupling still holds whenever
# either file changes — dockerfile_arg above already exits non-zero on a miss.
if [ "${1:-}" = "--print-pins" ]; then
    echo "NODE_VERSION=${NODE_VERSION}"
    echo "BUN_VERSION=${BUN_VERSION}"
    echo "PNPM_VERSION=${PNPM_VERSION}"
    exit 0
fi

case "$(uname -s)/$(uname -m)" in
    Linux/x86_64)   RUST_TARGET="x86_64-unknown-linux-gnu";  NODE_PLAT="linux-x64"    ;;
    Linux/aarch64)  RUST_TARGET="aarch64-unknown-linux-gnu"; NODE_PLAT="linux-arm64"  ;;
    Darwin/arm64)   RUST_TARGET="aarch64-apple-darwin";      NODE_PLAT="darwin-arm64" ;;
    *) echo "release-build: unsupported build host $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac

log() { echo "==> $*"; }

log "target ${RUST_TARGET}, node ${NODE_VERSION}, bun ${BUN_VERSION}, pnpm ${PNPM_VERSION}"

# The tracked .cargo/config.toml pins sccache as the rustc wrapper for local
# development. It is absent on a build host, and cargo hard-fails when its
# configured wrapper is not executable, so clear it — an empty value overrides
# the config file. (deploy/docker/Dockerfile has the same hazard and no such
# line, which is why that image does not build on a clean machine.)
export RUSTC_WRAPPER=""

# Every runner sets this, but a maintainer rehearsing the build locally does
# not — and pnpm 9 PROMPTS ("The modules directories will be removed and
# reinstalled from scratch. Proceed?") when it finds a node_modules laid down by
# a different pnpm major. A release build must never depend on a default being
# taken at a prompt nobody is watching.
export CI=1

# Resolve a concrete node release from the major pinned in the Dockerfile.
node_tarball_url() {
    local series="${NODE_VERSION%%.*}" full
    full="$(curl -fsSL https://nodejs.org/dist/index.json \
        | grep -o "\"version\":\"v${series}\.[0-9.]*\"" | head -n1 | cut -d'"' -f4)"
    if [ -z "$full" ]; then
        echo "release-build: no node ${series}.x release found upstream" >&2
        exit 1
    fi
    printf 'https://nodejs.org/dist/%s/node-%s-%s.tar.xz\n' "$full" "$full" "$NODE_PLAT"
}

install_bun() {
    if [ "$(bun --version 2>/dev/null)" = "$BUN_VERSION" ]; then return 0; fi
    curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" >/dev/null
}

# Provisioning is the ONE thing that genuinely differs between the two hosts —
# a root container with dnf versus a runner with Xcode already on it — so the
# two bodies stay separate instead of collapsing into an `if` inside one
# procedure. Everything after this point is shared.

provision_linux() {
    log "system packages"
    dnf install --assumeyes --quiet \
        clang cmake git openssl-devel perl pkgconfig unzip xz >/dev/null

    log "rust toolchain (version from rust-toolchain.toml)"
    # --default-toolchain none: rust-toolchain.toml is the single source of the
    # version, and rustup honours it on the first cargo invocation. Without this
    # the installer also downloads whatever `stable` happens to be that day — a
    # toolchain this build never uses.
    curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none >/dev/null
    export PATH="/root/.cargo/bin:$PATH"

    log "node ${NODE_VERSION}"
    curl -fsSL "$(node_tarball_url)" | tar -xJ -C /usr/local --strip-components=1

    log "pnpm ${PNPM_VERSION}"
    npm install --global --silent "pnpm@${PNPM_VERSION}"

    log "bun ${BUN_VERSION}"
    install_bun
    export PATH="/root/.bun/bin:$PATH"
}

provision_macos() {
    # Xcode's clang and SDK are already present on a macos runner, and rustup
    # honours rust-toolchain.toml, so only the JS toolchain needs pinning here.
    # It goes in a cache prefix rather than over whatever the host ships:
    # pnpm 10 stopped running dependency build scripts by default, which is
    # exactly the kind of difference that silently changes what gets embedded.
    local prefix="$HOME/.cache/baybo-release-toolchain"
    mkdir -p "$prefix"

    # `[ ]` does not glob, so match the pinned major with a case instead.
    case "$("$prefix/bin/node" --version 2>/dev/null)" in
        "v${NODE_VERSION}."*) ;;
        *) log "node ${NODE_VERSION}"
           curl -fsSL "$(node_tarball_url)" | tar -xJ -C "$prefix" --strip-components=1 ;;
    esac
    export PATH="$prefix/bin:$PATH"

    if [ "$(pnpm --version 2>/dev/null)" != "$PNPM_VERSION" ]; then
        log "pnpm ${PNPM_VERSION}"
        npm install --global --silent "pnpm@${PNPM_VERSION}"
    fi

    log "bun ${BUN_VERSION}"
    install_bun
    export PATH="$HOME/.bun/bin:$PATH"

    command -v rustup >/dev/null 2>&1 || {
        curl -fsSL --proto '=https' --tlsv1.2 https://sh.rustup.rs \
            | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none >/dev/null
        export PATH="$HOME/.cargo/bin:$PATH"
    }
}

case "$(uname -s)" in
    Linux)  provision_linux  ;;
    Darwin) provision_macos  ;;
esac

cargo --version
node --version
pnpm --version
bun --version

log "pnpm install"
pnpm install --frozen-lockfile

log "cargo build --release"
# BAYBO_REQUIRE_SIDECARS=1 makes sidecar packaging failures fatal instead of a
# `cargo:warning` nobody reads. It is NOT a complete guarantee: build.rs still
# treats a missing entrypoint as a bare warning, and an unreadable sidecars/
# directory yields an empty asset table silently. The embedded-asset assertion
# after the build is what actually checks that the sidecars shipped.
#
# BAYBO_SKIP_WEBUI must stay unset: setting it swaps the real dashboard for a
# placeholder page. Both Rust jobs in ci.yml set it, so this is the only place
# in the repo that compiles the shape a user actually receives.
BAYBO_REQUIRE_SIDECARS=1 cargo build --release --locked -p baybo

log "assert the embedded assets are real"
# BAYBO_REQUIRE_SIDECARS=1 asserts that nothing it *discovered* failed. These
# assert that the things we expect were discovered at all — the gap that lets a
# green build ship a binary whose Telegram/WeChat/deck-card services never
# start, and whose `/` is a "not built" stub. Both failure modes are invisible
# in the binary's size (the two channel bundles are ~0.18% of it) and neither
# produces a non-zero exit anywhere upstream.
for bundle in \
    sidecars/channel/telegram/dist/bundle.mjs \
    sidecars/channel/weixin/dist/bundle.mjs \
    sidecars/tool/browser/dist/bundle.mjs
do
    if [ ! -s "$bundle" ]; then
        echo "release-build: ${bundle} missing or empty — sidecar assets embedded empty" >&2
        exit 1
    fi
done
if grep -q 'Baybo WebUI not built' app/web/dist/index.html; then
    echo "release-build: the placeholder dashboard was embedded, not the real one" >&2
    exit 1
fi

log "package"
mkdir -p "$OUT_DIR"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
install -m 0755 target/release/baybo "$staging/baybo"
# The release profile only strips debuginfo; a full strip is another ~7 MiB off
# a ~55 MiB binary and costs nothing a user wants. On arm64 macOS a Mach-O must
# carry a valid signature to execute at all, and `strip` rewrites the binary —
# Apple's strip re-signs ad-hoc on the spot (measured: the code-signing
# identifier changes and the binary still runs), so no explicit `codesign` step
# is needed. Verified rather than assumed, because the failure mode if it were
# wrong is a binary that dies on SIGKILL with no message at all.
strip "$staging/baybo"
"$staging/baybo" --version >/dev/null || {
    echo "release-build: the stripped binary does not execute" >&2
    exit 1
}

tarball="$OUT_DIR/baybo-${RUST_TARGET}.tar.gz"
# Bare binary at the tar root: install.sh then extracts without globbing a
# version-stamped directory out of the path.
tar -czf "$tarball" -C "$staging" baybo
# macOS has no sha256sum; `shasum -a 256` emits the same "<hash>  <name>" shape
# that install.sh greps for, so the SHA256SUMS file is identical either way.
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$@"; }
else
    sha256() { shasum -a 256 "$@"; }
fi
( cd "$OUT_DIR" && sha256 "$(basename "$tarball")" > "$(basename "$tarball").sha256" )

log "built $(du -h "$tarball" | cut -f1) -> $tarball"
