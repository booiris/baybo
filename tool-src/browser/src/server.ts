#!/usr/bin/env node
// Aura's browser MCP sidecar is a thin wrapper around chrome-devtools-mcp:
// we call its `createMcpServer` programmatically with hardened args
// (telemetry off, isolation/persistence both via a dedicated userDataDir
// + a dedicated Chrome binary) and connect a StdioServerTransport.
//
// What we install: **Google Chrome for Testing** — Google's own Chrome
// stable build, packaged for automation. Same Blink/V8/Skia source
// as the Chrome end users install from chrome.google.com, identical
// proprietary codec stack (FFmpeg with H.264/AAC), Widevine CDM
// included. The "for Testing" distribution differs from the consumer
// Chrome only in: not auto-updating, no Google sign-in/sync, no
// installer-level code-signing for general user use. Not Chromium
// (the upstream open-source build, which lacks Google branding,
// Widevine, and proprietary codecs).
//
// We deliberately do not pass through `--isolated`: that flag creates
// a temp userDataDir auto-deleted on close, which conflicts with the
// "persistent across Aura restarts" property the operator gets by
// default. We achieve "isolated from the user's real Chrome profile"
// via `userDataDir` pointed at an Aura-managed cache path + an
// `executablePath` pointing at a Chrome under Aura's cache (not the
// user's system browser).
//
// Chrome auto-install + non-blocking ready: on first boot, if no
// Chrome is found under Aura's cache, we kick off a download of
// Chrome for Testing 'stable' into `$XDG_CACHE_HOME/aura/browser/chrome/`
// via @puppeteer/browsers in the background. The MCP server connects
// immediately so the gateway sees the full tool list and the LLM can
// reason about browser availability. Any `tools/call` arriving while
// the download is in progress is intercepted by `GuardingTransport`
// and answered with a synthetic "Chrome installing, X%" response —
// the LLM gets actionable progress and can retry. Once the download
// completes, the `args.executablePath` field is mutated in place, and
// CDDM's `getContext()` (which re-reads `serverArgs.executablePath`
// on every tool call) picks up the new path on the next attempt.
//
// Operator-facing knobs all live in `aura.json:browser.*`:
// - `enable`, `chrome_path`, `sandbox`, `profile_dir` flow through
//   `aura_tools::mcp::profile::browser_mcp_profile` into the child's
//   env as internal IPC vars (`AURA_BROWSER_CHROME_PATH`,
//   `AURA_BROWSER_NO_SANDBOX`, `AURA_BROWSER_PROFILE_DIR`).
// - The env vars themselves are NOT a public interface; setting them
//   directly works in development but isn't documented or supported.

import { existsSync, mkdirSync, readFileSync, readlinkSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir, hostname as osHostname, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { createMcpServer } from "chrome-devtools-mcp";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import {
  Browser,
  computeExecutablePath,
  detectBrowserPlatform,
  getInstalledBrowsers,
  install,
  resolveBuildId,
} from "@puppeteer/browsers";

import type { CreateMcpServerArgs } from "chrome-devtools-mcp";

import {
  type DockerHandle,
  type DockerPhase,
  checkDockerAvailable,
  isPidAlive,
  spawnContainer,
  sweepStaleContainers,
} from "./docker.js";
import { READ_PAGE_TOOL, handleReadPage } from "./read_page.js";

type ServerArgs = CreateMcpServerArgs;

type InstallPhase = "installing" | "ready" | "failed" | DockerPhase;

interface InstallState {
  phase: InstallPhase;
  percent: number;
  error?: string;
  buildId?: string;
}

// Chrome takes no font-dir flag; the only way to add a font dir is via
// fontconfig's FONTCONFIG_FILE. macOS Chrome uses Core Text and ignores
// fontconfig — the override is a no-op there.
function applyFontconfigOverride(): void {
  const raw = process.env["AURA_BROWSER_EXTRA_FONT_DIRS"];
  if (!raw) return;
  const dirs = raw.split(":").filter((d) => d.length > 0);
  if (dirs.length === 0) return;

  for (const d of dirs) {
    try {
      mkdirSync(d, { recursive: true });
    } catch (e) {
      log(`mkdir ${d} failed: ${(e as Error).message}`);
    }
  }

  const systemIncludes = [
    "/etc/fonts/fonts.conf",
    "/usr/local/etc/fonts/fonts.conf",
    "/opt/homebrew/etc/fonts/fonts.conf",
  ];
  const xml =
    `<?xml version="1.0"?>\n` +
    `<!DOCTYPE fontconfig SYSTEM "fonts.dtd">\n` +
    `<fontconfig>\n` +
    systemIncludes.map((p) => `  <include ignore_missing="yes">${p}</include>`).join("\n") +
    "\n" +
    dirs.map((d) => `  <dir>${d}</dir>`).join("\n") +
    `\n</fontconfig>\n`;

  const confPath = join(tmpdir(), `aura-browser-fonts-${process.pid}.conf`);
  writeFileSync(confPath, xml);
  process.env["FONTCONFIG_FILE"] = confPath;
  log(`fontconfig override: FONTCONFIG_FILE=${confPath} extra_dirs=${dirs.join(",")}`);
}

function parseViewport(raw: string | undefined): { width: number; height: number } | undefined {
  if (!raw) return undefined;
  const m = raw.match(/^(\d+)x(\d+)$/);
  if (!m) {
    process.stderr.write(`[browser-mcp] AURA_BROWSER_VIEWPORT '${raw}' is not WxH; ignoring\n`);
    return undefined;
  }
  return { width: Number(m[1]), height: Number(m[2]) };
}

function xdgCacheHome(): string {
  const xdg = process.env["XDG_CACHE_HOME"];
  return xdg && xdg.length > 0 ? xdg : join(homedir(), ".cache");
}

function defaultProfileDir(): string {
  return join(xdgCacheHome(), "aura", "browser", "profile");
}

function defaultChromeCacheDir(): string {
  return process.env["AURA_BROWSER_CACHE_DIR"] ?? join(xdgCacheHome(), "aura", "browser", "chrome");
}

const log = (msg: string): void => {
  process.stderr.write(`[browser-mcp] ${msg}\n`);
};

/**
 * Synchronous fast path: if a Chrome is already on disk (operator
 * pinned via `aura.json:browser.chrome_path`, plumbed to us as
 * `AURA_BROWSER_CHROME_PATH` by the Rust profile builder; or cached
 * from a previous run), return its path. Returns `null` if nothing
 * is available — the caller then kicks off the background download
 * via {@link installChromeInBackground}.
 *
 * `AURA_BROWSER_CHROME_PATH` is an internal IPC channel between the
 * gateway and the sidecar; operators configure the path through
 * `aura.json:browser.chrome_path` only.
 */
async function findExistingChrome(): Promise<string | null> {
  const pinned = process.env["AURA_BROWSER_CHROME_PATH"];
  if (pinned && pinned.length > 0) {
    if (existsSync(pinned)) {
      log(`using configured Chrome: ${pinned}`);
      return pinned;
    }
    log(
      `aura.json:browser.chrome_path=${pinned} does not exist on disk; ` +
        `falling through to cache lookup`,
    );
  }

  const cacheDir = defaultChromeCacheDir();
  mkdirSync(cacheDir, { recursive: true });

  const platform = detectBrowserPlatform();
  if (!platform) {
    throw new Error(
      "@puppeteer/browsers detectBrowserPlatform returned null — unsupported OS/arch combo. " +
        "Set aura.json:browser.chrome_path to a Chrome binary you have available.",
    );
  }

  const installed = await getInstalledBrowsers({ cacheDir });
  const cached = installed.find((b) => b.browser === Browser.CHROME);
  if (cached) {
    log(`using cached Chrome: buildId=${cached.buildId} path=${cached.executablePath}`);
    return cached.executablePath;
  }

  return null;
}

/**
 * Long-running async path: download Google Chrome for Testing 'stable'
 * into Aura's cache and update `state` in place so the
 * {@link GuardingTransport} can surface progress. Resolves to the
 * installed executable path on success; throws on failure.
 */
async function installChromeInBackground(state: InstallState): Promise<string> {
  const cacheDir = defaultChromeCacheDir();
  const platform = detectBrowserPlatform();
  if (!platform) {
    throw new Error("@puppeteer/browsers: unsupported platform for auto-install");
  }
  log(`no Chrome in ${cacheDir}; resolving Chrome for Testing 'stable' buildId for ${platform}`);
  const buildId = await resolveBuildId(Browser.CHROME, platform, "stable");
  state.buildId = buildId;
  log(`downloading Chrome for Testing buildId=${buildId} platform=${platform} -> ${cacheDir}`);

  let lastReport = -1;
  await install({
    cacheDir,
    browser: Browser.CHROME,
    buildId,
    downloadProgressCallback: (downloaded: number, total: number): void => {
      if (total <= 0) return;
      const percent = Math.floor((downloaded / total) * 100);
      state.percent = percent;
      if (percent >= lastReport + 10) {
        const mb = (n: number): string => (n / (1024 * 1024)).toFixed(1);
        log(`download ${percent}% (${mb(downloaded)}/${mb(total)} MiB)`);
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
  log(`Chrome installed at ${exe}`);
  return exe;
}

function buildArgs(executablePath: string | undefined): ServerArgs {
  const userDataDir = process.env["AURA_BROWSER_PROFILE_DIR"] ?? defaultProfileDir();
  mkdirSync(userDataDir, { recursive: true });

  const proxyServer = process.env["AURA_BROWSER_PROXY"] || undefined;
  const viewport = parseViewport(process.env["AURA_BROWSER_VIEWPORT"]);

  // Chrome's renderer sandbox is on by default. The Rust profile
  // builder sets AURA_BROWSER_NO_SANDBOX=1 when `aura.json:browser.sandbox`
  // is false (the default — most container/CI hosts can't satisfy
  // Chrome's user-namespace prerequisites). Append `--no-sandbox` to
  // chromeArg so it reaches Chrome at launch.
  const chromeArg: string[] = [];
  if (process.env["AURA_BROWSER_NO_SANDBOX"] === "1") {
    chromeArg.push("--no-sandbox");
  }

  return {
    headless: true,
    isolated: false,
    usageStatistics: false,
    performanceCrux: false,
    userDataDir,
    executablePath,
    proxyServer,
    viewport,
    chromeArg: chromeArg.length > 0 ? chromeArg : undefined,
    redactNetworkHeaders: true,
    categoryNavigationAutomation: true,
    categoryDebugging: true,
    categoryEmulation: true,
    categoryPerformance: true,
    categoryNetwork: true,
    categoryExtensions: false,
    slim: false,
  };
}

// ---------------------------------------------------------------------
// GuardingTransport
// ---------------------------------------------------------------------
//
// Wraps a real `StdioServerTransport` with one job: while Chrome is
// downloading, intercept `tools/call` requests and reply with a
// synthetic "still installing" message. Everything else (initialize,
// notifications/initialized, tools/list, ping, etc.) flows through
// unchanged so the gateway sees a fully-functional MCP server from
// the moment we connect.

interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: number | string;
  method: string;
  params?: unknown;
}

interface JsonRpcMessage {
  jsonrpc?: "2.0";
  id?: number | string;
  method?: string;
  params?: unknown;
  result?: unknown;
  error?: unknown;
}

function isToolCallRequest(msg: unknown): msg is JsonRpcRequest {
  if (typeof msg !== "object" || msg === null) return false;
  const m = msg as JsonRpcMessage;
  return (
    m.method === "tools/call" &&
    (typeof m.id === "number" || typeof m.id === "string")
  );
}

class GuardingTransport {
  private real: StdioServerTransport;
  private state: InstallState;
  public onmessage?: (msg: unknown) => void;
  public onerror?: (err: Error) => void;
  public onclose?: () => void;

  constructor(real: StdioServerTransport, state: InstallState) {
    this.real = real;
    this.state = state;
    this.real.onmessage = (msg: unknown): void => {
      if (this.state.phase !== "ready" && isToolCallRequest(msg)) {
        void this.sendSyntheticReply(msg);
        return;
      }
      this.onmessage?.(msg);
    };
    this.real.onerror = (err: Error): void => {
      this.onerror?.(err);
    };
    this.real.onclose = (): void => {
      this.onclose?.();
    };
  }

  private async sendSyntheticReply(req: JsonRpcRequest): Promise<void> {
    let text: string;
    switch (this.state.phase) {
      case "installing": {
        const buildSuffix = this.state.buildId
          ? ` (Chrome for Testing buildId=${this.state.buildId})`
          : "";
        text =
          `Browser is still being prepared for first-time setup: downloading Chrome${buildSuffix}, ` +
          `${this.state.percent}% complete. The download is ~175 MiB and runs in the background. ` +
          `Please retry this tool call in a few seconds.`;
        break;
      }
      case "docker-checking":
        text =
          `Browser is starting in docker mode: probing the docker daemon. ` +
          `Please retry this tool call in a few seconds.`;
        break;
      case "docker-building-image":
        text =
          `Browser docker image is being built for the first time (Chrome + Xvfb + deps, ~300 MiB image, ` +
          `takes 1-3 minutes). Subsequent boots reuse the cached image. Please retry shortly.`;
        break;
      case "docker-starting-container":
        text =
          `Browser container is starting up. Please retry this tool call in a few seconds.`;
        break;
      case "docker-waiting-for-cdp":
        text =
          `Browser container is up; waiting for Chrome's DevTools endpoint to come online. ` +
          `Please retry this tool call in a few seconds.`;
        break;
      case "ready":
        // Race between phase flip and the next intercepted call — let it
        // fall through to the wrapped server on the next cycle.
        this.onmessage?.(req);
        return;
      case "failed":
      default:
        text =
          `Browser is unavailable: ${this.state.error ?? "unknown error"}. ` +
          `Check the gateway logs (aura-browser-mcp lines), then either fix the underlying issue ` +
          `(network for auto-install, docker daemon for docker mode) and restart the gateway, or ` +
          `set aura.json:browser.chrome_path to an existing Chrome binary as a workaround.`;
        break;
    }
    await this.real.send({
      jsonrpc: "2.0",
      id: req.id,
      result: {
        content: [{ type: "text", text }],
        isError: true,
      },
    });
  }

  async start(): Promise<void> {
    return this.real.start();
  }
  async close(): Promise<void> {
    return this.real.close();
  }
  async send(msg: unknown): Promise<void> {
    return this.real.send(msg as Parameters<StdioServerTransport["send"]>[0]);
  }
}

/**
 * Existing host-headless flow: look for Chrome on disk; if missing,
 * kick off a background download via @puppeteer/browsers and let
 * `GuardingTransport` surface progress while CDDM connects with a
 * pending executablePath. Mutates `state` and `args.executablePath`
 * in place.
 *
 * Extracted from the docker pivot so this codepath stays pixel-
 * identical to pre-docker behaviour for operators who don't flip
 * `browser.docker.enable`.
 */
async function runHostFallback(state: InstallState): Promise<ServerArgs> {
  // Puppeteer reads process.env at spawn time, so calling this here
  // (not at import time) is fine.
  applyFontconfigOverride();

  // Clean any stale singleton lock left by a previous docker container
  // (different hostname) or a crashed previous host Chrome (PID dead).
  // Live-PID-on-this-hostname locks are left alone so a real conflict
  // still surfaces.
  const profileDir = process.env["AURA_BROWSER_PROFILE_DIR"] ?? defaultProfileDir();
  try {
    mkdirSync(profileDir, { recursive: true });
    clearStaleChromeLocks(profileDir);
  } catch (e) {
    log(`profile dir prep failed: ${(e as Error).message}`);
  }

  let initialPath: string | null = null;
  try {
    initialPath = await findExistingChrome();
  } catch (e) {
    state.phase = "failed";
    state.error = e instanceof Error ? e.message : String(e);
  }

  if (initialPath) {
    state.phase = "ready";
  } else if (state.phase !== "failed") {
    state.phase = "installing";
  }

  const args = buildArgs(initialPath ?? undefined);

  if (state.phase === "installing") {
    void installChromeInBackground(state).then(
      (exe) => {
        args.executablePath = exe;
        state.phase = "ready";
        state.percent = 100;
        log("Chrome ready; browser tools are live");
      },
      (err: unknown) => {
        state.phase = "failed";
        state.error = err instanceof Error ? err.message : String(err);
        log(`Chrome install failed: ${state.error}`);
      },
    );
  }
  return args;
}

/**
 * Clear stale Chrome `Singleton*` lock symlinks from a profile dir.
 *
 * Chrome refuses to start when the lock points at a hostname different
 * from the current one (it can't kill-probe across hosts to verify the
 * PID is dead, so it conservatively assumes the other instance is
 * still alive — the classic NFS-shared-profile case). This trips up
 * docker↔host-headless mode switches: the docker container has a
 * different hostname than the host, so each side leaves a lock the
 * other can't clear.
 *
 * Within an Aura process, host-headless and docker modes are mutually
 * exclusive — we never run both Chromes against this profile at the
 * same time. So a hostname mismatch is always stale. A *same-hostname*
 * lock with a live PID is the only case we leave alone (it'd be a real
 * concurrent-instance conflict that Chrome should fail loudly on).
 */
/**
 * Cross-check that a live PID actually belongs to a Chrome family
 * process before respecting its `Singleton*` lock. Linux PIDs are
 * recycled, so a previous Chrome PID that died, then got reissued to
 * an unrelated process (sleep, sshd worker, …), would otherwise pass
 * `isPidAlive` and block Chrome from ever starting on this profile.
 *
 * Linux: read `/proc/<pid>/comm`. macOS: `/proc` doesn't exist; fall
 * back to "assume Chrome" so behaviour matches today's conservative
 * default. Docker entrypoint runs the same dance inside the container
 * (Linux), so the check is effective in both modes.
 */
function isLikelyChromeProcess(pid: number): boolean {
  try {
    const comm = readFileSync(`/proc/${pid}/comm`, "utf8").trim();
    return /^(chrome|chromium|google-chrome)/i.test(comm);
  } catch {
    return true;
  }
}

function clearStaleChromeLocks(profileDir: string): void {
  const myHostname = osHostname();
  for (const name of ["SingletonLock", "SingletonCookie", "SingletonSocket"]) {
    const lockPath = join(profileDir, name);
    let target: string;
    try {
      target = readlinkSync(lockPath);
    } catch {
      continue;
    }
    const m = target.match(/^(.+)-(\d+)$/);
    if (!m) {
      log(`Chrome lock ${lockPath} has unexpected target '${target}'; leaving alone`);
      continue;
    }
    const lockHostname = m[1] ?? "";
    const lockPid = Number(m[2]);
    if (lockHostname === myHostname && Number.isInteger(lockPid)) {
      if (isPidAlive(lockPid) && isLikelyChromeProcess(lockPid)) {
        log(
          `Chrome lock ${lockPath} points at live PID ${lockPid} on this host; leaving alone ` +
            `(would race a real running instance — close it first or use a different profile_dir)`,
        );
        continue;
      }
      const reason = isPidAlive(lockPid)
        ? `PID ${lockPid} is alive but is not a Chrome process (likely PID reuse on a busy host)`
        : `PID ${lockPid} is dead on ${myHostname}`;
      log(`clearing stale Chrome lock ${lockPath} (${reason})`);
    } else {
      log(
        `clearing stale Chrome lock ${lockPath} (target ${target} from a different host or container)`,
      );
    }
    try {
      unlinkSync(lockPath);
    } catch (e) {
      log(`failed to remove ${lockPath}: ${(e as Error).message}`);
    }
  }
}

function dockerDirPath(): string {
  // Bundle materialises to $XDG_CACHE_HOME/aura/sidecars/browser-<hash>/bundle.mjs;
  // the `docker/` aux asset (declared in package.json:aura.auxAssets)
  // lands as a sibling under the same hash dir.
  return resolve(dirname(fileURLToPath(import.meta.url)), "docker");
}

function pickFirstExistingDir(raw: string | undefined): string | undefined {
  if (!raw) return undefined;
  for (const d of raw.split(":").filter((s) => s.length > 0)) {
    try {
      if (statSync(d).isDirectory()) return d;
    } catch {
      // not a dir / not present → skip
    }
  }
  return undefined;
}

interface DockerSpawnOutcome {
  args: ServerArgs;
  handle: DockerHandle;
}

/**
 * Resolve the docker-mode args. Throws on every failure path; the
 * caller catches and falls back to the host-headless flow.
 */
async function trySpawnDocker(state: InstallState): Promise<DockerSpawnOutcome> {
  state.phase = "docker-checking";
  const check = await checkDockerAvailable();
  if (!check.ok) {
    throw new Error(`docker not available: ${check.reason}`);
  }
  log(`docker daemon reachable (server ${check.serverVersion}); sweeping stale containers`);
  // Operator-set browser.sandbox=true doesn't reach Chrome in docker mode:
  // the container's entrypoint hardcodes --no-sandbox because the slim
  // base ships no SUID chrome-sandbox helper. Warn loudly so the operator
  // doesn't think the sandbox is on when it isn't.
  if (process.env["AURA_BROWSER_NO_SANDBOX"] !== "1") {
    log(
      "browser.sandbox=true ignored in docker mode — the container is the trust boundary " +
        "and Chrome runs with --no-sandbox inside (see CLAUDE.md 'Docker mode' subsection)",
    );
  }
  await sweepStaleContainers();

  // Docker mode uses consumer Google Chrome installed inside the image
  // via apt (NOT @puppeteer/browsers / Chrome for Testing). Image is
  // pinned by `aura-browser:<sha256(Dockerfile + entrypoint.sh)[..12]>`
  // and gets whatever Chrome stable was current at build time. No host-
  // side Chrome version lookup needed — the only network call is the
  // initial `wget` inside the Dockerfile, which docker handles.
  const operatorImageTag = process.env["AURA_BROWSER_DOCKER_IMAGE_TAG"] || undefined;

  const profileDir = process.env["AURA_BROWSER_PROFILE_DIR"] ?? defaultProfileDir();
  // Belt-and-braces: the entrypoint inside the container also clears
  // stale locks, but doing it host-side first means the bind-mounted
  // /data/profile already looks clean to Chrome on first launch (avoids
  // the rare race where Chrome reads the lock before the entrypoint's
  // cleanup fires).
  try {
    mkdirSync(profileDir, { recursive: true });
    clearStaleChromeLocks(profileDir);
  } catch (e) {
    log(`profile dir prep failed: ${(e as Error).message}`);
  }

  const handle = await spawnContainer({
    dockerDir: dockerDirPath(),
    imageTag: operatorImageTag,
    profileDir,
    fontDir: pickFirstExistingDir(process.env["AURA_BROWSER_EXTRA_FONT_DIRS"]),
    webVncPort: parseVncPort(
      process.env["AURA_BROWSER_DOCKER_WEB_VNC_PORT"],
      "WEB_VNC_PORT",
    ),
    viewport: parseViewport(process.env["AURA_BROWSER_VIEWPORT"]) ?? { width: 1920, height: 1080 },
    onPhase: (p) => {
      state.phase = p;
    },
  });

  // CDDM ignores `headless` in connect-mode; set false anyway so the
  // boot summary reads correctly and the operator's mental model
  // matches reality.
  const args = buildArgs(undefined);
  args.browserUrl = handle.cdpUrl;
  args.headless = false;
  return { args, handle };
}

function parseVncPort(raw: string | undefined, label: string): number | undefined {
  if (!raw) return undefined;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 1 || n > 65535) {
    log(`AURA_BROWSER_DOCKER_${label} '${raw}' is not a valid TCP port; ignoring`);
    return undefined;
  }
  return n;
}

async function main(): Promise<void> {
  const state: InstallState = { phase: "installing", percent: 0 };
  const dockerCdpUrl = process.env["AURA_BROWSER_DOCKER_CDP_URL"];
  const dockerEnable = process.env["AURA_BROWSER_DOCKER_ENABLE"] === "1";
  let dockerHandle: DockerHandle | null = null;
  let args: ServerArgs;
  let mode: "cdp_url" | "docker" | "host";

  if (dockerCdpUrl) {
    log(`docker.cdp_url set; connecting to existing CDP at ${dockerCdpUrl}`);
    args = buildArgs(undefined);
    args.browserUrl = dockerCdpUrl;
    args.headless = false;
    state.phase = "ready";
    mode = "cdp_url";
  } else if (dockerEnable && process.platform === "darwin") {
    // Docker Desktop on macOS runs Linux containers in a hidden VM, so
    // "docker mode" would still be Linux Chrome behind a VM — defeats
    // the "real native Chrome" point. macOS operators get host-headless
    // with their native Chrome regardless of `docker.enable`. The
    // explicit log makes this intentional override visible (an operator
    // who set the flag should not have to wonder why it didn't take).
    log(
      "docker.enable=true ignored on macOS — Docker Desktop runs Linux containers in a VM, so " +
        "the in-container Chrome would be Linux Chrome behind that VM, not native macOS Chrome. " +
        "Falling back to host-headless with native macOS Chrome.",
    );
    args = await runHostFallback(state);
    mode = "host";
  } else if (dockerEnable) {
    try {
      const outcome = await trySpawnDocker(state);
      args = outcome.args;
      dockerHandle = outcome.handle;
      state.phase = "ready";
      mode = "docker";
      log(`docker container ${dockerHandle.containerName} (image ${dockerHandle.imageTag}) ready; CDP=${dockerHandle.cdpUrl}`);
    } catch (e) {
      const reason = e instanceof Error ? e.message : String(e);
      log(`docker mode unavailable: ${reason}; falling back to host-headless`);
      // Reset phase so the host fallback resumes from a clean slate.
      state.phase = "installing";
      state.error = undefined;
      args = await runHostFallback(state);
      mode = "host";
    }
  } else {
    args = await runHostFallback(state);
    mode = "host";
  }

  const { server: cddmServer } = await createMcpServer(args, {});

  // Proxy layer: CDDM speaks MCP on one end of an in-process transport
  // pair; we drive it via a Client on the other end. Our outer Server
  // is what the gateway actually talks to over stdio. This lets us
  // (1) inject extra tools like `browser/read_page` without touching
  // CDDM, and (2) selectively rewrite tool calls in the future.
  const [cddmInner, cddmOuter] = InMemoryTransport.createLinkedPair();
  await cddmServer.connect(cddmInner);
  const cddmClient = new Client(
    { name: "aura-browser-proxy", version: "1.0.0" },
    { capabilities: {} },
  );
  await cddmClient.connect(cddmOuter);

  const proxy = new Server(
    { name: "aura-browser", version: "1.0.0" },
    { capabilities: { tools: {} } },
  );
  proxy.setRequestHandler(ListToolsRequestSchema, async () => {
    const cddmTools = await cddmClient.listTools();
    return { tools: [...cddmTools.tools, READ_PAGE_TOOL] };
  });
  proxy.setRequestHandler(CallToolRequestSchema, async (req) => {
    if (req.params.name === READ_PAGE_TOOL.name) {
      return await handleReadPage(cddmClient);
    }
    return await cddmClient.callTool({
      name: req.params.name,
      arguments: req.params.arguments,
    });
  });

  const realTransport = new StdioServerTransport();
  const transport = new GuardingTransport(realTransport, state);
  await proxy.connect(transport as unknown as StdioServerTransport);

  const chromeArgList = args.chromeArg && args.chromeArg.length > 0 ? args.chromeArg.join(",") : "";
  const viewportStr = args.viewport ? `${args.viewport.width}x${args.viewport.height}` : "<chrome default>";
  const target = args.browserUrl
    ? `browserUrl=${args.browserUrl}`
    : `executable=${args.executablePath ?? "<pending install>"}`;
  log(
    `chrome-devtools-mcp ready: mode=${mode} userDataDir=${args.userDataDir} ${target} ` +
      `viewport=${viewportStr} headless=${args.headless} ` +
      `sandbox=${chromeArgList.includes("--no-sandbox") ? "off" : "on"} ` +
      `telemetry=off install_state=${state.phase} extra_tools=read_page`,
  );

  const shutdown = (): void => {
    log("shutting down");
    if (dockerHandle) {
      void dockerHandle.stop().catch(() => undefined);
    }
    void proxy.close().catch(() => undefined);
    void cddmClient.close().catch(() => undefined);
    void cddmServer.close().catch(() => undefined);
    setTimeout(() => process.exit(0), 50).unref();
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

main().catch((e: unknown) => {
  const msg = e instanceof Error ? e.message : String(e);
  process.stderr.write(`[browser-mcp] fatal: ${msg}\n`);
  process.exit(1);
});
