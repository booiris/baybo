#!/usr/bin/env node
// Build a development-signed IPA of the SwiftUI app and install + launch it on
// a USB-connected device via `xcrun devicectl`. Assumes scripts/build-app.sh
// --device --release inputs (web bundle + xcframework) are already in place, or
// run with --prepare to build them first.
//
//   node scripts/install.mjs [--prepare] [--debug] [--no-launch] [--device <udid>]
import { execFileSync, execSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const TEAM_ID = process.env.BAYBO_TEAM_ID || "KLK5BP5YS6";

const args = process.argv.slice(2);
const has = (f) => args.includes(f);
const opt = (f) => {
  const i = args.indexOf(f);
  return i >= 0 ? args[i + 1] : undefined;
};
const configuration = has("--debug") ? "Debug" : "Release";

// Non-GUI sessions (SSH/launchd) need the login keychain unlocked so codesign
// works headlessly.
try {
  const manager = execSync("launchctl managername", { encoding: "utf8" }).trim();
  if (manager !== "Aqua" && !process.env.BAYBO_SKIP_KEYCHAIN_PREP) {
    execSync("security unlock-keychain login.keychain-db", { stdio: "inherit" });
    execSync("security set-key-partition-list -S apple-tool:,apple: login.keychain-db", {
      stdio: "inherit",
    });
  }
} catch {
  /* best-effort */
}

const run = (cmd, cmdArgs, cwd = root) =>
  execFileSync(cmd, cmdArgs, { cwd, stdio: "inherit" });

if (has("--prepare")) {
  run("bash", ["scripts/build-app.sh", "--device", ...(has("--debug") ? [] : ["--release"]), "--skip-web"]);
  run("bash", ["-c", "cd web && pnpm install --silent && pnpm build"], root);
  run("bash", ["-c", "rm -rf App/Resources/transcript && mkdir -p App/Resources/transcript && cp -R web/dist/. App/Resources/transcript/"], root);
  run("xcodegen", ["generate"]);
}

const buildDir = join(root, "build");
mkdirSync(buildDir, { recursive: true });
const archivePath = join(buildDir, "Baybo.xcarchive");
const exportDir = join(buildDir, "export");

// Resolve the target device BEFORE the archive, not after the export. `devicectl`
// has no "the one that is plugged in" default — omit --device and it exits 64 on
// a usage dump — and a device that is merely PAIRED is not a device it can reach:
// a paired-but-disconnected one fails with `error 1011, unable to locate a
// device`, naming an ecid rather than anything you passed it. Both of those used
// to land four minutes in, after a successful archive and export.
const liveDevice = (d) => {
  const c = d?.connectionProperties ?? {};
  return c.pairingState === "paired" && c.transportType && c.tunnelState !== "unavailable";
};

const resolveDevice = () => {
  // An explicit id is taken at face value: the caller knows something the
  // listing does not, and devicectl's own error is the right one to see.
  const explicit = opt("--device");
  if (explicit) return explicit;

  const listing = join(buildDir, "devices.json");
  execFileSync("xcrun", ["devicectl", "list", "devices", "--json-output", listing], {
    cwd: root,
    stdio: "ignore",
  });
  const devices = JSON.parse(readFileSync(listing, "utf8"))?.result?.devices ?? [];
  const live = devices.filter(liveDevice);
  if (live.length === 1) {
    const only = live[0];
    console.log(
      `\n> target: ${only.deviceProperties?.name ?? "?"} (${only.connectionProperties?.transportType})`,
    );
    return only.identifier;
  }
  if (live.length > 1) {
    const shown = live
      .map((d) => `  ${d.identifier}  ${d.deviceProperties?.name ?? "?"}`)
      .join("\n");
    console.error(`several devices connected; pass --device <udid>:\n${shown}`);
    process.exit(1);
  }

  const paired = devices.filter((d) => d?.connectionProperties?.pairingState === "paired");
  console.error(
    paired.length === 0
      ? "no device is paired with this Mac. Connect the iPhone over USB, unlock it, and accept Trust This Computer."
      : "no device is CONNECTED (these are paired, but nothing is reachable right now):\n" +
        paired
          .map((d) => {
            const c = d.connectionProperties ?? {};
            return `  ${d.identifier}  ${d.deviceProperties?.name ?? "?"}  last seen ${c.lastConnectionDate ?? "never"}`;
          })
          .join("\n") +
        "\n\nOver USB: plug it in with a DATA cable and unlock it." +
        "\nOver Wi-Fi: it has to be enabled once over USB — Xcode > Window > Devices" +
        "\n  and Simulators > the device > 'Connect via network' — and both ends have to" +
        "\n  be on the same network with mDNS allowed." +
        "\n\nPass --device <udid> to skip this check and let devicectl speak for itself.",
  );
  process.exit(1);
};

const targetDevice = resolveDevice();

run("xcodebuild", [
  "-project", "Baybo.xcodeproj",
  "-scheme", "Baybo",
  "-configuration", configuration,
  "-sdk", "iphoneos",
  "-destination", "generic/platform=iOS",
  "-archivePath", archivePath,
  // The local bundle id (`com.baybo.app.dev`) is an App ID that has never
  // existed until someone runs this, and its entitlements need Push
  // Notifications + App Groups registered against it. Without this flag
  // xcodebuild may not talk to the portal, falls back to the team wildcard
  // profile, and dies with "doesn't include the App Groups capability" before
  // it compiles anything. release.mjs passes it for the same reason.
  "-allowProvisioningUpdates",
  "archive",
]);

const exportPlist = join(buildDir, "ExportOptions.plist");
run("bash", ["-c", `cat > '${exportPlist}' <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>method</key><string>debugging</string>
  <key>teamID</key><string>${TEAM_ID}</string>
</dict></plist>
EOF`]);

run("xcodebuild", [
  "-exportArchive",
  "-archivePath", archivePath,
  "-exportPath", exportDir,
  "-exportOptionsPlist", exportPlist,
  "-allowProvisioningUpdates",
]);

const ipa = readdirSync(exportDir)
  .filter((f) => f.endsWith(".ipa"))
  .map((f) => join(exportDir, f))
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!ipa || !existsSync(ipa)) {
  console.error("no .ipa produced under", exportDir);
  process.exit(1);
}

// Read the id off the archive rather than naming it here. This script only
// ever builds Debug/Release, which project.yml gives the `.dev` sibling id so a
// local install lands BESIDE the App Store app instead of replacing it — and a
// second spelling of that id here is exactly how the launch would end up
// pointed at the OTHER Baybo on the phone.
const archivedApp = join(archivePath, "Products", "Applications", "Baybo.app");
const bundleId = execFileSync(
  "/usr/libexec/PlistBuddy",
  ["-c", "Print :CFBundleIdentifier", join(archivedApp, "Info.plist")],
  { encoding: "utf8" },
).trim();
if (!bundleId) {
  console.error("could not read CFBundleIdentifier from", archivedApp);
  process.exit(1);
}

const deviceArgs = ["--device", targetDevice];
run("xcrun", ["devicectl", "device", "install", "app", ...deviceArgs, ipa]);
if (!has("--no-launch")) {
  run("xcrun", ["devicectl", "device", "process", "launch", ...deviceArgs, bundleId]);
}
console.log("installed", ipa, `(${bundleId})`);
