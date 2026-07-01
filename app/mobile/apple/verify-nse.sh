#!/usr/bin/env bash
#
# verify-nse.sh — end-to-end proof that the iOS NSE decrypts a real push on the
# Simulator, using the pinned cross-language fixture from `device_proto::fixtures`.
#
# WHY a script you run (not CI): the App Group keychain the NSE reads is gated by
# the `com.apple.security.application-groups` entitlement, and signing needs your
# Apple Development identity (its private key requires interactive Touch ID /
# password), so this can't run headlessly.
#
# IMPORTANT — App Group *provisioning*: the simulator only launches an app whose
# App Group is REGISTERED to the signing team. Manual codesign (what this script
# does) cannot register an App Group — only Xcode automatic signing can, and App
# Groups are a paid Apple Developer capability. So unless group.com.baybo.app is
# already provisioned for your team, the launch is denied (the script detects
# this and prints the Xcode-automatic-signing path). Re-signing also requires the
# `com.apple.security.get-task-allow` entitlement or the sim refuses to launch.
#
# Steps: build (debug, sim) → code-sign app + NSE with your Dev identity +
# get-task-allow + the App Group → boot an iOS 26 sim → install → launch (the
# debug build seeds the fixture push key into the keychain + requests provisional
# auth) → push the fixture payload → open the Simulator to see the result.
#
# PASS  = a notification reading:  Baybo / The agent finished replying.
# FAIL  = the placeholder:         New message / Open Baybo
#         (NSE ran but couldn't read/decrypt — check the keychain self-check line)
#
# Usage:  app/mobile/apple/verify-nse.sh
#   env:  BAYBO_SIGN_ID   override the signing identity (default: first "Apple Development")
#         BAYBO_SIM_UDID  reuse an existing booted iOS 26 simulator

set -euo pipefail

IOS_DIR="$(cd "$(dirname "$0")/.." && pwd)"          # app/mobile
GEN="$IOS_DIR/src-tauri/gen/apple"
APP="$GEN/build/arm64-sim/Baybo.app"

# Pinned fixture (device_proto::fixtures): KEY 0x00..1f, NONCE 0xa0..ab.
KEY="000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
BID="sim-test"
ENC="d4kMNjmKp4+aLbJhjpvf1788vNs6SFqB4LC+kRAFpmIYoHDjwKCM4qOo1c94IRQ/I6otFitxpo9p/SuNSM/54cyGcKhziw=="
NONCE_B64="oKGio6Slpqeoqaqr"

echo "▸ building (debug, aarch64-sim)…"
( cd "$IOS_DIR" && rm -rf "$GEN/build/arm64-sim" "$GEN/build/baybo-mobile-app_iOS.xcarchive" \
    && cargo tauri ios build --debug --target aarch64-sim --no-sign )

IDENTITY="${BAYBO_SIGN_ID:-$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development/{print $2; exit}')}"
[ -n "$IDENTITY" ] || { echo "✗ no 'Apple Development' code-signing identity found"; exit 1; }
echo "▸ signing with: $IDENTITY"
PROJECT_TEAM_ID="$(awk -F': *' '/DEVELOPMENT_TEAM:/ {print $2; exit}' "$GEN/project.yml")"
case "$PROJECT_TEAM_ID" in
  *'$('*|*'$'*)
    PROJECT_TEAM_ID=""
    ;;
esac
TEAM_ID="${BAYBO_TEAM_ID:-$PROJECT_TEAM_ID}"
[ -n "$TEAM_ID" ] || { echo "✗ could not read DEVELOPMENT_TEAM from project.yml; set BAYBO_TEAM_ID"; exit 1; }
KEYCHAIN_GROUP="${BAYBO_KEYCHAIN_GROUP:-$TEAM_ID.com.baybo.app}"
echo "▸ keychain access group: $KEYCHAIN_GROUP"

# Sim-launch entitlements: get-task-allow (REQUIRED — the simulator only launches
# apps that carry it; the --no-sign linker signature includes it, so re-signing
# without it gets the launch denied by SBMainWorkspace) + the App Group / keychain
# sharing the NSE needs. aps-environment is intentionally omitted: it is a
# restricted push entitlement that needs a provisioning profile, and a local
# simctl push targets the bundle id directly without it.
ENT="$(mktemp -t baybo-sim-ent).plist"
cat > "$ENT" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.get-task-allow</key><true/>
  <key>com.apple.security.application-groups</key><array><string>group.com.baybo.app</string></array>
  <key>keychain-access-groups</key><array><string>$KEYCHAIN_GROUP</string></array>
</dict></plist>
PLIST

# Same entitlements for the embedded NSE (it is launched as its own process when
# a push arrives, so it needs get-task-allow + the shared App Group too).
codesign -f -s "$IDENTITY" --entitlements "$ENT" "$APP/PlugIns/NotificationExtension.appex"
codesign -f -s "$IDENTITY" --entitlements "$ENT" "$APP"
echo "▸ signed app + NSE"

UDID="${BAYBO_SIM_UDID:-$(xcrun simctl create baybo-verify \
  com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro com.apple.CoreSimulator.SimRuntime.iOS-26-5)}"
xcrun simctl boot "$UDID" 2>/dev/null || true
xcrun simctl bootstatus "$UDID" -b >/dev/null

xcrun simctl uninstall "$UDID" com.baybo.app 2>/dev/null || true
xcrun simctl install "$UDID" "$APP"
echo "▸ installed; launching with the fixture seed…"
if ! SIMCTL_CHILD_BAYBO_SEED_PUSH_KEY="$BID:$KEY" xcrun simctl launch "$UDID" com.baybo.app >/dev/null 2>&1; then
  cat <<'GUIDE'

✗ The app would not launch — the simulator rejects the App Group entitlement
  (com.apple.security.application-groups: group.com.baybo.app) because it is not
  PROVISIONED for your team. Manual codesign (this script) cannot register an
  App Group; only Xcode automatic signing can, and App Groups are a paid Apple
  Developer capability. The reliable path:

    1. open src-tauri/gen/apple/baybo-mobile-app.xcodeproj in Xcode
    2. for BOTH targets (Baybo + NotificationExtension), Signing & Capabilities:
       pick your Team (Xcode registers group.com.baybo.app + provisions; you may
       need a unique bundle id if com.baybo.app is taken — keep the App Group id
       in sync across both targets and PushKeyStore.swift / keychain.rs)
    3. Run the app once (seeds the key), then: xcrun simctl push <udid> <id> the
       fixture payload this script wrote, and check Notification Center.

  See docs/todo/mobile/phase1.md § "What's verified, and the external boundary".
GUIDE
  exit 1
fi

DATA="$(xcrun simctl get_app_container "$UDID" com.baybo.app data)"
for _ in $(seq 1 10); do [ -f "$DATA/tmp/baybo-seed-result.txt" ] && break; sleep 1; done
echo "▸ keychain self-check: $(cat "$DATA/tmp/baybo-seed-result.txt" 2>/dev/null || echo '(missing)')"
echo "  (expected: store=ok readback=match)"

PAYLOAD="$(mktemp -t baybo-push).json"
cat > "$PAYLOAD" <<JSON
{"aps":{"alert":{"title":"New message","body":"Open Baybo"},"mutable-content":1},"enc":"$ENC","n":"$NONCE_B64","bid":"$BID"}
JSON
xcrun simctl terminate "$UDID" com.baybo.app 2>/dev/null || true
xcrun simctl push "$UDID" com.baybo.app "$PAYLOAD"
open -a Simulator
echo
echo "▸ Open Notification Center on the simulator (swipe down from the top, or the"
echo "  lock screen). PASS = 'Baybo / The agent finished replying.'"
