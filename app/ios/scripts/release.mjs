#!/usr/bin/env node
import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  rmSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const PROJECT = join(ROOT, "Baybo.xcodeproj");
const SCHEME = "Baybo";
const CONFIGURATION = "Distribution";
const APP_NAME = "Baybo.app";
const EXTENSION_NAME = "NotificationExtension.appex";
const EXPECTED_APNS_ENVIRONMENT = "production";
// The SHIPPED identity, spelled out here so a release can never inherit it from
// whatever project.yml happens to say. Local builds carry a `.dev` sibling id
// (project.yml's BAYBO_BUNDLE_ID) precisely so a device install lands beside the
// App Store app rather than replacing it, and the whole point of that split is
// that it must never travel the other way: an App Store upload under
// com.baybo.app.dev, or one entitled to a second keychain group, would strand
// every existing install's device identity.
const EXPECTED_BUNDLE_ID = "com.baybo.app";
const EXPECTED_EXTENSION_BUNDLE_ID = `${EXPECTED_BUNDLE_ID}.NotificationExtension`;
const EXPECTED_DISPLAY_NAME = "Baybo";
const EXPECTED_KEYCHAIN_GROUP_SUFFIX = `.${EXPECTED_BUNDLE_ID}`;
// The Rust core reads its shared access group off this Info key at runtime, so
// the shipped value IS the group every already-installed device's push key sits
// in. A build that shipped the local group here would strand every one of them.
const KEYCHAIN_GROUP_INFO_KEY = "BayboKeychainAccessGroup";
const DEBUG_SEED_SYMBOL = "debug_seed_push_key";
const EXPORT_OPTIONS = join(ROOT, "scripts", "ExportOptions-AppStore.plist");
const VERIFY_OPTIONS_NAME = "ExportOptions-Verify.plist";
const LOCAL_EXPORT_DESTINATION = "export";
const DISTRIBUTION_AUTHORITY = "Apple Distribution:";
const PROVISIONED_DEVICES_KEY = "ProvisionedDevices";
const EMBEDDED_PROFILE_NAME = "embedded.mobileprovision";
const USAGE = `Usage:
  node scripts/release.mjs --version <x.y.z> [--build-number <integer>] [--upload]

The default action archives, exports and verifies an App Store build. --upload
then delivers it to App Store Connect. --build-number defaults to a local
YYYYMMDDHHMM stamp, which is the convention every uploaded build follows.`;

const fail = (message) => {
  throw new Error(message);
};

const optionValue = (args, index, option) => {
  const value = args[index + 1];
  if (!value || value.startsWith("--")) {
    fail(`${option} requires a value`);
  }
  return value;
};

// App Store Connect rejects any build whose number is not higher than the last
// one uploaded, and nothing in-tree records what that was — the stamp keeps the
// sequence monotonic without a local ledger.
const defaultBuildNumber = () => {
  const now = new Date();
  const pad = (value) => String(value).padStart(2, "0");
  return [
    now.getFullYear(),
    pad(now.getMonth() + 1),
    pad(now.getDate()),
    pad(now.getHours()),
    pad(now.getMinutes()),
  ].join("");
};

const parseArgs = (args) => {
  let version;
  let buildNumber;
  let upload = false;

  for (let index = 0; index < args.length; index += 1) {
    switch (args[index]) {
      case "--version":
        version = optionValue(args, index, "--version");
        index += 1;
        break;
      case "--build-number":
        buildNumber = optionValue(args, index, "--build-number");
        index += 1;
        break;
      case "--upload":
        upload = true;
        break;
      case "--help":
      case "-h":
        console.log(USAGE);
        process.exit(0);
        break;
      default:
        fail(`unknown argument: ${args[index]}`);
    }
  }

  if (!version || !/^\d+\.\d+\.\d+$/.test(version)) {
    fail("--version must contain three dot-separated integers, for example 1.0.0");
  }
  if (buildNumber === undefined) {
    buildNumber = defaultBuildNumber();
  } else if (!/^[1-9]\d*$/.test(buildNumber)) {
    fail("--build-number must be a positive integer");
  }

  return { version, buildNumber, upload };
};

const run = (command, args, cwd = ROOT) => {
  console.log(`\n> ${command} ${args.join(" ")}`);
  execFileSync(command, args, { cwd, stdio: "inherit" });
};

const plistValue = (plist, key) =>
  execFileSync("/usr/libexec/PlistBuddy", ["-c", `Print :${key}`, plist], {
    encoding: "utf8",
  }).trim();

const plistHasKey = (plist, key) =>
  spawnSync("/usr/libexec/PlistBuddy", ["-c", `Print :${key}`, plist], {
    encoding: "utf8",
  }).status === 0;

const assertEqual = (label, actual, expected) => {
  if (actual !== expected) {
    fail(`${label}: expected ${expected}, got ${actual}`);
  }
};

const refreshTranscriptBundle = () => {
  const webRoot = join(ROOT, "web");
  const webBundle = join(webRoot, "dist");
  const appBundle = join(ROOT, "App", "Resources", "transcript");

  run("pnpm", ["install", "--silent", "--frozen-lockfile"], webRoot);
  run("pnpm", ["build"], webRoot);
  if (!existsSync(join(webBundle, "index.html"))) {
    fail(`web build did not produce ${join(webBundle, "index.html")}`);
  }

  rmSync(appBundle, { recursive: true, force: true });
  cpSync(webBundle, appBundle, { recursive: true });
};

const assertReleaseCore = (appBinary) => {
  const result = spawnSync("xcrun", ["nm", "-a", appBinary], {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.error) {
    fail(`could not inspect archive symbols: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`nm failed while inspecting the archived app (exit ${result.status})`);
  }
  if (result.stdout.includes(DEBUG_SEED_SYMBOL)) {
    fail(`archive contains debug-only symbol ${DEBUG_SEED_SYMBOL}`);
  }
};

const validateArchive = (archivePath, version, buildNumber) => {
  const app = join(archivePath, "Products", "Applications", APP_NAME);
  const appInfo = join(app, "Info.plist");
  const extensionInfo = join(app, "PlugIns", EXTENSION_NAME, "Info.plist");
  const appBinary = join(app, "Baybo");

  for (const path of [appInfo, extensionInfo, appBinary]) {
    if (!existsSync(path)) {
      fail(`archive is missing ${path}`);
    }
  }

  assertEqual(
    "BayboApnsEnvironment",
    plistValue(appInfo, "BayboApnsEnvironment"),
    EXPECTED_APNS_ENVIRONMENT,
  );
  assertEqual(
    "app bundle id",
    plistValue(appInfo, "CFBundleIdentifier"),
    EXPECTED_BUNDLE_ID,
  );
  assertEqual(
    "extension bundle id",
    plistValue(extensionInfo, "CFBundleIdentifier"),
    EXPECTED_EXTENSION_BUNDLE_ID,
  );
  assertEqual(
    "app display name",
    plistValue(appInfo, "CFBundleDisplayName"),
    EXPECTED_DISPLAY_NAME,
  );
  for (const [label, plist] of [["app", appInfo], ["extension", extensionInfo]]) {
    const group = plistValue(plist, KEYCHAIN_GROUP_INFO_KEY);
    if (!group.endsWith(EXPECTED_KEYCHAIN_GROUP_SUFFIX)) {
      fail(
        `${label} ${KEYCHAIN_GROUP_INFO_KEY}: ${group} does not end in ${EXPECTED_KEYCHAIN_GROUP_SUFFIX}`,
      );
    }
  }
  assertEqual(
    "app marketing version",
    plistValue(appInfo, "CFBundleShortVersionString"),
    version,
  );
  assertEqual(
    "app build number",
    plistValue(appInfo, "CFBundleVersion"),
    buildNumber,
  );
  assertEqual(
    "extension marketing version",
    plistValue(extensionInfo, "CFBundleShortVersionString"),
    version,
  );
  assertEqual(
    "extension build number",
    plistValue(extensionInfo, "CFBundleVersion"),
    buildNumber,
  );
  assertReleaseCore(appBinary);

  console.log("\nArchive validation passed:");
  console.log(`  bundle id: ${EXPECTED_BUNDLE_ID} ("${EXPECTED_DISPLAY_NAME}")`);
  console.log(`  keychain group Info key: ${EXPECTED_KEYCHAIN_GROUP_SUFFIX.slice(1)}`);
  console.log(`  runtime APNs environment: ${EXPECTED_APNS_ENVIRONMENT}`);
  console.log(`  app + extension: ${version} (${buildNumber})`);
  console.log(`  debug seed symbol: absent`);
};

// The archive is development-signed under automatic signing; distribution
// identity, profile and entitlements only exist after the export re-signs.
const localExportOptions = (outputRoot) => {
  const path = join(outputRoot, VERIFY_OPTIONS_NAME);
  rmSync(path, { force: true });
  cpSync(EXPORT_OPTIONS, path);
  execFileSync("/usr/libexec/PlistBuddy", [
    "-c",
    `Set :destination ${LOCAL_EXPORT_DESTINATION}`,
    path,
  ]);
  return path;
};

const exportArchive = (archivePath, exportPath, optionsPath) => {
  rmSync(exportPath, { recursive: true, force: true });
  run("xcodebuild", [
    "-exportArchive",
    "-archivePath", archivePath,
    "-exportPath", exportPath,
    "-exportOptionsPlist", optionsPath,
    "-allowProvisioningUpdates",
  ]);
};

const exportedIpa = (exportPath) => {
  const entries = readdirSync(exportPath).filter((name) => name.endsWith(".ipa"));
  if (entries.length !== 1) {
    fail(`expected exactly one .ipa in ${exportPath}, found ${entries.length}`);
  }
  return join(exportPath, entries[0]);
};

const signedEntitlements = (bundlePath) => {
  const xml = execFileSync(
    "codesign",
    ["-d", "--entitlements", "-", "--xml", bundlePath],
    { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] },
  );
  return JSON.parse(
    execFileSync("plutil", ["-convert", "json", "-o", "-", "-"], {
      input: xml,
      encoding: "utf8",
    }),
  );
};

const signingAuthorities = (bundlePath) => {
  const result = spawnSync("codesign", ["-dvv", bundlePath], { encoding: "utf8" });
  return `${result.stdout}${result.stderr}`;
};

const decodedProfile = (bundlePath, scratchDir) => {
  const source = join(bundlePath, EMBEDDED_PROFILE_NAME);
  if (!existsSync(source)) {
    fail(`${bundlePath} has no ${EMBEDDED_PROFILE_NAME}`);
  }
  const decoded = join(scratchDir, `${bundlePath.split("/").pop()}.profile.plist`);
  execFileSync("security", ["cms", "-D", "-i", source, "-o", decoded]);
  return decoded;
};

const assertDistributionSigned = (label, bundlePath, scratchDir) => {
  const entitlements = signedEntitlements(bundlePath);
  // App Store profiles carry get-task-allow=false rather than dropping the key,
  // so the invariant is "not debuggable", not "absent".
  if (entitlements["get-task-allow"] === true) {
    fail(`${label}: get-task-allow is true — this is a development signature`);
  }
  if (!signingAuthorities(bundlePath).includes(DISTRIBUTION_AUTHORITY)) {
    fail(`${label}: not signed by an ${DISTRIBUTION_AUTHORITY} identity`);
  }
  const profile = decodedProfile(bundlePath, scratchDir);
  if (plistHasKey(profile, PROVISIONED_DEVICES_KEY)) {
    fail(
      `${label}: embedded profile "${plistValue(profile, "Name")}" lists provisioned devices — App Store profiles do not`,
    );
  }
  return entitlements;
};

const assertKeychainGroups = (label, entitlements) => {
  const groups = entitlements["keychain-access-groups"];
  if (!Array.isArray(groups) || groups.length !== 1) {
    fail(
      `${label}: signed keychain-access-groups must be exactly one entry, got ${JSON.stringify(groups)}`,
    );
  }
  if (!groups[0].endsWith(EXPECTED_KEYCHAIN_GROUP_SUFFIX)) {
    fail(`${label}: signed keychain group ${groups[0]} does not end in ${EXPECTED_KEYCHAIN_GROUP_SUFFIX}`);
  }
};

const verifyExport = (exportPath, version, buildNumber) => {
  const ipa = exportedIpa(exportPath);
  const scratchDir = join(exportPath, "verify");
  rmSync(scratchDir, { recursive: true, force: true });
  mkdirSync(scratchDir, { recursive: true });
  run("unzip", ["-q", ipa, "-d", scratchDir]);

  const app = join(scratchDir, "Payload", APP_NAME);
  const extension = join(app, "PlugIns", EXTENSION_NAME);
  for (const path of [app, extension]) {
    if (!existsSync(path)) {
      fail(`exported ipa is missing ${path}`);
    }
  }

  const appEntitlements = assertDistributionSigned("app", app, scratchDir);
  assertEqual(
    "app signed aps-environment",
    appEntitlements["aps-environment"],
    EXPECTED_APNS_ENVIRONMENT,
  );
  // Five keychain items carry no explicit access group, so iOS files them under
  // the DEFAULT one — which is the FIRST entry of this list. A shipped build
  // must therefore have exactly one entry, and it must be the shipped app's:
  // a second entry ahead of it would silently move every existing install's
  // device identity, pairing and sign key to a group they were never in.
  assertKeychainGroups("app", appEntitlements);
  // The extension's App Store profile carries no aps-environment at all — only
  // the host app routes push, so asserting one here would reject every build.
  // The extension's signed groups matter as much as the app's: it is the half
  // that READS the push key, and project.yml defines the shipped identity on
  // both targets. Asserting only the app would leave the doc sentence about
  // "the whole shipped identity" a claim the checks do not keep.
  assertKeychainGroups("extension", assertDistributionSigned("extension", extension, scratchDir));

  const appInfo = join(app, "Info.plist");
  assertEqual(
    "exported app marketing version",
    plistValue(appInfo, "CFBundleShortVersionString"),
    version,
  );
  assertEqual(
    "exported app build number",
    plistValue(appInfo, "CFBundleVersion"),
    buildNumber,
  );
  assertEqual(
    "exported BayboApnsEnvironment",
    plistValue(appInfo, "BayboApnsEnvironment"),
    EXPECTED_APNS_ENVIRONMENT,
  );
  assertEqual(
    "exported app bundle id",
    plistValue(appInfo, "CFBundleIdentifier"),
    EXPECTED_BUNDLE_ID,
  );
  assertEqual(
    "exported app display name",
    plistValue(appInfo, "CFBundleDisplayName"),
    EXPECTED_DISPLAY_NAME,
  );

  console.log("\nExport verification passed:");
  console.log(`  ipa: ${ipa}`);
  console.log(`  bundle id: ${EXPECTED_BUNDLE_ID} ("${EXPECTED_DISPLAY_NAME}")`);
  console.log(`  signed keychain groups: one, ${EXPECTED_KEYCHAIN_GROUP_SUFFIX.slice(1)}`);
  console.log(`  signed aps-environment: ${EXPECTED_APNS_ENVIRONMENT}`);
  console.log(`  get-task-allow: not set`);
  console.log(`  identity: ${DISTRIBUTION_AUTHORITY.replace(":", "")}`);
  console.log(`  profiles: no provisioned devices (App Store)`);
};

const main = () => {
  const { version, buildNumber, upload } = parseArgs(process.argv.slice(2));
  const timestamp = new Date().toISOString().replace(/[-:.TZ]/g, "");
  const outputRoot = join(ROOT, "build", "AppStore");
  const artifactName = `Baybo-${version}-${buildNumber}-${timestamp}`;
  const archivePath = join(outputRoot, `${artifactName}.xcarchive`);
  const exportPath = join(outputRoot, artifactName);
  const uploadPath = `${exportPath}-upload`;

  mkdirSync(outputRoot, { recursive: true });
  refreshTranscriptBundle();
  run("bash", ["scripts/build-core.sh", "--release"]);
  run("xcodegen", ["generate"]);
  run("xcodebuild", [
    "-project", PROJECT,
    "-scheme", SCHEME,
    "-configuration", CONFIGURATION,
    "-sdk", "iphoneos",
    "-destination", "generic/platform=iOS",
    "-archivePath", archivePath,
    "-allowProvisioningUpdates",
    `MARKETING_VERSION=${version}`,
    `CURRENT_PROJECT_VERSION=${buildNumber}`,
    "archive",
  ]);

  validateArchive(archivePath, version, buildNumber);
  exportArchive(archivePath, exportPath, localExportOptions(outputRoot));
  verifyExport(exportPath, version, buildNumber);

  if (upload) {
    exportArchive(archivePath, uploadPath, EXPORT_OPTIONS);
    console.log(`\nUpload completed: ${version} (${buildNumber})`);
  } else {
    console.log(`\nArchive ready: ${archivePath}`);
    console.log(`Verified export: ${exportPath}`);
    console.log("Re-run with --upload to build, verify, and upload a fresh archive.");
  }
};

try {
  main();
} catch (error) {
  console.error(`\nrelease failed: ${error instanceof Error ? error.message : error}`);
  process.exitCode = 1;
}
