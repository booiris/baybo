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
# Why the build/sign steps look the way they do:
#   * Build: scripts/build-core.sh --sim-only (DEBUG profile — the seed
#     hook is compiled out otherwise) → web transcript bundle if missing →
#     xcodegen → xcodebuild with CODE_SIGNING_ALLOWED=NO and a deterministic
#     -derivedDataPath (build/DerivedData).
#   * BAYBO_IOS_KEYCHAIN_ACCESS_GROUP is exported before the Rust build: the
#     app-side keychain group is baked into the staticlib at compile time
#     (ffi/build.rs), so it must match the group this script signs with.
#   * The built Info.plists' BayboKeychainAccessGroup is patched to the real
#     group before re-signing: with signing disabled, $(AppIdentifierPrefix)
#     expands empty and the NSE would query the wrong access group.
#   * The push payload's `enc` is derived from CIPHERTEXT_HEX in
#     crates/device-proto/src/fixtures.rs at run time — a base64 ciphertext
#     pinned here instead goes stale silently (an earlier one predated the
#     Aura→Baybo rename and decrypts to title "Aura"); deriving from the Rust
#     source keeps the two in lockstep.
#
# Steps: build (debug, sim) → code-sign app + NSE with your Dev identity +
# get-task-allow + the App Group → boot an iOS 26 sim → install → launch (the
# debug build seeds the fixture push key into the keychain via
# BAYBO_SEED_PUSH_KEY) → push the fixture payload → open the Simulator to see
# the result.
#
# PASS  = a notification reading:  Baybo / The agent finished replying.
# FAIL  = the placeholder:         New message / Open Baybo
#         (NSE ran but couldn't read/decrypt — check the keychain self-check line)
#
# Usage:  app/ios/scripts/verify-nse.sh
#   env:  BAYBO_SIGN_ID         override the signing identity (default: first "Apple Development")
#         BAYBO_TEAM_ID         override the team (default: DEVELOPMENT_TEAM in project.yml — KLK5BP5YS6)
#         BAYBO_KEYCHAIN_GROUP  override the keychain access group (default: <team>.com.baybo.app)
#         BAYBO_SIM_UDID        reuse an existing booted iOS 26 simulator

set -euo pipefail

IOS_DIR="$(cd "$(dirname "$0")/.." && pwd)"          # app/ios
REPO_ROOT="$(cd "$IOS_DIR/../.." && pwd)"
FIXTURES_RS="$REPO_ROOT/crates/device-proto/src/fixtures.rs"
DERIVED="$IOS_DIR/build/DerivedData"
APP="$DERIVED/Build/Products/Debug-iphonesimulator/Baybo.app"
APPEX="PlugIns/NotificationExtension.appex"

# Pinned fixture (device_proto::fixtures): KEY 0x00..=0x1f, NONCE 0xa0..=0xab.
KEY="000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
BID="sim-test"
NONCE_B64="oKGio6Slpqeoqaqr"

[ -f "$FIXTURES_RS" ] || { echo "✗ fixture source not found: $FIXTURES_RS"; exit 1; }
CT_HEX="$(sed -n 's/^pub const CIPHERTEXT_HEX: &str = "\([0-9a-f]*\)";$/\1/p' "$FIXTURES_RS")"
case "$CT_HEX" in
  *[!0-9a-f]*|"") echo "✗ could not extract CIPHERTEXT_HEX from $FIXTURES_RS"; exit 1 ;;
esac
[ "${#CT_HEX}" -ge 34 ] && [ $(( ${#CT_HEX} % 2 )) -eq 0 ] \
  || { echo "✗ CIPHERTEXT_HEX looks malformed (len ${#CT_HEX})"; exit 1; }
ENC="$(printf '%s' "$CT_HEX" | xxd -r -p | base64)"
[ -n "$ENC" ] || { echo "✗ base64 encoding of the fixture ciphertext failed"; exit 1; }
echo "▸ fixture ciphertext: ${#CT_HEX} hex chars → enc=$ENC"

IDENTITY="${BAYBO_SIGN_ID:-$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development/{print $2; exit}')}"
[ -n "$IDENTITY" ] || { echo "✗ no 'Apple Development' code-signing identity found"; exit 1; }
echo "▸ signing with: $IDENTITY"
PROJECT_TEAM_ID="$(awk -F': *' '/DEVELOPMENT_TEAM:/ {print $2; exit}' "$IOS_DIR/project.yml")"
case "$PROJECT_TEAM_ID" in
  *'$('*|*'$'*)
    PROJECT_TEAM_ID=""
    ;;
esac
TEAM_ID="${BAYBO_TEAM_ID:-$PROJECT_TEAM_ID}"
[ -n "$TEAM_ID" ] || { echo "✗ could not read DEVELOPMENT_TEAM from project.yml; set BAYBO_TEAM_ID"; exit 1; }
KEYCHAIN_GROUP="${BAYBO_KEYCHAIN_GROUP:-$TEAM_ID.com.baybo.app}"
echo "▸ keychain access group: $KEYCHAIN_GROUP"

echo "▸ building (debug, iphonesimulator)…"
if [ ! -f "$IOS_DIR/App/Resources/transcript/index.html" ]; then
  echo "▸ transcript web bundle missing — building it…"
  ( cd "$IOS_DIR/web" && pnpm install --silent && pnpm build )
  rm -rf "$IOS_DIR/App/Resources/transcript"
  mkdir -p "$IOS_DIR/App/Resources/transcript"
  cp -R "$IOS_DIR/web/dist/." "$IOS_DIR/App/Resources/transcript/"
fi
# DEBUG profile on purpose: debug_seed_push_key is #[cfg(debug_assertions)].
# The exported group is baked into the staticlib (ffi/build.rs) and must match
# the entitlement this script signs below.
XCLOG="$(mktemp -t baybo-xcodebuild).log"
( cd "$IOS_DIR" \
    && BAYBO_IOS_KEYCHAIN_ACCESS_GROUP="$KEYCHAIN_GROUP" scripts/build-core.sh --sim-only \
    && xcodegen generate \
    && xcodebuild -project Baybo.xcodeproj -scheme Baybo -configuration Debug \
         -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' \
         -derivedDataPath "$DERIVED" build CODE_SIGNING_ALLOWED=NO >"$XCLOG" 2>&1 ) \
  || { grep -E 'error|warning: |BUILD' "$XCLOG" || tail -40 "$XCLOG"; echo "✗ build failed (full log: $XCLOG)"; exit 1; }
grep -E 'error|warning: |BUILD' "$XCLOG" || true
[ -d "$APP" ] || { echo "✗ built app not found at $APP"; exit 1; }
[ -d "$APP/$APPEX" ] || { echo "✗ NSE appex not embedded at $APP/$APPEX"; exit 1; }

# READ the id off the app rather than naming it: a Debug build carries the
# `.dev` sibling id (project.yml's BAYBO_BUNDLE_ID), and a hardcoded
# com.baybo.app here would drive simctl at whatever OTHER Baybo is installed.
BUNDLE_ID="$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$APP/Info.plist")"
[ -n "$BUNDLE_ID" ] || { echo "✗ could not read CFBundleIdentifier from $APP/Info.plist"; exit 1; }
echo "▸ bundle id: $BUNDLE_ID"

# With signing disabled, $(AppIdentifierPrefix) expands to nothing, so the
# BayboKeychainAccessGroup key would read as a bare ".<bundle id>". BOTH sides
# read it now — the NSE for its lookup, and the Rust core for the group it
# writes the push key to — so patch both plists to the real group.
for PLIST in "$APP/Info.plist" "$APP/$APPEX/Info.plist"; do
  /usr/libexec/PlistBuddy -c "Set :BayboKeychainAccessGroup $KEYCHAIN_GROUP" "$PLIST" 2>/dev/null \
    || /usr/libexec/PlistBuddy -c "Add :BayboKeychainAccessGroup string $KEYCHAIN_GROUP" "$PLIST"
done

# Sim-launch entitlements: get-task-allow (REQUIRED — the simulator only launches
# apps that carry it) + the App Group / keychain sharing the NSE needs.
# aps-environment is intentionally omitted: it is a restricted push entitlement
# that needs a provisioning profile, and a local simctl push targets the bundle
# id directly without it.
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

# Nested code first (CODE_SIGNING_ALLOWED=NO leaves everything unsigned): any
# embedded dylib/framework, then the NSE (launched as its own process when a
# push arrives, so it needs get-task-allow + the shared App Group too), then
# the app.
if [ -d "$APP/Frameworks" ]; then
  find "$APP/Frameworks" -maxdepth 1 \( -name '*.dylib' -o -name '*.framework' \) -print0 \
    | while IFS= read -r -d '' NESTED; do codesign -f -s "$IDENTITY" "$NESTED"; done
fi
codesign -f -s "$IDENTITY" --entitlements "$ENT" "$APP/$APPEX"
codesign -f -s "$IDENTITY" --entitlements "$ENT" "$APP"
echo "▸ signed app + NSE"

UDID="${BAYBO_SIM_UDID:-$(xcrun simctl create baybo-verify \
  com.apple.CoreSimulator.SimDeviceType.iPhone-16-Pro com.apple.CoreSimulator.SimRuntime.iOS-26-5)}"
xcrun simctl boot "$UDID" 2>/dev/null || true
xcrun simctl bootstatus "$UDID" -b >/dev/null

xcrun simctl uninstall "$UDID" "$BUNDLE_ID" 2>/dev/null || true
xcrun simctl install "$UDID" "$APP"
echo "▸ installed; launching with the fixture seed…"
if ! SIMCTL_CHILD_BAYBO_SEED_PUSH_KEY="$BID:$KEY" xcrun simctl launch "$UDID" "$BUNDLE_ID" >/dev/null 2>&1; then
  cat <<'GUIDE'

✗ The app would not launch — the simulator rejects the App Group entitlement
  (com.apple.security.application-groups: group.com.baybo.app) because it is not
  PROVISIONED for your team. Manual codesign (this script) cannot register an
  App Group; only Xcode automatic signing can, and App Groups are a paid Apple
  Developer capability. The reliable path:

    1. run scripts/build-app.sh once (regenerates Baybo.xcodeproj via xcodegen),
       then open app/ios/Baybo.xcodeproj in Xcode
    2. for BOTH targets (Baybo + NotificationExtension), Signing & Capabilities:
       pick your Team (Xcode registers group.com.baybo.app + provisions; you may
       need a unique bundle id if com.baybo.app is taken — keep the App Group id
       in sync across both targets and PushKeyStore.swift / ffi/src/keychain.rs)
    3. Run the app once with BAYBO_SEED_PUSH_KEY in the scheme environment
       (seeds the key), then: xcrun simctl push <udid> <bundle-id> the fixture
       payload this script wrote, and check Notification Center.
GUIDE
  exit 1
fi

DATA="$(xcrun simctl get_app_container "$UDID" "$BUNDLE_ID" data)"
for _ in $(seq 1 10); do [ -f "$DATA/tmp/baybo-seed-result.txt" ] && break; sleep 1; done
echo "▸ keychain self-check: $(cat "$DATA/tmp/baybo-seed-result.txt" 2>/dev/null || echo '(missing)')"
echo "  (expected: store=ok readback=match)"

PAYLOAD="$(mktemp -t baybo-push).json"
cat > "$PAYLOAD" <<JSON
{"aps":{"alert":{"title":"New message","body":"Open Baybo"},"mutable-content":1},"enc":"$ENC","n":"$NONCE_B64","bid":"$BID"}
JSON
xcrun simctl terminate "$UDID" "$BUNDLE_ID" 2>/dev/null || true
xcrun simctl push "$UDID" "$BUNDLE_ID" "$PAYLOAD"
open -a Simulator
echo
echo "▸ Open Notification Center on the simulator (swipe down from the top, or the"
echo "  lock screen). PASS = 'Baybo / The agent finished replying.'"
