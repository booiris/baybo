import { spawn, spawnSync } from "node:child_process";
import { homedir } from "node:os";
import { join } from "node:path";

// The Apple Development team the iOS app + Notification Extension are signed
// against. It's hardcoded in the Xcode project (`DEVELOPMENT_TEAM` in
// src-tauri/gen/apple/project.yml and the generated .xcodeproj), but it also has
// to be in the environment here: Tauri reads APPLE_DEVELOPMENT_TEAM to fill
// `teamID` in the ExportOptions.plist it generates for the `-exportArchive` step,
// and that step does NOT consult the Xcode project's build settings. Keep this in
// sync with project.yml if the team ever changes.
const IOS_DEVELOPMENT_TEAM = "KLK5BP5YS6";

const args = process.argv.slice(2);
const env = { ...process.env };

if (args[0] === "ios") {
  env.APPLE_DEVELOPMENT_TEAM = IOS_DEVELOPMENT_TEAM;
  // `ios build` / `ios dev` codesign the app (dev installs a signed build on the
  // device). In a non-GUI (SSH) session codesign can't reach the signing key, so
  // prepare the keychain first — covers every iOS build path (`pnpm tauri ios
  // dev`, `pnpm ios:build`, `pnpm ios:install`, which all route through here).
  if (args[1] === "build" || args[1] === "dev") {
    ensureKeychainSigningAccess();
  }
}

const child = spawn("tauri", args, { stdio: "inherit", env });

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});

child.on("error", (error) => {
  console.error(`baybo-mobile: failed to run tauri: ${error.message}`);
  process.exit(1);
});

// In a non-GUI (Background) launchd session — most commonly SSH — codesign can't
// present its "allow access to key" prompt, so signing dies with
// `errSecInternalComponent`. Unlock the login keychain and grant apple-tool:/apple:
// (codesign) non-interactive access to the signing keys before building. `security`
// reads the password straight from the terminal (getpass), so it never passes
// through this process, its argv, or the shell history. No-op in a GUI session (the
// prompt works there). Set BAYBO_SKIP_KEYCHAIN_PREP=1 to skip entirely — e.g. you
// manage signing access yourself, or use a dedicated CI keychain.
function ensureKeychainSigningAccess() {
  if (process.env.BAYBO_SKIP_KEYCHAIN_PREP === "1" || isGuiSession()) {
    return;
  }
  const keychain = join(homedir(), "Library", "Keychains", "login.keychain-db");
  console.error(
    "baybo-mobile: non-GUI session (e.g. SSH) — preparing the login keychain so\n" +
      "  codesign can reach the signing key. `security` will prompt for your login/\n" +
      "  keychain password (hidden; never stored or put on the command line). It may\n" +
      "  ask twice (unlock, then grant). Set BAYBO_SKIP_KEYCHAIN_PREP=1 to skip.",
  );
  // Unlock for this session, then grant codesign non-interactive key access. The
  // partition-list grant is persistent (needed once), but re-asserting is cheap.
  securityPrep(["unlock-keychain", keychain]);
  securityPrep(["set-key-partition-list", "-S", "apple-tool:,apple:", "-s", keychain]);
}

function isGuiSession() {
  // `launchctl managername` prints "Aqua" only in a GUI login session (the one that
  // can show the codesign keychain prompt); "Background" / "System" otherwise. On
  // any failure, assume non-GUI so the prep still runs.
  const result = spawnSync("launchctl", ["managername"], { encoding: "utf8" });
  return result.status === 0 && result.stdout.trim() === "Aqua";
}

function securityPrep(secArgs) {
  // Discard stdout — `set-key-partition-list` dumps the matched key's attributes
  // there on success (noisy, harmless). The password prompt goes to /dev/tty
  // (getpass) and errors to stderr, so both still show. Non-fatal: if the user
  // cancels, or the grant is already in place, let the build proceed — a genuinely
  // missing grant still surfaces as the codesign error (with the documented fix).
  const result = spawnSync("security", secArgs, { stdio: ["inherit", "ignore", "inherit"] });
  if (result.status !== 0) {
    console.error(`baybo-mobile: security ${secArgs[0]} exited ${result.status ?? result.signal}; continuing.`);
  }
}
