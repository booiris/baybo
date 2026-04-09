#!/usr/bin/env bash
set -e

REPO_URL="https://github.com/booiris/cargo-workspace-unused-pub"

if ! command -v cargo-workspace-unused-pub &> /dev/null; then
    echo "[!] cargo-workspace-unused-pub is missing."
    echo "[+] Installing from GitHub..."
    cargo install --git "$REPO_URL"
else
    echo "[*] Checking if cargo-workspace-unused-pub is up to date..."
    LATEST_HASH=$(git ls-remote "$REPO_URL" HEAD 2>/dev/null | awk '{print $1}' | cut -c 1-8 || true)
    # Extract the commit hash from `cargo install --list` output
    CURRENT_HASH=$(cargo install --list | grep "cargo-workspace-unused-pub" | grep -o '#[a-f0-9]*' | head -n 1 | cut -c 2-9 || true)
    
    if [ -n "$LATEST_HASH" ] && [ "$LATEST_HASH" != "$CURRENT_HASH" ]; then
        echo "[!] cargo-workspace-unused-pub is outdated (Current: ${CURRENT_HASH:-unknown}, Latest: $LATEST_HASH)."
        echo "[+] Updating from GitHub..."
        cargo install --git "$REPO_URL"
    else
        echo "[+] cargo-workspace-unused-pub is up to date."
    fi
fi

echo "[+] Running cargo workspace-unused-pub..."
cargo workspace-unused-pub --skip-test-usages  --force-regenerate  -q "$@"
