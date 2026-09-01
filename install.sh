#!/bin/sh
#
# Baybo installer.
#
#   curl -fsSL https://raw.githubusercontent.com/booiris/baybo/master/install.sh | sh
#
# Downloads a prebuilt binary from the latest GitHub release, verifies its
# checksum, and installs it into ~/.local/bin. It deliberately does NOT run
# `baybo setup` (the wizard needs a TTY on both stdin and stderr, which a pipe
# cannot give it) and does NOT run `baybo gateway install` (that bakes the
# INSTALLING process's PATH into the service unit, and inside `curl | sh` —
# before the rc edit takes effect — ~/.local/bin is not on PATH yet, so the
# daemon would permanently lose bun/node/uv/claude/codex).
#
# It never touches ~/.baybo. That directory holds the encryption key, the secret
# vault and the conversation database.

set -eu

REPO="booiris/baybo"
BIN_NAME="baybo"
# Every target the release workflow publishes. install.sh refuses anything not
# on this list rather than constructing a URL that 404s, or — worse — falling
# back to an artifact for the wrong platform.
SUPPORTED_TARGETS="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin"
OS="$(uname -s)"
# The oldest glibc the published binaries load on, set by the manylinux_2_28
# container the release workflow builds in. Keep in step with
# scripts/release-build.sh.
MIN_GLIBC_MAJOR=2
MIN_GLIBC_MINOR=28

# Overridable so the docker harness (scripts/test-install.sh) can point the whole
# script at a local http.server instead of github.com.
BASE_URL="${BAYBO_BASE_URL:-https://github.com/${REPO}/releases}"

# Tags the block this script appends to a shell rc file, so --uninstall can
# remove exactly that block and leave an identical-looking line written by some
# other installer alone.
RC_MARKER="Added by the baybo installer"

VERSION="${BAYBO_VERSION:-}"
INSTALL_DIR="${BAYBO_INSTALL_DIR:-}"
NO_MODIFY_PATH="${BAYBO_NO_MODIFY_PATH:-}"
SKIP_DEP_CHECK="${BAYBO_SKIP_DEP_CHECK:-}"
DO_UNINSTALL=""

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RED=$(printf '\033[31m'); C_YELLOW=$(printf '\033[33m')
    C_GREEN=$(printf '\033[32m'); C_DIM=$(printf '\033[2m'); C_OFF=$(printf '\033[0m')
else
    C_RED=''; C_YELLOW=''; C_GREEN=''; C_DIM=''; C_OFF=''
fi

# Refuse plaintext and stale TLS on the real download path. The restriction is
# scoped to the URL actually in use rather than hardcoded, because a non-https
# BAYBO_BASE_URL is an explicit local override — scripts/test-install.sh serves
# a fake release over http on loopback.
fetch() { # url out timeout [quiet]
    fetch_url="$1"; fetch_out="$2"; fetch_timeout="$3"; fetch_quiet="${4:-}"
    set --
    case "$fetch_url" in
        https://*) set -- --proto '=https' --tlsv1.2 ;;
    esac
    if [ -n "$fetch_quiet" ]; then set -- "$@" -sS; else set -- "$@" --progress-bar; fi
    curl -fL --retry 3 --max-time "$fetch_timeout" -o "$fetch_out" "$@" "$fetch_url"
}

info() { printf '%s\n' "$*" >&2; }
step() { printf '%s==>%s %s\n' "$C_GREEN" "$C_OFF" "$*" >&2; }
warn() { printf '%swarning:%s %s\n' "$C_YELLOW" "$C_OFF" "$*" >&2; }
err()  { printf '%serror:%s %s\n' "$C_RED" "$C_OFF" "$*" >&2; exit 1; }
has()  { command -v "$1" >/dev/null 2>&1; }

usage() {
    cat >&2 <<EOF
Install baybo.

Usage: install.sh [options]

  --version <tag>    install a specific release (default: latest)
  --bin-dir <dir>    install into <dir> (default: ~/.local/bin)
  --no-modify-path   do not touch your shell rc file
  --uninstall        remove the binary and the PATH entry (keeps ~/.baybo)
  -h, --help         show this message

Environment: BAYBO_VERSION, BAYBO_INSTALL_DIR, BAYBO_NO_MODIFY_PATH,
BAYBO_SKIP_DEP_CHECK, BAYBO_BASE_URL.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2:-}"; [ -n "$VERSION" ] || err "--version needs a tag"; shift 2 ;;
        --bin-dir) INSTALL_DIR="${2:-}"; [ -n "$INSTALL_DIR" ] || err "--bin-dir needs a path"; shift 2 ;;
        --no-modify-path) NO_MODIFY_PATH=1; shift ;;
        --uninstall) DO_UNINSTALL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) usage; err "unknown option: $1" ;;
    esac
done

resolve_install_dir() {
    if [ -n "$INSTALL_DIR" ]; then
        printf '%s\n' "$INSTALL_DIR"
    elif [ -n "${XDG_BIN_HOME:-}" ]; then
        printf '%s\n' "$XDG_BIN_HOME"
    else
        printf '%s\n' "$HOME/.local/bin"
    fi
}

# ---------------------------------------------------------------------------
# uninstall
# ---------------------------------------------------------------------------

uninstall() {
    dir="$(resolve_install_dir)"
    removed=0
    if [ -e "$dir/$BIN_NAME" ]; then
        rm -f "$dir/$BIN_NAME"; step "removed $dir/$BIN_NAME"; removed=1
    fi
    # Only remove the env script if we wrote it. uv installs a file at the very
    # same path, and deleting that would silently break uv's PATH setup.
    if [ -e "$dir/env" ] && grep -q "$RC_MARKER" "$dir/env" 2>/dev/null; then
        rm -f "$dir/env"; step "removed $dir/env"; removed=1
    fi
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        if [ -f "$rc" ] && grep -qF "$RC_MARKER" "$rc" 2>/dev/null; then
            tmp_rc="$(mktemp)"
            # Drop our marker comment and the source line directly after it, and
            # nothing else — the same rc may carry another tool's identical
            # source line.
            awk -v marker="$RC_MARKER" '
                index($0, marker) { drop_next = 1; next }
                drop_next { drop_next = 0; next }
                { print }
            ' "$rc" > "$tmp_rc" && cat "$tmp_rc" > "$rc"
            rm -f "$tmp_rc"
            step "removed the PATH line from $rc"; removed=1
        fi
    done
    [ "$removed" -eq 1 ] || info "nothing to remove in $dir"
    info ""
    info "Your workspace at ~/.baybo was NOT touched — it holds the encryption"
    info "key, the secret vault and your conversations. Remove it by hand if you"
    info "really mean to."
    exit 0
}

[ -n "$DO_UNINSTALL" ] && uninstall

# ---------------------------------------------------------------------------
# platform
# ---------------------------------------------------------------------------

build_from_source_hint() {
    printf '%s' "  git clone https://github.com/${REPO} && cd baybo
  pnpm install
  cargo build --release && cargo install --path crates/baybo

You need rustup, pnpm and bun; see the Requirements table in README.md."
}

detect_target() {
    arch="$(uname -m)"

    case "$OS" in
        Darwin)
            # Under Rosetta on Apple silicon, `uname -m` lies and says x86_64.
            # sysctl does not, and getting this wrong hands an M-series user a
            # missing Intel build instead of the arm64 one that exists.
            if [ "$arch" = "x86_64" ] &&
               [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
                arch=arm64
            fi
            case "$arch" in
                arm64|aarch64) printf 'aarch64-apple-darwin\n' ;;
                *) err "Intel Macs have no prebuilt binary. Build from source:

$(build_from_source_hint)" ;;
            esac
            ;;
        Linux)
            # uname reports arm64 on some systems and aarch64 on others; rustc
            # target triples only ever say aarch64.
            case "$arch" in
                x86_64|amd64) arch=x86_64 ;;
                aarch64|arm64) arch=aarch64 ;;
                *) err "unsupported architecture: ${arch}. Published targets: ${SUPPORTED_TARGETS}" ;;
            esac

            # A glibc binary refuses to load on musl. Check ldd rather than
            # /etc/alpine-release, which misses Void, musl-Gentoo and distroless.
            if (ldd --version 2>&1 || true) | grep -qi musl; then
                err "this looks like a musl system (Alpine, Void, …), and only glibc
binaries are published. Build from source:

$(build_from_source_hint)"
            fi

            printf '%s-unknown-linux-gnu\n' "$arch"
            ;;
        *) err "unsupported OS: ${OS}. Baybo targets Linux and macOS only." ;;
    esac
}

check_glibc() {
    [ "$OS" = "Linux" ] || return 0   # macOS has no glibc floor to check
    ldd_out="$(ldd --version 2>&1 | head -n1 || true)"
    # "ldd (GNU libc) 2.36", "ldd (Ubuntu GLIBC 2.35-0ubuntu3) 2.35"
    ver="$(printf '%s\n' "$ldd_out" | grep -o '[0-9][0-9]*\.[0-9][0-9]*$' || true)"
    [ -n "$ver" ] || return 0  # unrecognised ldd: do not block on a guess

    major="${ver%%.*}"
    minor="${ver#*.}"
    if [ "$major" -lt "$MIN_GLIBC_MAJOR" ] ||
       { [ "$major" -eq "$MIN_GLIBC_MAJOR" ] && [ "$minor" -lt "$MIN_GLIBC_MINOR" ]; }; then
        err "glibc ${ver} is too old; the published binaries need ${MIN_GLIBC_MAJOR}.${MIN_GLIBC_MINOR} or newer.
Build from source, or run the container image instead (deploy/docker/)."
    fi
}

# ---------------------------------------------------------------------------
# runtime dependencies
#
# baybo shells out to other programs. Two of them decide whether it starts at
# all, so they are gates; the rest degrade quietly — often with the warning
# going only to ~/.baybo/logs/, where a first-time user will never look. That
# silence is the whole reason this section exists.
#
# None of them are installed for you on purpose: baybo re-resolves each one from
# PATH on every spawn attempt, so installing bun an hour from now fixes the
# sidecars without restarting anything.
# ---------------------------------------------------------------------------

check_runtime_deps() {
    if [ -n "$SKIP_DEP_CHECK" ]; then
        warn "BAYBO_SKIP_DEP_CHECK is set; not checking runtime dependencies"
        return 0
    fi

    # baybo `git init`s three identity repos during startup, before almost any
    # subcommand runs. Without git the first command a new user types dies with
    # a raw "spawn `git init …`" error that explains nothing.
    has git || err "git is required — baybo creates its workspace identity repos at
startup and cannot run without it. Install it (apt install git / dnf install
git) and re-run. Override with BAYBO_SKIP_DEP_CHECK=1 if you know better."

    # macOS keeps trust in the keychain, so there is no bundle path to look for.
    if [ "$OS" = "Linux" ]; then
        ca_found=""
        for ca in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt \
                  /etc/ssl/ca-bundle.pem /etc/ssl/cert.pem /etc/ssl/certs; do
            [ -e "$ca" ] && { ca_found=1; break; }
        done
        [ -n "$ca_found" ] || warn "no system CA bundle found. Baybo aborts (SIGABRT, with a core dump and no
  useful message) when the trust store is empty. Install ca-certificates."
    fi

    # rg and uv are spawned by name, so PATH is the whole story for them. bun and
    # node are not: crates/process/src/host_tool.rs resolves those through an env
    # override, then PATH, then ~/.local/bin, then ~/.bun/bin. Checking only PATH
    # here would warn that Telegram will never start on a machine where the
    # daemon finds bun perfectly well — bun's own installer puts it in ~/.bun/bin
    # and adds that to PATH only for login shells.
    has_hosttool() {
        [ -n "$2" ] && [ -x "$2" ] && return 0
        command -v "$1" >/dev/null 2>&1 && return 0
        [ -x "$HOME/.local/bin/$1" ] && return 0
        [ -x "$HOME/.bun/bin/$1" ] && return 0
        return 1
    }

    has rg   || warn "ripgrep (rg) not found — the agent's Grep and Glob tools will fail."
    has_hosttool bun "${BAYBO_BUN_BIN:-}" || warn "bun not found — Telegram/WeChat sidecars and deck cards will never start
  (they retry silently in the background). See https://bun.sh."
    has_hosttool node "${BAYBO_NODE_BIN:-}" || warn "node not found — the browser tool sidecar will retry-loop forever, and
  the default setup wizard enables the browser tool."
    has uv   || warn "uv not found — the agent's shell rewrites python to \`uv run python\`, so
  every python call will fail with exit 127. See https://docs.astral.sh/uv."
    # macOS always has sandbox-exec; only Linux can end up with no backend.
    if [ "$OS" = "Linux" ] && ! has bwrap && ! has docker; then
        warn "no sandbox backend (bwrap or docker) — the agent's shell commands run
  on the host with no OS isolation."
    fi
}

# ---------------------------------------------------------------------------
# download + install
# ---------------------------------------------------------------------------

main() {
    for cmd in curl tar mktemp uname install grep; do
        has "$cmd" || err "$cmd is required but not installed"
    done

    target="$(detect_target)"
    check_glibc
    check_runtime_deps

    asset="${BIN_NAME}-${target}.tar.gz"
    if [ -n "$VERSION" ]; then
        url="${BASE_URL}/download/${VERSION}/${asset}"
        sums_url="${BASE_URL}/download/${VERSION}/SHA256SUMS"
    else
        # The /releases/latest/download/ path is served by github.com and never
        # touches api.github.com, whose unauthenticated limit is 60 requests per
        # hour per IP — which a shared office NAT or a CI fleet burns in minutes.
        url="${BASE_URL}/latest/download/${asset}"
        sums_url="${BASE_URL}/latest/download/SHA256SUMS"
    fi

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    step "downloading ${asset}"
    if ! fetch "$url" "$tmp/$asset" 300; then
        err "download failed: ${url}
If this is a fresh checkout of a repo with no releases yet, there is nothing to
install — build from source instead (see README.md)."
    fi

    # The checksum gate fails CLOSED. This script is served from a mutable
    # branch while the tarball it fetches comes from a frozen release, and what
    # separates the projects that survive that pairing (chezmoi, k3s) from the
    # ones that merely get away with it (cargo-binstall, zoxide, starship — none
    # of which verify anything) is exactly this: verifying against a digest
    # published under the SAME release. A warn-and-continue turns that into
    # decoration, because whoever can serve you a doctored tarball can usually
    # also make the sums file 404.
    fetch "$sums_url" "$tmp/SHA256SUMS" 60 quiet 2>/dev/null || err "could not fetch SHA256SUMS from ${sums_url}
Every release publishes it, so this is either a broken download or a release
that should not be trusted. Re-run to retry."

    step "verifying checksum"
    expected="$(grep " ${asset}\$" "$tmp/SHA256SUMS" | awk '{print $1}')"
    [ -n "$expected" ] || err "SHA256SUMS has no entry for ${asset}.
The release is incomplete or the asset was renamed; refusing to install
something unverified."

    # No hashing tool is the one case that stays a warning: it is a genuine
    # minimal-image situation and nothing an attacker gets to choose.
    if has sha256sum; then
        actual="$(sha256sum "$tmp/$asset" | awk '{print $1}')"
    elif has shasum; then
        actual="$(shasum -a 256 "$tmp/$asset" | awk '{print $1}')"
    else
        actual=""
        warn "neither sha256sum nor shasum found; cannot verify the download."
    fi
    if [ -n "$actual" ] && [ "$actual" != "$expected" ]; then
        err "checksum mismatch for ${asset}
  expected ${expected}
  actual   ${actual}
Refusing to install. This is a corrupted download, or a tampered one."
    fi

    tar -xzf "$tmp/$asset" -C "$tmp"
    [ -f "$tmp/$BIN_NAME" ] || err "archive did not contain a '${BIN_NAME}' binary"

    dir="$(resolve_install_dir)"
    mkdir -p "$dir"
    install -m 0755 "$tmp/$BIN_NAME" "$dir/$BIN_NAME"
    step "installed ${dir}/${BIN_NAME}"

    # An older `cargo install --path crates/baybo` puts a binary in ~/.cargo/bin.
    # Depending on rc ordering, that one can win — so the upgrade silently does
    # nothing and the user keeps running the old build.
    existing="$(command -v "$BIN_NAME" 2>/dev/null || true)"
    if [ -n "$existing" ] && [ "$existing" != "$dir/$BIN_NAME" ]; then
        warn "another ${BIN_NAME} is earlier on your PATH and will shadow this one:
  ${existing}
  Remove it (e.g. cargo uninstall baybo) or fix the PATH order."
    fi

    installed_version="$("$dir/$BIN_NAME" --version 2>/dev/null || true)"
    [ -n "$installed_version" ] || err "installed binary does not run — see the
Requirements section in README.md. This is usually a glibc mismatch."

    setup_path "$dir"

    info ""
    step "${installed_version}"
    info ""
    info "Next steps:"
    # Only point at the env script when one was actually written — with
    # --no-modify-path it never is, and telling someone to source a file that
    # does not exist is how a first run ends in confusion.
    if [ -f "$dir/env" ]; then
        info "  1. ${C_DIM}. \"${dir}/env\"${C_OFF}   (or open a new shell)"
    else
        info "  1. put ${C_DIM}${dir}${C_OFF} on your PATH"
    fi
    info "  2. ${C_DIM}baybo setup${C_OFF}         first-run wizard — interactive, needs a terminal"
    info "  3. ${C_DIM}baybo gateway install${C_OFF} && ${C_DIM}baybo gateway enable${C_OFF}   run it as a service"
    info ""
    info "Upgrading later means re-running this script; a running gateway keeps"
    info "the old code until ${C_DIM}baybo gateway restart${C_OFF}."
}

setup_path() {
    dir="$1"

    case ":${PATH}:" in
        *":${dir}:"*) return 0 ;;
    esac

    if [ -n "$NO_MODIFY_PATH" ]; then
        warn "${dir} is not on your PATH; add it yourself (--no-modify-path was given)"
        return 0
    fi

    # One env script plus one guarded source line, so re-running the installer
    # is idempotent instead of appending another PATH export every time.
    # uv writes a file at this same path; leave a foreign one alone, since it
    # already does the one thing we need it to do.
    if [ ! -e "$dir/env" ] || grep -q "$RC_MARKER" "$dir/env" 2>/dev/null; then
        cat > "$dir/env" <<EOF
#!/bin/sh
# ${RC_MARKER}.
case ":\${PATH}:" in
    *":${dir}:"*) ;;
    *) export PATH="${dir}:\$PATH" ;;
esac
EOF
        chmod 0644 "$dir/env"
    fi

    if [ -n "${CI:-}" ] || [ ! -t 1 ]; then
        warn "not editing any shell rc file (non-interactive); run: . \"${dir}/env\""
        return 0
    fi

    case "${SHELL:-}" in
        */zsh) rc="$HOME/.zshrc" ;;
        */bash) rc="$HOME/.bashrc" ;;
        */fish)
            warn "fish detected; add this to config.fish yourself:
  fish_add_path ${dir}"
            return 0 ;;
        *) rc="$HOME/.profile" ;;
    esac

    line=". \"${dir}/env\""
    if [ -f "$rc" ] && grep -qF "$line" "$rc"; then
        return 0
    fi
    printf '\n# %s.\n%s\n' "$RC_MARKER" "$line" >> "$rc"
    step "added ${dir} to your PATH in ${rc}"
}

main
