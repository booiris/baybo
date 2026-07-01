// ios-install.mjs — build a STANDALONE, code-signed Baybo and install it on a
// physically-connected iPhone, so the app keeps working with no Mac attached.
//
// WHY this exists: `tauri ios dev` installs a *development* app whose frontend is
// served live from the Vite dev server (`build.devUrl`, http://localhost:1420).
// The moment that dev session ends — or you delete the dev app — the phone has
// nothing to load and the app is dead. `tauri ios build` instead bundles
// `build.frontendDist` (../dist) INTO the app, producing a self-contained binary
// that runs offline. This script runs that build (via `scripts/tauri-env.mjs`),
// then installs the result over USB.
//
// Signing is Apple-"debugging" (development-signed) against the team hardcoded in
// `src-tauri/gen/apple/project.yml` (`DEVELOPMENT_TEAM`); the connected device must
// already be registered with that team (running `tauri ios dev`/Xcode against it
// once registers it). App Group + Push capabilities need a paid Apple Developer
// team — see app/mobile/apple/README.md and docs/modules/mobile/companion.md.
//
// Usage (from app/mobile):
//   pnpm ios:install                 build (release) + install + launch
//   pnpm ios:install --debug         faster debug build instead of release
//   pnpm ios:install --skip-build    install the last-built IPA as-is
//   pnpm ios:install --no-launch     install only, don't auto-launch
//   pnpm ios:install --device <id>   target a specific device (CoreDevice id/UDID/name)
// Any other flag is forwarded to `tauri ios build` (e.g. `-t aarch64`, `-v`).
//   env: BAYBO_IOS_DEVICE  same as --device.

import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const BUNDLE_ID = "com.baybo.app";
const EXPORT_METHOD = "debugging";
const SCRIPTS_DIR = dirname(fileURLToPath(import.meta.url));
const MOBILE_DIR = resolve(SCRIPTS_DIR, "..");
const APPLE_DIR = join(MOBILE_DIR, "src-tauri", "gen", "apple");
const BUILD_DIR = join(APPLE_DIR, "build");

// Fallback re-export: if `tauri ios build`'s own export step fails but the archive
// was produced (it's always fully signed, both targets, real team), we re-export it
// here with an explicit teamID read back from the archive. Kept as a safety net —
// the team is now hardcoded on both targets, so the historical
// `No Account for Team "$(…)"` export failure shouldn't occur. See
// docs/modules/mobile/companion.md.
const EXPORT_OPTIONS_PLIST = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>method</key>
    <string>{{METHOD}}</string>
    <key>signingStyle</key>
    <string>automatic</string>
    <key>teamID</key>
    <string>{{TEAM}}</string>
    <key>stripSwiftSymbols</key>
    <true/>
</dict>
</plist>
`;
const LOCAL_BIN = join(MOBILE_DIR, "node_modules", ".bin");

const { skipBuild, noLaunch, device, buildArgs } = parseArgs(process.argv.slice(2));

if (!skipBuild) {
  build(buildArgs);
}

const ipa = newestFileWithExt(BUILD_DIR, ".ipa");
if (!ipa) {
  fail(
    `no .ipa found under ${BUILD_DIR}. Run a build first (drop --skip-build), or check the build output above.`,
  );
}

const { app, tmp } = unpackApp(ipa);
const target = resolveDevice(device);
console.error(`▸ installing ${appLabel(ipa)} → ${target.name} (${target.identifier})`);
install(target.identifier, app);
rmSync(tmp, { recursive: true, force: true });

if (noLaunch) {
  console.error(`▸ installed. Open Baybo on ${target.name} to launch it.`);
} else {
  launch(target.identifier, target.name);
}

function parseArgs(argv) {
  let skipBuild = false;
  let noLaunch = false;
  let device = process.env.BAYBO_IOS_DEVICE?.trim() || undefined;
  const buildArgs = [];

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === "--skip-build") {
      skipBuild = true;
    } else if (arg === "--no-launch") {
      noLaunch = true;
    } else if (arg === "--device") {
      device = argv[(i += 1)];
    } else if (arg.startsWith("--device=")) {
      device = arg.slice("--device=".length);
    } else {
      buildArgs.push(arg);
    }
  }
  return { skipBuild, noLaunch, device, buildArgs };
}

function build(extraArgs) {
  // Route through tauri-env.mjs, which sets APPLE_DEVELOPMENT_TEAM for Tauri's
  // export step and — in a non-GUI (SSH) session — unlocks the keychain so
  // codesign can reach the signing key. --export-method debugging => a
  // development-signed IPA installable on the team's registered devices (not App
  // Store / ad-hoc distribution).
  const startedAt = Date.now();
  const args = [
    join(SCRIPTS_DIR, "tauri-env.mjs"),
    "ios",
    "build",
    "--export-method",
    EXPORT_METHOD,
    ...extraArgs,
  ];
  const env = { ...process.env, PATH: `${LOCAL_BIN}:${process.env.PATH ?? ""}` };
  const result = spawnSync(process.execPath, args, { stdio: "inherit", cwd: MOBILE_DIR, env });

  if (result.status === 0) {
    return;
  }
  // Tauri archives before it exports, so a fresh archive means only the (broken)
  // export step failed — re-export it ourselves. If no fresh archive exists, the
  // archive/compile itself failed, which is a real error.
  const archive = newestArchive(BUILD_DIR);
  if (!archive || archive.mtime < startedAt) {
    fail(`tauri ios build failed (exit ${result.status ?? result.signal}).`);
  }
  console.error("▸ tauri export failed; re-exporting the archive with a resolved teamID…");
  exportArchive(archive.path);
}

function exportArchive(archivePath) {
  const team = teamFromArchive(archivePath);
  if (!team) {
    fail(`could not read the signing team from ${archivePath}/Info.plist.`);
  }
  const optionsPath = join(BUILD_DIR, "ExportOptions.plist");
  writeFileSync(
    optionsPath,
    EXPORT_OPTIONS_PLIST.replace("{{METHOD}}", EXPORT_METHOD).replace("{{TEAM}}", team),
  );
  const result = spawnSync(
    "xcrun",
    [
      "xcodebuild",
      "-exportArchive",
      "-archivePath",
      archivePath,
      "-exportOptionsPlist",
      optionsPath,
      "-exportPath",
      BUILD_DIR,
      "-allowProvisioningUpdates",
    ],
    { stdio: "inherit" },
  );
  if (result.status !== 0) {
    fail("xcodebuild -exportArchive failed (see output above).");
  }
}

function teamFromArchive(archivePath) {
  const result = spawnSync(
    "/usr/libexec/PlistBuddy",
    ["-c", "Print :ApplicationProperties:Team", join(archivePath, "Info.plist")],
    { encoding: "utf8" },
  );
  return result.status === 0 ? result.stdout.trim() : undefined;
}

function newestArchive(dir) {
  if (!existsSync(dir)) {
    return undefined;
  }
  let best;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.isDirectory() && entry.name.endsWith(".xcarchive")) {
      const full = join(dir, entry.name);
      const mtime = statSync(full).mtimeMs;
      if (!best || mtime > best.mtime) {
        best = { path: full, mtime };
      }
    }
  }
  return best;
}

function unpackApp(ipa) {
  const dest = mkdtempSync(join(tmpdir(), "baybo-ipa-"));
  const result = spawnSync("unzip", ["-q", "-o", ipa, "-d", dest], { stdio: "inherit" });
  if (result.status !== 0) {
    rmSync(dest, { recursive: true, force: true });
    fail(`could not unzip ${ipa}.`);
  }
  const payload = join(dest, "Payload");
  const app = existsSync(payload)
    ? readdirSync(payload)
        .filter((entry) => entry.endsWith(".app"))
        .map((entry) => join(payload, entry))[0]
    : undefined;
  if (!app) {
    rmSync(dest, { recursive: true, force: true });
    fail(`no Payload/*.app inside ${ipa}.`);
  }
  return { app, tmp: dest };
}

function resolveDevice(requested) {
  const out = mkdtempSync(join(tmpdir(), "baybo-dev-"));
  const jsonPath = join(out, "devices.json");
  const result = spawnSync("xcrun", ["devicectl", "list", "devices", "--json-output", jsonPath], {
    encoding: "utf8",
  });
  if (result.status !== 0) {
    fail(`xcrun devicectl list devices failed:\n${result.stderr ?? ""}`);
  }
  const devices = JSON.parse(readFileSync(jsonPath, "utf8")).result?.devices ?? [];
  rmSync(out, { recursive: true, force: true });
  const view = devices.map((d) => ({
    name: d.deviceProperties?.name ?? "(unknown)",
    identifier: d.identifier,
    udid: d.hardwareProperties?.udid,
    platform: d.hardwareProperties?.platform,
    connected: d.connectionProperties?.tunnelState === "connected",
  }));

  if (requested) {
    const match = view.find((d) =>
      [d.identifier, d.udid, d.name].some((id) => id && id === requested),
    );
    if (!match) {
      fail(`no device matches '${requested}'. Connected:\n${describe(view)}`);
    }
    return match;
  }

  const connected = view.filter((d) => d.platform === "iOS" && d.connected);
  if (connected.length === 0) {
    fail(
      "no connected iOS device found. Plug in an unlocked iPhone (trust this Mac), then retry.\n" +
        `Seen:\n${describe(view)}`,
    );
  }
  if (connected.length > 1) {
    fail(`multiple connected devices — pass --device <id>:\n${describe(connected)}`);
  }
  return connected[0];
}

function install(identifier, app) {
  const result = spawnSync(
    "xcrun",
    ["devicectl", "device", "install", "app", "--device", identifier, app],
    { stdio: "inherit" },
  );
  if (result.status !== 0) {
    fail(
      "install failed. Common causes: the device isn't registered with the signing team " +
        "(run `pnpm tauri ios dev` against it once, or add it in Xcode), the phone is locked, " +
        "or Developer Mode is off (Settings ▸ Privacy & Security ▸ Developer Mode).",
    );
  }
}

function launch(identifier, name) {
  const result = spawnSync(
    "xcrun",
    ["devicectl", "device", "process", "launch", "--device", identifier, BUNDLE_ID],
    { stdio: "inherit" },
  );
  if (result.status !== 0) {
    // A locked phone refuses remote launch — the install still succeeded.
    console.error(`▸ installed, but couldn't auto-launch. Unlock ${name} and tap Baybo.`);
  }
}

function newestFileWithExt(dir, ext) {
  if (!existsSync(dir)) {
    return undefined;
  }
  let best;
  const walk = (current) => {
    for (const entry of readdirSync(current, { withFileTypes: true })) {
      const full = join(current, entry.name);
      if (entry.isDirectory()) {
        walk(full);
      } else if (entry.name.endsWith(ext)) {
        const mtime = statSync(full).mtimeMs;
        if (!best || mtime > best.mtime) {
          best = { path: full, mtime };
        }
      }
    }
  };
  walk(dir);
  return best?.path;
}

function appLabel(ipa) {
  return ipa.replace(`${MOBILE_DIR}/`, "");
}

function describe(view) {
  return view
    .map((d) => `  - ${d.name} [${d.platform ?? "?"}] ${d.identifier}${d.connected ? " (connected)" : ""}`)
    .join("\n");
}

function fail(message) {
  console.error(`baybo-mobile: ${message}`);
  process.exit(1);
}
