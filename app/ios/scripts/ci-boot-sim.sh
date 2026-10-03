#!/usr/bin/env bash
# CI only: pick the test simulator, start booting it in the BACKGROUND, and
# export its UDID as SIM_UDID for every later step of the job.
#
# Why background: a cold simulator boot plus first app install is ~2.5 min on a
# macos-26 runner, and xcodebuild only starts it when the first test step runs —
# so it used to open the unit-test step with nothing else happening. Started at
# the top of the job, it overlaps the Rust/web/Xcode build instead. Test steps
# then `xcrun simctl bootstatus "$SIM_UDID" -b`, which waits for this boot (or
# boots the device itself if this one died).
#
# Why a UDID: `name=…,OS=latest` makes every xcodebuild resolve the device
# again, and nothing guarantees it lands on the one already booting. "Newest
# installed iOS runtime" is the same choice OS=latest makes — the runner image
# rotates its runtime set monthly, so never pin a version here.
set -euo pipefail

DEVICE="${1:-iPhone 17 Pro}"

udid="$(xcrun simctl list devices available -j | python3 -c '
import json, re, sys
name = sys.argv[1]
best = None
for runtime, devices in json.load(sys.stdin)["devices"].items():
    m = re.search(r"SimRuntime\.iOS-(\d+)-(\d+)(?:-(\d+))?$", runtime)
    if not m:
        continue
    version = tuple(int(part or 0) for part in m.groups())
    for device in devices:
        if device["name"] == name and (best is None or version > best[0]):
            best = (version, device["udid"])
print(best[1] if best else "")
' "$DEVICE")"

if [ -z "$udid" ]; then
  echo "::error::no available '$DEVICE' simulator on this runner"
  xcrun simctl list devices available
  exit 1
fi

echo "SIM_UDID=$udid" >> "$GITHUB_ENV"
echo "booting $DEVICE ($udid) in the background"
nohup xcrun simctl boot "$udid" > "${RUNNER_TEMP:-/tmp}/sim-boot.log" 2>&1 &
