#!/usr/bin/env bash
#
# verify-nse.sh — end-to-end proof that the iOS NSE decrypts a real push on the
# Simulator, using the pinned cross-language fixture from `device_proto::fixtures`.
#
# WHY a script you run (not CI): the App Group keychain the NSE reads is gated by
# the `com.apple.security.application-groups` entitlement, which is only honored
# for a *code-signed* build. Signing needs your Apple Development identity, whose
# private key requires interactive keychain authorization (Touch ID / password) —
# so this can't run headlessly. Everything else (build, objc2 provisional-auth
# registration, the push pipeline) is already verified; this closes the loop.
#
# Steps: build (debug, sim) → code-sign app + NSE with your Dev identity and the
# App Group entitlement → boot an iOS 26 sim → install → launch (the debug build
# seeds the fixture push key into the keychain and requests provisional auth) →
# push the fixture payload → open the Simulator so you can see the result.
#
# PASS  = a notification reading:  Aura / The agent finished replying.
# FAIL  = the placeholder:         New message / Open Aura
#         (NSE ran but couldn't read/decrypt — check the keychain self-check line)
#
# Usage:  app/mobile/ios/apple/verify-nse.sh
#   env:  AURA_SIGN_ID   override the signing identity (default: first "Apple Development")
#         AURA_SIM_UDID  reuse an existing booted iOS 26 simulator

set -euo pipefail

IOS_DIR="$(cd "$(dirname "$0")/.." && pwd)"          # app/mobile/ios
GEN="$IOS_DIR/src-tauri/gen/apple"
APP="$GEN/build/arm64-sim/Aura.app"

# Pinned fixture (device_proto::fixtures): KEY 0x00..1f, NONCE 0xa0..ab.
KEY="000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
BID="sim-test"
ENC="d4kMNjmKp4+aLbJhjpvf1788vNs6SFqB4LC+kRAFpmIYoHDjwKCM4qOo1c94IRQ/I6otFitxpo9p/SuNSM/54cyGcKhziw=="
NONCE_B64="oKGio6Slpqeoqaqr"

echo "▸ building (debug, aarch64-sim)…"
( cd "$IOS_DIR" && rm -rf "$GEN/build/arm64-sim" "$GEN/build/aura-mobile-app_iOS.xcarchive" \
    && cargo tauri ios build --debug --target aarch64-sim --no-sign )

IDENTITY="${AURA_SIGN_ID:-$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development/{print $2; exit}')}"
[ -n "$IDENTITY" ] || { echo "✗ no 'Apple Development' code-signing identity found"; exit 1; }
echo "▸ signing with: $IDENTITY"

# App Group + keychain entitlements ONLY. (aps-environment is a restricted push
# entitlement that AMFI rejects without a matching profile; it is not needed for
# a local simctl push, which targets the bundle id directly.)
ENT="$(mktemp -t aura-sim-ent).plist"
cat > "$ENT" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.application-groups</key><array><string>group.com.aura.app</string></array>
  <key>keychain-access-groups</key><array><string>group.com.aura.app</string></array>
</dict></plist>
PLIST

codesign -f -s "$IDENTITY" --entitlements "$GEN/NotificationExtension/NotificationExtension.entitlements" \
  "$APP/PlugIns/NotificationExtension.appex"
codesign -f -s "$IDENTITY" --entitlements "$ENT" "$APP"
echo "▸ signed app + NSE"

UDID="${AURA_SIM_UDID:-$(xcrun simctl create aura-verify \
  com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro com.apple.CoreSimulator.SimRuntime.iOS-26-5)}"
xcrun simctl boot "$UDID" 2>/dev/null || true
xcrun simctl bootstatus "$UDID" -b >/dev/null

xcrun simctl uninstall "$UDID" com.aura.app 2>/dev/null || true
xcrun simctl install "$UDID" "$APP"
echo "▸ installed; launching with the fixture seed…"
SIMCTL_CHILD_AURA_SEED_PUSH_KEY="$BID:$KEY" xcrun simctl launch "$UDID" com.aura.app >/dev/null

DATA="$(xcrun simctl get_app_container "$UDID" com.aura.app data)"
for _ in $(seq 1 10); do [ -f "$DATA/tmp/aura-seed-result.txt" ] && break; sleep 1; done
echo "▸ keychain self-check: $(cat "$DATA/tmp/aura-seed-result.txt" 2>/dev/null || echo '(missing)')"
echo "  (expected: store=ok readback=match)"

PAYLOAD="$(mktemp -t aura-push).json"
cat > "$PAYLOAD" <<JSON
{"aps":{"alert":{"title":"New message","body":"Open Aura"},"mutable-content":1},"enc":"$ENC","n":"$NONCE_B64","bid":"$BID","kid":0}
JSON
xcrun simctl terminate "$UDID" com.aura.app 2>/dev/null || true
xcrun simctl push "$UDID" com.aura.app "$PAYLOAD"
open -a Simulator
echo
echo "▸ Open Notification Center on the simulator (swipe down from the top, or the"
echo "  lock screen). PASS = 'Aura / The agent finished replying.'"
