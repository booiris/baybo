#!/usr/bin/env bash
#
# Exercise install.sh against a FAKE release served from localhost, inside real
# distro containers.
#
# install.sh is the one file in this repo that no PR gate can run end to end:
# until a release exists it has nothing to install, and after one exists the
# only honest test is a machine that is not this one. This harness closes most
# of that gap locally — the CI `verify-install` job covers the rest (the real
# github.com `latest` hop, which nothing here can fake).
#
# Usage: scripts/test-install.sh [distro …]      (default: all of them)
#
# shellcheck disable=SC2016
# The single-quoted snippets below are shell programs for the CONTAINER to run.
# $HOME must expand there, not here.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PORT="${PORT:-8731}"
TAG="v0.0.0-harness"
DISTROS=("$@")
if [ ${#DISTROS[@]} -eq 0 ]; then
    DISTROS=(debian12 ubuntu2204 ubi9 alpine nogit)
fi

pass=0; fail=0
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; pass=$((pass + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; fail=$((fail + 1)); }
head_() { printf '\n\033[1m%s\033[0m\n' "$*"; }

# --- fake release -----------------------------------------------------------

serve_root="$(mktemp -d)"
trap 'rm -rf "$serve_root"; [ -n "${http_pid:-}" ] && kill "$http_pid" 2>/dev/null || true' EXIT

build_fake_release() {
    local stage; stage="$(mktemp -d)"
    cat > "$stage/baybo" <<'EOF'
#!/bin/sh
[ "$1" = "--version" ] && echo "baybo 0.0.0-harness" && exit 0
echo "stub baybo: $*"
EOF
    chmod 0755 "$stage/baybo"

    local dl="$serve_root/latest/download"
    mkdir -p "$dl" "$serve_root/download/$TAG"
    for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
        tar -czf "$dl/baybo-${target}.tar.gz" -C "$stage" baybo
    done
    ( cd "$dl" && sha256sum ./*.tar.gz | sed 's|\./||' > SHA256SUMS )
    cp "$dl"/* "$serve_root/download/$TAG/"
    rm -rf "$stage"
}

build_fake_release
python3 -m http.server "$PORT" --directory "$serve_root" >/dev/null 2>&1 &
http_pid=$!
sleep 0.5

# --- images -----------------------------------------------------------------
#
# Prebaked so the loop is a container start, not an apt run: measured ~50x
# faster than installing curl on every iteration.

ensure_image() {
    local name="$1" base="$2" prep="$3"
    docker image inspect "baybo-install-test:$name" >/dev/null 2>&1 && return 0
    printf 'baking baybo-install-test:%s …\n' "$name" >&2
    docker build -q -t "baybo-install-test:$name" -f - . >/dev/null <<EOF
FROM $base
RUN $prep
EOF
}

APT_PREP='apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates git && rm -rf /var/lib/apt/lists/*'

case " ${DISTROS[*]} " in *" debian12 "*)  ensure_image debian12  debian:12    "$APT_PREP" ;; esac
case " ${DISTROS[*]} " in *" ubuntu2204 "*) ensure_image ubuntu2204 ubuntu:22.04 "$APT_PREP" ;; esac
# ubi9 already ships curl-minimal, which owns /usr/bin/curl — asking for `curl`
# on top of it is a package conflict, not an upgrade.
case " ${DISTROS[*]} " in *" ubi9 "*)      ensure_image ubi9      redhat/ubi9  'dnf install -y -q git ca-certificates && dnf clean all' ;; esac
case " ${DISTROS[*]} " in *" alpine "*)    ensure_image alpine    alpine:latest 'apk add --no-cache curl ca-certificates git' ;; esac
# Deliberately no git: install.sh must refuse rather than install a binary whose
# very first command dies on `spawn git init`.
case " ${DISTROS[*]} " in *" nogit "*)     ensure_image nogit     debian:12    'apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/*' ;; esac

# RUN_TTY=1 allocates a pty, which is the only way to reach install.sh's rc-file
# editing: it skips that when stdout is not a terminal, precisely so a piped
# `curl | sh` in CI does not rewrite someone's shell config.
run_in() {
    local image="$1"; shift
    docker run --rm --network host ${RUN_TTY:+-t} \
        -e "BAYBO_BASE_URL=http://127.0.0.1:${PORT}" \
        -v "$REPO_ROOT/install.sh:/install.sh:ro" \
        "baybo-install-test:$image" sh -c "$*" 2>&1
}

# --- cases ------------------------------------------------------------------

expect_install() {
    local image="$1"
    head_ "$image — installs and runs"
    local out
    if out="$(run_in "$image" 'sh /install.sh --no-modify-path && $HOME/.local/bin/baybo --version')"; then
        if printf '%s' "$out" | grep -q '0.0.0-harness'; then
            ok "$image installed and the binary runs"
        else
            bad "$image installed but --version did not report the harness build"
            printf '%s\n' "$out" | sed 's/^/    /'
        fi
    else
        bad "$image install failed"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi

    head_ "$image — re-run is idempotent"
    if out="$(run_in "$image" 'sh /install.sh --no-modify-path >/dev/null && sh /install.sh --no-modify-path && $HOME/.local/bin/baybo --version')"; then
        ok "$image second install succeeded"
    else
        bad "$image second install failed"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi

    head_ "$image — uninstall removes the binary, the env file and the rc line"
    # Seeds an unrelated `. env` line first: uninstall must remove only its own
    # marked block. uv writes an identical-looking line to the very same path.
    if out="$(RUN_TTY=1 run_in "$image" '
        export SHELL=/bin/bash; unset CI
        printf "%s\n" ". \"\$HOME/.local/bin/env\"  # someone else" > $HOME/.bashrc
        sh /install.sh 2>/dev/null >/dev/null
        sh /install.sh --uninstall 2>/dev/null >/dev/null
        test -e $HOME/.local/bin/baybo && { echo "binary survived"; exit 1; }
        test -e $HOME/.local/bin/env && { echo "env file survived"; exit 1; }
        grep -q "someone else" $HOME/.bashrc || { echo "clobbered a foreign rc line"; exit 1; }
        test "$(grep -c "local/bin/env" $HOME/.bashrc)" = "1" || { echo "own rc line survived"; exit 1; }
        echo clean')"; then
        ok "$image uninstall was complete and left the foreign rc line alone"
    else
        bad "$image uninstall incomplete"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi

    head_ "$image — PATH edit happens once, not once per upgrade"
    local n out2
    # A pty (RUN_TTY) plus an empty CI is what makes install.sh willing to touch
    # a shell rc file at all. Without both, this test silently measures nothing.
    if out2="$(RUN_TTY=1 run_in "$image" '
        export SHELL=/bin/bash; unset CI; touch $HOME/.bashrc
        # Silence stderr, NOT stdout: install.sh logs to stderr, and redirecting
        # stdout would make its own [ -t 1 ] check false and skip the rc edit.
        sh /install.sh 2>/dev/null
        sh /install.sh 2>/dev/null
        printf "rc-lines=%s\n" "$(grep -c "local/bin/env" $HOME/.bashrc || true)"')"; then
        n="$(printf '%s' "$out2" | tr -d '\r' | sed -n 's/.*rc-lines=\([0-9]*\).*/\1/p')"
        if [ "${n:-0}" = "1" ]; then
            ok "$image wrote exactly one PATH line across two installs"
        else
            bad "$image wrote ${n:-?} PATH lines across two installs (want 1)"
            printf '%s\n' "$out2" | sed 's/^/    /'
        fi
    else
        bad "$image PATH idempotency check errored"
        printf '%s\n' "$out2" | sed 's/^/    /'
    fi
}

expect_refusal() {
    local image="$1" why="$2" needle="$3"
    head_ "$image — refuses: $why"
    local out
    if out="$(run_in "$image" 'sh /install.sh --no-modify-path')"; then
        bad "$image installed anyway (expected a refusal: $why)"
        printf '%s\n' "$out" | sed 's/^/    /'
    elif printf '%s' "$out" | grep -qi "$needle"; then
        ok "$image refused with the right reason"
    else
        bad "$image refused, but not for the expected reason ($needle)"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
}

# The checksum gate fails closed, which means the interesting assertion is that
# a BROKEN release is refused — a gate that is only ever exercised on good input
# proves nothing. Both cases mutate the served release and restore it after.
expect_checksum_refusal() {
    local dl="$serve_root/latest/download"

    head_ "checksum gate — refuses when SHA256SUMS is missing"
    mv "$dl/SHA256SUMS" "$dl/SHA256SUMS.bak"
    local out
    if out="$(run_in debian12 'sh /install.sh --no-modify-path')"; then
        bad "installed with no SHA256SUMS published"
    elif printf '%s' "$out" | grep -q 'could not fetch SHA256SUMS'; then
        ok "refused when SHA256SUMS is missing"
    else
        bad "refused, but not for the missing-SHA256SUMS reason"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
    mv "$dl/SHA256SUMS.bak" "$dl/SHA256SUMS"

    head_ "checksum gate — refuses a tampered tarball"
    cp "$dl/SHA256SUMS" "$dl/SHA256SUMS.bak"
    # Flip the recorded digest: same effect as the tarball having been swapped.
    sed 's/^[0-9a-f]\{64\}/0000000000000000000000000000000000000000000000000000000000000000/' \
        "$dl/SHA256SUMS.bak" > "$dl/SHA256SUMS"
    if out="$(run_in debian12 'sh /install.sh --no-modify-path')"; then
        bad "installed a tarball whose digest did not match"
    elif printf '%s' "$out" | grep -q 'checksum mismatch'; then
        ok "refused a tarball whose digest did not match"
    else
        bad "refused, but not for the checksum-mismatch reason"
        printf '%s\n' "$out" | sed 's/^/    /'
    fi
    mv "$dl/SHA256SUMS.bak" "$dl/SHA256SUMS"
}

for d in "${DISTROS[@]}"; do
    case "$d" in
        debian12|ubuntu2204|ubi9) expect_install "$d" ;;
        alpine) expect_refusal alpine "musl is not a published target" "musl" ;;
        nogit)  expect_refusal nogit  "git is required at startup"     "git is required" ;;
        *) bad "unknown distro: $d" ;;
    esac
done

case " ${DISTROS[*]} " in *" debian12 "*) expect_checksum_refusal ;; esac

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
