#!/usr/bin/env node
import { execFileSync, spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
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
const DEBUG_SEED_SYMBOL = "debug_seed_push_key";
const EXPORT_OPTIONS = join(ROOT, "scripts", "ExportOptions-AppStore.plist");
const USAGE = `Usage:
  node scripts/release.mjs --version <x.y.z> --build-number <integer> [--upload]

The default action creates and validates an App Store archive. --upload exports
and uploads that archive to App Store Connect.`;

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
  if (!buildNumber || !/^[1-9]\d*$/.test(buildNumber)) {
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
  console.log(`  APNs environment: ${EXPECTED_APNS_ENVIRONMENT}`);
  console.log(`  app + extension: ${version} (${buildNumber})`);
  console.log(`  debug seed symbol: absent`);
};

const main = () => {
  const { version, buildNumber, upload } = parseArgs(process.argv.slice(2));
  const timestamp = new Date().toISOString().replace(/[-:.TZ]/g, "");
  const outputRoot = join(ROOT, "build", "AppStore");
  const artifactName = `Baybo-${version}-${buildNumber}-${timestamp}`;
  const archivePath = join(outputRoot, `${artifactName}.xcarchive`);
  const exportPath = join(outputRoot, artifactName);

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

  if (upload) {
    run("xcodebuild", [
      "-exportArchive",
      "-archivePath", archivePath,
      "-exportPath", exportPath,
      "-exportOptionsPlist", EXPORT_OPTIONS,
      "-allowProvisioningUpdates",
    ]);
    console.log(`\nUpload completed from ${archivePath}`);
  } else {
    console.log(`\nArchive ready: ${archivePath}`);
    console.log("Re-run with --upload to build, validate, and upload a fresh archive.");
  }
};

try {
  main();
} catch (error) {
  console.error(`\nrelease failed: ${error instanceof Error ? error.message : error}`);
  process.exitCode = 1;
}
