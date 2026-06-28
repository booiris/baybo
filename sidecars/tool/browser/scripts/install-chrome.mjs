// Optional Chrome pre-warm for the browser tool sidecar.
//
// What this installs: Google Chrome for Testing — Google's own Chrome
// stable build, packaged for automation (same source/codecs/Widevine
// as consumer Chrome, minus auto-update + sign-in). Not Chromium (the
// open-source upstream).
//
// The sidecar auto-downloads Chrome for Testing into Baybo's cache on
// first boot (see `sidecars/tool/browser/src/server.ts::findExistingChrome`),
// so this script is **not required for normal operation**. Run it
// only when you want to seed the cache before first agent use — e.g.
// in CI to bake Chrome into a container image, or in air-gapped
// environments where the gateway machine has no outbound HTTP.
//
// Cache location matches what the wrapper looks for, so a successful
// run here means the wrapper's cache lookup hits on the next boot.

import {
  Browser,
  computeExecutablePath,
  detectBrowserPlatform,
  getInstalledBrowsers,
  install,
  resolveBuildId,
} from "@puppeteer/browsers";
import { homedir } from "node:os";
import { join } from "node:path";
import { mkdirSync } from "node:fs";

function xdgCacheHome() {
  const xdg = process.env["XDG_CACHE_HOME"];
  return xdg && xdg.length > 0 ? xdg : join(homedir(), ".cache");
}

const cacheDir =
  process.env["BAYBO_BROWSER_CACHE_DIR"] ?? join(xdgCacheHome(), "baybo", "browser", "chrome");
mkdirSync(cacheDir, { recursive: true });

const platform = detectBrowserPlatform();
if (!platform) {
  console.error("install-chrome: unsupported platform");
  process.exit(1);
}

const installed = await getInstalledBrowsers({ cacheDir });
const cached = installed.find((b) => b.browser === Browser.CHROME);
if (cached) {
  console.log(`Chrome already cached at buildId=${cached.buildId}`);
  console.log(`  path: ${cached.executablePath}`);
  console.log("Sidecar will pick this up automatically on next boot.");
  process.exit(0);
}

const buildId = await resolveBuildId(Browser.CHROME, platform, "stable");
console.log(`installing Chrome for Testing buildId=${buildId} platform=${platform} -> ${cacheDir}`);

let lastReport = -1;
await install({
  cacheDir,
  browser: Browser.CHROME,
  buildId,
  downloadProgressCallback: (downloaded, total) => {
    if (total <= 0) return;
    const percent = Math.floor((downloaded / total) * 100);
    if (percent >= lastReport + 10) {
      const mb = (n) => (n / (1024 * 1024)).toFixed(1);
      console.log(`download ${percent}% (${mb(downloaded)}/${mb(total)} MiB)`);
      lastReport = percent;
    }
  },
});

const exe = computeExecutablePath({
  cacheDir,
  browser: Browser.CHROME,
  buildId,
  platform,
});
console.log("");
console.log("Chrome installed.");
console.log(`  path: ${exe}`);
console.log("Sidecar will pick this up automatically on next boot.");
