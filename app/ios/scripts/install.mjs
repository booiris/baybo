#!/usr/bin/env node
// Build a development-signed IPA of the SwiftUI app and install + launch it on
// a USB-connected device via `xcrun devicectl` — the successor of app/mobile's
// ios-install.mjs, minus Tauri. Assumes scripts/build-app.sh --device --release
// inputs (web bundle + xcframework) are already in place, or run with
// --prepare to build them first.
//
//   node scripts/install.mjs [--prepare] [--debug] [--no-launch] [--device <udid>]
import { execFileSync, execSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const BUNDLE_ID = "com.baybo.app";
const TEAM_ID = process.env.BAYBO_TEAM_ID || "KLK5BP5YS6";

const args = process.argv.slice(2);
const has = (f) => args.includes(f);
const opt = (f) => {
  const i = args.indexOf(f);
  return i >= 0 ? args[i + 1] : undefined;
};
const configuration = has("--debug") ? "Debug" : "Release";

// Non-GUI sessions (SSH/launchd) need the login keychain unlocked so codesign
// works headlessly — same prep app/mobile's tauri-env.mjs did.
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

run("xcodebuild", [
  "-project", "Baybo.xcodeproj",
  "-scheme", "Baybo",
  "-configuration", configuration,
  "-sdk", "iphoneos",
  "-destination", "generic/platform=iOS",
  "-archivePath", archivePath,
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
]);

const ipa = readdirSync(exportDir)
  .filter((f) => f.endsWith(".ipa"))
  .map((f) => join(exportDir, f))
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!ipa || !existsSync(ipa)) {
  console.error("no .ipa produced under", exportDir);
  process.exit(1);
}

const deviceArgs = opt("--device") ? ["--device", opt("--device")] : [];
run("xcrun", ["devicectl", "device", "install", "app", ...deviceArgs, ipa]);
if (!has("--no-launch")) {
  run("xcrun", ["devicectl", "device", "process", "launch", ...deviceArgs, BUNDLE_ID]);
}
console.log("installed", ipa);
