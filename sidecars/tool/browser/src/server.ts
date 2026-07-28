#!/usr/bin/env node
// Baybo's browser MCP sidecar is a thin wrapper around chrome-devtools-mcp:
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
// "persistent across Baybo restarts" property the operator gets by
// default. We achieve "isolated from the user's real Chrome profile"
// via `userDataDir` pointed at an Baybo-managed cache path + an
// `executablePath` pointing at a Chrome under Baybo's cache (not the
// user's system browser).
//
// Chrome auto-install + non-blocking ready: on first boot, if no
// Chrome is found under Baybo's cache, we kick off a download of
// Chrome for Testing 'stable' into `$XDG_CACHE_HOME/baybo/browser/chrome/`
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
// Operator-facing knobs all live in `baybo.json:browser.*`:
// - `enable`, `chrome_path`, `sandbox`, `profile_dir` flow through
//   `baybo_tools::mcp::profile::browser_mcp_profile` into the child's
//   env as internal IPC vars (`BAYBO_BROWSER_CHROME_PATH`,
//   `BAYBO_BROWSER_NO_SANDBOX`, `BAYBO_BROWSER_PROFILE_DIR`).
// - The env vars themselves are NOT a public interface; setting them
//   directly works in development but isn't documented or supported.

import { execFile } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readlinkSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir, hostname as osHostname, tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import { createMcpServer } from "chrome-devtools-mcp";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  type CallToolResult,
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
  type DockerSpawnOptions,
  checkDockerAvailable,
  isPidAlive,
  probeCdpEndpoint,
  spawnContainer,
  sweepStaleContainers,
} from "./docker.js";
import { createLogger, errText } from "./log.js";
import { READ_PAGE_TOOL, handleReadPage } from "./read_page.js";
import {
  BrowserActivity,
  BrowserWatchdog,
  type BrowserHealer,
  type PhaseGate,
} from "./watchdog.js";

// CDDM tools we hide from the agent's tool list. Lighthouse audits are
// slow and rarely useful for browsing; WebMCP only fires on pages that
// implement the WebMCP host protocol, which no normal site does — the
// agent would just keep calling list_webmcp_tools and getting back
// empty lists. `take_memory_snapshot` is debug-only (the rest of the
// memory category is already gated behind `experimentalMemory` in CDDM
// itself, but the take-snapshot tool ships unconditionally). Filtering
// happens in the proxy's ListTools handler so the model never sees
// them; CallTool rejects them defensively too in case a future model
// hallucinates the name.
const BLOCKED_TOOLS: ReadonlySet<string> = new Set([
  "lighthouse_audit",
  "list_webmcp_tools",
  "execute_webmcp_tool",
  "take_memory_snapshot",
]);

function proxyToolError(text: string): CallToolResult {
  return { content: [{ type: "text", text }], isError: true };
}

type ServerArgs = CreateMcpServerArgs;

// `recovering` is the watchdog's phase: the browser was ready, stopped
// answering, and repair is in flight. It matters because `GuardingTransport`
// gates on `phase !== "ready"` — without a phase to flip to, an agent calling
// a browser tool mid-restart would get a raw puppeteer "Target closed"
// instead of an actionable "retry in a few seconds".
type InstallPhase = "installing" | "ready" | "failed" | "recovering" | DockerPhase;

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
  const raw = envValue("BAYBO_BROWSER_EXTRA_FONT_DIRS");
  if (raw === undefined) return;
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

  const confPath = join(tmpdir(), `baybo-browser-fonts-${process.pid}.conf`);
  writeFileSync(confPath, xml);
  process.env["FONTCONFIG_FILE"] = confPath;
  log(`fontconfig override: FONTCONFIG_FILE=${confPath} extra_dirs=${dirs.join(",")}`);
}

function parseViewport(raw: string | undefined): { width: number; height: number } | undefined {
  if (raw === undefined) return undefined;
  const m = raw.match(/^(\d+)x(\d+)$/);
  if (!m) {
    logger.warn(`BAYBO_BROWSER_VIEWPORT '${raw}' is not WxH; ignoring`);
    return undefined;
  }
  return { width: Number(m[1]), height: Number(m[2]) };
}

/**
 * An env var's value, or `undefined` when it is unset **or empty**.
 *
 * Empty is a deliberate "not configured" sentinel on this interface, not an
 * accident: the sidecar's own test harness passes
 * `BAYBO_BROWSER_DOCKER_CDP_URL: ""` to force the host-headless path. Routing
 * every read through one helper keeps that convention in a single place
 * instead of re-deriving it at each truthiness check — where the difference
 * between "unset" and "set to empty" is invisible.
 */
function envValue(name: string): string | undefined {
  const raw = process.env[name];
  return raw !== undefined && raw.length > 0 ? raw : undefined;
}

function xdgCacheHome(): string {
  return envValue("XDG_CACHE_HOME") ?? join(homedir(), ".cache");
}

function defaultProfileDir(): string {
  return join(xdgCacheHome(), "baybo", "browser", "profile");
}

/** The Chrome user-data dir, however the operator configured it. */
function resolvedProfileDir(): string {
  return envValue("BAYBO_BROWSER_PROFILE_DIR") ?? defaultProfileDir();
}

function defaultChromeCacheDir(): string {
  // The gateway's connect_stdio strips inherited env (env_clear), so an
  // operator-set BAYBO_BROWSER_CACHE_DIR does NOT reach the runtime
  // sidecar — only the standalone `pnpm install-chrome` script (which
  // runs in the operator's shell, not via the gateway) honours it. Read
  // is intentionally absent here so the runtime path always uses the
  // canonical `$XDG_CACHE_HOME/baybo/browser/chrome/` and stays in sync
  // with the install script's default branch.
  return join(xdgCacheHome(), "baybo", "browser", "chrome");
}

const logger = createLogger("[browser-mcp]");
const log = logger.info;
const logDebug = logger.debug;

const execFileAsync = promisify(execFile);

/**
 * Synchronous fast path: if a Chrome is already on disk (operator
 * pinned via `baybo.json:browser.chrome_path`, plumbed to us as
 * `BAYBO_BROWSER_CHROME_PATH` by the Rust profile builder; or cached
 * from a previous run), return its path. Returns `null` if nothing
 * is available — the caller then kicks off the background download
 * via {@link installChromeInBackground}.
 *
 * `BAYBO_BROWSER_CHROME_PATH` is an internal IPC channel between the
 * gateway and the sidecar; operators configure the path through
 * `baybo.json:browser.chrome_path` only.
 */
async function findExistingChrome(): Promise<string | null> {
  const pinned = envValue("BAYBO_BROWSER_CHROME_PATH");
  if (pinned !== undefined) {
    if (existsSync(pinned)) {
      log(`using configured Chrome: ${pinned}`);
      return pinned;
    }
    log(
      `baybo.json:browser.chrome_path=${pinned} does not exist on disk; ` +
        `falling through to cache lookup`,
    );
  }

  const cacheDir = defaultChromeCacheDir();
  mkdirSync(cacheDir, { recursive: true });

  const platform = detectBrowserPlatform();
  if (platform === undefined) {
    throw new Error(
      "@puppeteer/browsers detectBrowserPlatform returned null — unsupported OS/arch combo. " +
        "Set baybo.json:browser.chrome_path to a Chrome binary you have available.",
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
 * into Baybo's cache and update `state` in place so the
 * {@link GuardingTransport} can surface progress. Resolves to the
 * installed executable path on success; throws on failure.
 */
async function installChromeInBackground(state: InstallState): Promise<string> {
  const cacheDir = defaultChromeCacheDir();
  const platform = detectBrowserPlatform();
  if (platform === undefined) {
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
  const userDataDir = resolvedProfileDir();
  mkdirSync(userDataDir, { recursive: true });

  const viewport = parseViewport(envValue("BAYBO_BROWSER_VIEWPORT"));

  // Chrome's renderer sandbox is on by default. The Rust profile
  // builder sets BAYBO_BROWSER_NO_SANDBOX=1 when `baybo.json:browser.sandbox`
  // is false (the default — most container/CI hosts can't satisfy
  // Chrome's user-namespace prerequisites). Append `--no-sandbox` to
  // chromeArg so it reaches Chrome at launch.
  const chromeArg: string[] = [];
  if (process.env["BAYBO_BROWSER_NO_SANDBOX"] === "1") {
    chromeArg.push("--no-sandbox");
  }

  return {
    headless: true,
    isolated: false,
    usageStatistics: false,
    performanceCrux: false,
    userDataDir,
    executablePath,
    viewport,
    chromeArg: chromeArg.length > 0 ? chromeArg : undefined,
    redactNetworkHeaders: true,
    categoryNavigationAutomation: true,
    categoryDebugging: true,
    // Drops `emulate` (CPU/network/device emulation) + `resize_page`.
    // Viewport is fixed at sidecar boot via `BAYBO_BROWSER_VIEWPORT`, and
    // the schema for `emulate` alone is ~1.2 KB of enum metadata that
    // the agent rarely needs.
    categoryEmulation: false,
    // Performance/lighthouse tracing is niche enough that we'd rather not
    // tempt the agent into multi-second trace recordings on every site
    // visit. Operators who want the perf tooling can bring up DevTools
    // by hand or run chrome-devtools-mcp standalone.
    categoryPerformance: false,
    categoryNetwork: true,
    categoryExtensions: false,
    slim: false,
    // Adds an optional `pageId` parameter to every page-scoped CDDM
    // tool. When the agent passes it, the call routes to that exact
    // page instead of CDDM's process-global "selected page" — which
    // is the only thing that lets multiple Baybo agents share one
    // browser sidecar without their navigations / clicks / snapshots
    // tripping over each other (a select_page+act sequence is no
    // longer racy because CDDM's own toolMutex serialises every call,
    // but routing per-call by id removes the need for select_page in
    // the first place).
    //
    // Caveat — CDDM 0.23.0 inconsistency: page-scoped tools fall back
    // to the selected page when `pageId` is omitted (build/src/index.js
    // pre-resolves with `params.pageId && getPageById(...) :
    // getSelectedMcpPage()`), but `evaluate_script` is registered as
    // non-page-scoped and re-resolves inside its own handler without
    // that fallback (build/src/tools/script.js: `experimentalPageIdRouting
    // ? getPageById(pageId) : getSelectedMcpPage()` — `getPageById(undefined)`
    // throws "No page found"). So once routing is on, **`evaluate_script`
    // requires `pageId`**. Our injected `browser/read_page` tool
    // surfaces this in its schema + threads `pageId` through to the
    // underlying `evaluate_script` call.
    //
    // For per-agent cookie/storage isolation (not just per-call page
    // targeting), agents should additionally pass `isolatedContext:
    // "<agent-id>"` to `new_page` — CDDM creates a Puppeteer
    // BrowserContext per name, giving each agent its own cookie jar.
    experimentalPageIdRouting: true,
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
        const buildSuffix = this.state.buildId !== undefined
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
      case "recovering":
        // Written by the watchdog via `BrowserHealer.outageText` — the honest
        // wording differs per mode, so it is not synthesised here.
        text = this.state.error ?? "Browser is temporarily unavailable. Please retry shortly.";
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
          `Check the gateway logs (baybo-browser-mcp lines), then either fix the underlying issue ` +
          `(network for auto-install, docker daemon for docker mode) and restart the gateway, or ` +
          `set baybo.json:browser.chrome_path to an existing Chrome binary as a workaround.`;
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
  const profileDir = resolvedProfileDir();
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

  if (initialPath !== null) {
    state.phase = "ready";
  } else if (state.phase !== "failed") {
    state.phase = "installing";
  }

  const args = buildArgs(initialPath ?? undefined);

  if (state.phase === "installing") {
    void installChromeWithRetry(state, args);
  }
  return args;
}

/**
 * Chrome-download attempts before the sidecar declares the install failed.
 *
 * A single attempt used to be the whole policy: a boot that landed in a
 * network blip set `phase = "failed"` and nothing ever retried, so
 * `browser/*` stayed dead until the operator noticed and restarted the
 * gateway. `failed` is also the one phase the watchdog can't help with —
 * there is no browser to probe — so the retry has to live here.
 */
const CHROME_INSTALL_ATTEMPTS = 4;

/** Linear backoff base between install attempts (15s, 30s, 45s). */
const CHROME_INSTALL_RETRY_BASE_MS = 15_000;

async function installChromeWithRetry(state: InstallState, args: ServerArgs): Promise<void> {
  for (let attempt = 1; attempt <= CHROME_INSTALL_ATTEMPTS; attempt += 1) {
    try {
      const exe = await installChromeInBackground(state);
      args.executablePath = exe;
      state.phase = "ready";
      state.percent = 100;
      state.error = undefined;
      log("Chrome ready; browser tools are live");
      return;
    } catch (e) {
      const reason = errText(e);
      if (attempt === CHROME_INSTALL_ATTEMPTS) {
        state.phase = "failed";
        state.error = reason;
        logger.error(`Chrome install failed after ${attempt} attempts: ${reason}`);
        return;
      }
      const delayMs = CHROME_INSTALL_RETRY_BASE_MS * attempt;
      state.percent = 0;
      logger.warn(
        `Chrome install attempt ${attempt}/${CHROME_INSTALL_ATTEMPTS} failed (${reason}); ` +
          `retrying in ${Math.round(delayMs / 1000)}s`,
      );
      await sleep(delayMs);
    }
  }
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
 * Within an Baybo process, host-headless and docker modes are mutually
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
  // Only SingletonLock carries Chrome's `hostname-pid` symlink target.
  // SingletonCookie (a random cookie id) and SingletonSocket (a socket
  // path) are non-`hostname-pid` by design, so we process the lock first,
  // remember whether we cleared it, and treat the other two as its stale
  // companions.
  let lockCleared = false;
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
      // A non-`hostname-pid` target is the steady state for
      // SingletonCookie/SingletonSocket — warning "unexpected target" on
      // it fired a false alarm on every boot. Clear them only as stale
      // companions of a cleared lock; otherwise leave them silently. On
      // SingletonLock a non-pid target really is unexpected, so keep the
      // line there.
      if (name === "SingletonLock") {
        log(`Chrome lock ${lockPath} has unexpected target '${target}'; leaving alone`);
      } else if (lockCleared) {
        removeChromeLock(lockPath);
      }
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
    if (removeChromeLock(lockPath) && name === "SingletonLock") {
      lockCleared = true;
    }
  }
}

const CHROME_PROCESS_NAME_RE = /chrom(e|ium)/i;

/**
 * Positively identify a pid as a Chrome-family process.
 *
 * Deliberately the mirror image of {@link isLikelyChromeProcess}, which
 * answers "should we *respect* this lock" and therefore assumes Chrome when it
 * can't tell. Killing needs the opposite default: an unverifiable pid must
 * never be signalled, or a stale lock whose pid was recycled onto an unrelated
 * process would get that process killed. macOS has no `/proc` at all, so
 * inheriting the permissive default there would make the kill a coin flip —
 * hence the `ps` fallback rather than a bare `catch { return true }`.
 */
async function isConfirmedChromeProcess(pid: number): Promise<boolean> {
  try {
    return CHROME_PROCESS_NAME_RE.test(readFileSync(`/proc/${pid}/comm`, "utf8").trim());
  } catch {
    // No /proc (macOS) or the process vanished — ask ps before giving up.
  }
  try {
    const { stdout } = await execFileAsync("ps", ["-o", "comm=", "-p", String(pid)], {
      timeout: 2_000,
    });
    // macOS ps prints the full executable path; Linux prints the bare name.
    return CHROME_PROCESS_NAME_RE.test(basename(stdout.trim()));
  } catch {
    return false;
  }
}

/**
 * PID of the Chrome currently holding this profile, when it is alive, on this
 * host, and confirmed to be Chrome.
 *
 * `SingletonLock` is a symlink to `<hostname>-<pid>`, and that target is the
 * only handle we have on a Chrome that CDDM launched: it launches with
 * `pipe: true`, so there is no `DevToolsActivePort` file and no CDP port to
 * find the process by.
 */
async function lockedChromePid(profileDir: string): Promise<number | undefined> {
  let target: string;
  try {
    target = readlinkSync(join(profileDir, "SingletonLock"));
  } catch {
    return undefined;
  }
  const m = target.match(/^(.+)-(\d+)$/);
  if (!m) return undefined;
  if (m[1] !== osHostname()) return undefined;
  const pid = Number(m[2]);
  if (!Number.isInteger(pid) || pid <= 0) return undefined;
  if (!isPidAlive(pid)) return undefined;
  return (await isConfirmedChromeProcess(pid)) ? pid : undefined;
}

/** Poll a pid out of existence, so the lock clear that follows sees it gone. */
async function waitForPidToExit(pid: number, budgetMs: number): Promise<boolean> {
  const deadline = Date.now() + budgetMs;
  while (Date.now() < deadline) {
    if (!isPidAlive(pid)) return true;
    await sleep(100);
  }
  return !isPidAlive(pid);
}

/** How long to wait for a SIGKILLed Chrome to leave the process table. */
const CHROME_KILL_GRACE_MS = 3_000;

function removeChromeLock(lockPath: string): boolean {
  try {
    unlinkSync(lockPath);
    return true;
  } catch (e) {
    log(`failed to remove ${lockPath}: ${(e as Error).message}`);
    return false;
  }
}

function dockerDirPath(): string {
  // Bundle materialises to $XDG_CACHE_HOME/baybo/sidecars/browser-<hash>/bundle.mjs;
  // the `docker/` aux asset (declared in package.json:baybo.auxAssets)
  // lands as a sibling under the same hash dir.
  return resolve(dirname(fileURLToPath(import.meta.url)), "docker");
}

function pickFirstExistingDir(raw: string | undefined): string | undefined {
  if (raw === undefined) return undefined;
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
  /** Retained so the watchdog can respawn an identical container. */
  opts: DockerSpawnOptions;
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
  logDebug(`docker daemon reachable (server ${check.serverVersion}); sweeping stale containers`);
  // Operator-set browser.sandbox=true doesn't reach Chrome in docker mode:
  // the container's entrypoint hardcodes --no-sandbox because the slim
  // base ships no SUID chrome-sandbox helper. Warn loudly so the operator
  // doesn't think the sandbox is on when it isn't.
  if (process.env["BAYBO_BROWSER_NO_SANDBOX"] !== "1") {
    log(
      "browser.sandbox=true ignored in docker mode — the container is the trust boundary " +
        "and Chrome runs with --no-sandbox inside (see CLAUDE.md 'Docker mode' subsection)",
    );
  }
  await sweepStaleContainers();

  // Docker mode uses consumer Google Chrome installed inside the image
  // via apt (NOT @puppeteer/browsers / Chrome for Testing). Image is
  // pinned by `baybo-browser:<sha256(Dockerfile + entrypoint.sh)[..12]>`
  // and gets whatever Chrome stable was current at build time. No host-
  // side Chrome version lookup needed — the only network call is the
  // initial `wget` inside the Dockerfile, which docker handles.
  const operatorImageTag = envValue("BAYBO_BROWSER_DOCKER_IMAGE_TAG");

  const profileDir = resolvedProfileDir();
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

  const opts: DockerSpawnOptions = {
    dockerDir: dockerDirPath(),
    imageTag: operatorImageTag,
    profileDir,
    fontDir: pickFirstExistingDir(envValue("BAYBO_BROWSER_EXTRA_FONT_DIRS")),
    webVncPort: parseVncPort(envValue("BAYBO_BROWSER_DOCKER_WEB_VNC_PORT"), "WEB_VNC_PORT"),
    viewport: parseViewport(envValue("BAYBO_BROWSER_VIEWPORT")) ?? { width: 1920, height: 1080 },
    onPhase: (p) => {
      state.phase = p;
    },
  };
  const handle = await spawnContainer(opts);

  // CDDM ignores `headless` in connect-mode; set false anyway so the
  // boot summary reads correctly and the operator's mental model
  // matches reality.
  const args = buildArgs(undefined);
  args.browserUrl = handle.cdpUrl;
  args.headless = false;
  return { args, handle, opts };
}

// ---------------------------------------------------------------------
// Watchdog wiring
// ---------------------------------------------------------------------

/**
 * The cheapest chrome-devtools-mcp call that actually touches the browser.
 * It resolves CDDM's `getContext()` — which is where a crashed Chrome gets
 * relaunched (`ensureBrowserLaunched` re-launches whenever
 * `browser.connected` is false) and a dropped CDP session gets reconnected —
 * then returns the page list without navigating anything.
 *
 * CDDM never throws out of a tool handler; a failure comes back as
 * `isError` with the message in a text block, so it has to be re-thrown to
 * read as a failed probe.
 */
const CDDM_LIST_PAGES_TOOL = "list_pages";

/** Ceiling on the end-to-end call each `recover()` uses to prove itself. */
const RECOVERY_CALL_TIMEOUT_MS = 30_000;

/** Ceiling on container cleanup when the watchdog gives up and exits. */
const ESCALATION_CLEANUP_TIMEOUT_MS = 15_000;

function firstText(content: unknown): string {
  if (!Array.isArray(content)) return "";
  for (const block of content as unknown[]) {
    if (typeof block !== "object" || block === null) continue;
    const { type, text } = block as { type?: unknown; text?: unknown };
    if (type !== "text" || typeof text !== "string") continue;
    const trimmed = text.trim();
    if (trimmed.length > 0) return trimmed;
  }
  return "";
}

async function assertBrowserAnswers(client: Client, signal?: AbortSignal): Promise<void> {
  const res = await client.callTool(
    { name: CDDM_LIST_PAGES_TOOL, arguments: {} },
    undefined,
    signal !== undefined ? { signal } : undefined,
  );
  if (res.isError === true) {
    const detail = firstText(res.content);
    throw new Error(
      detail.length > 0 ? detail : `${CDDM_LIST_PAGES_TOOL} reported an error`,
    );
  }
}

function hostHealer(cddmClient: Client, activity: BrowserActivity): BrowserHealer {
  return {
    mode: "host",
    canEscalate: true,
    // CDDM launches Chrome lazily on the first tool call, so before then
    // there is nothing to watch — and probing would force a Chrome the
    // operator may never use into memory on every boot.
    armed: () => activity.everCalled,
    check: (signal) => assertBrowserAnswers(cddmClient, signal),
    outageText: (reason) =>
      `Browser stopped responding (${reason}) and is being restarted automatically. Any pages ` +
      `that were open are gone — re-navigate once it is back. Please retry this tool call in a ` +
      `few seconds.`,
    async recover() {
      const profileDir = resolvedProfileDir();

      // A Chrome that *hung* is worse than one that died: puppeteer keeps
      // reporting `browser.connected === true`, so CDDM's own relaunch never
      // fires (`ensureBrowserLaunched` returns early on that flag) and every
      // tool call parks on the wedged process indefinitely. Killing it is
      // what turns the hang into the crash CDDM already knows how to recover
      // from. We only get here after consecutive probes went unanswered on an
      // otherwise idle browser, so there is no live work to lose.
      const hung = await lockedChromePid(profileDir);
      if (hung !== undefined) {
        log(`killing unresponsive Chrome (pid=${hung}) holding ${profileDir}`);
        try {
          process.kill(hung, "SIGKILL");
          if (!(await waitForPidToExit(hung, CHROME_KILL_GRACE_MS))) {
            // Still in the process table (typically not yet reaped). The lock
            // clear below leaves a live-looking pid's lock alone, so this
            // attempt will fail and the next one — by which point it is gone —
            // will succeed.
            log(`Chrome pid=${hung} still present after SIGKILL; retrying on the next tick`);
          }
        } catch (e) {
          log(`could not kill Chrome pid=${hung}: ${errText(e)}`);
        }
      }

      // Whether Chrome died on its own or we just killed it, the `Singleton*`
      // lock it left behind is what stops the relaunch: Chrome refuses to
      // start on a locked profile and CDDM surfaces that as "The browser is
      // already running for <dir>". The boot path clears these once; nothing
      // did so after a mid-run crash until now.
      clearStaleChromeLocks(profileDir);

      // Performs the relaunch and proves it took.
      await assertBrowserAnswers(cddmClient, AbortSignal.timeout(RECOVERY_CALL_TIMEOUT_MS));
    },
  };
}

function dockerHealer(
  cddmClient: Client,
  args: ServerArgs,
  opts: DockerSpawnOptions,
  getHandle: () => DockerHandle,
  setHandle: (h: DockerHandle) => void,
): BrowserHealer {
  // The boot `opts` reports progress by assigning `state.phase`, which is
  // right at boot and wrong here: it would overwrite the watchdog's
  // `recovering` phase with `docker-building-image`, telling the agent its
  // browser is in "first-time setup, takes 1-3 minutes" when what actually
  // happened is a crash mid-session. Recovery reports to the log instead.
  const recoveryOpts: DockerSpawnOptions = {
    ...opts,
    onPhase: (p) => logDebug(`recovery: ${p}`),
  };
  return {
    mode: "docker",
    canEscalate: true,
    armed: () => true,
    check: (signal) => probeCdpEndpoint(getHandle().cdpUrl, signal),
    outageText: (reason) =>
      `Browser container stopped responding (${reason}) and is being replaced automatically. Any ` +
      `pages that were open are gone — re-navigate once it is back. Please retry this tool call ` +
      `in a few seconds.`,
    async recover() {
      const dead = getHandle();
      log(`replacing unresponsive container ${dead.containerName}`);
      await dead.stop();
      await sweepStaleContainers();
      const next = await spawnContainer(recoveryOpts);
      setHandle(next);
      // CDDM re-reads `serverArgs.browserUrl` inside getContext() on every
      // tool call, so repointing it here is all it takes for the next
      // connection to find the replacement container — whose published port
      // is a fresh ephemeral one, hence the mutation rather than a reuse.
      //
      // This only lands because the old connection is already gone. CDDM
      // caches the browser in a module-level singleton guarded on
      // `browser?.connected` alone: on a cache hit it returns the cached
      // instance and discards every argument, including a changed
      // browserUrl. Force-removing the container above is what drops that
      // connection and makes the next `ensureBrowserConnected` honour the new
      // URL. Two corollaries for anyone editing this: never try to switch
      // *modes* (launched ↔ connected) at runtime — the guard ignores mode
      // entirely and would hand back the wrong browser — and don't reach for
      // CDDM's `closeBrowser` to force the issue: it isn't exported from the
      // package entry, and a deep import would be inlined by esbuild as a
      // second module instance with its own singleton, silently doing nothing.
      args.browserUrl = next.cdpUrl;
      log(`container replaced: ${next.containerName} at ${next.cdpUrl}`);
      // A container whose CDP port answers can still be unreachable through
      // the puppeteer connection CDDM has cached, so prove the whole path
      // rather than trusting the port probe.
      await assertBrowserAnswers(cddmClient, AbortSignal.timeout(RECOVERY_CALL_TIMEOUT_MS));
    },
  };
}

function cdpUrlHealer(cdpUrl: string): BrowserHealer {
  return {
    mode: "cdp_url",
    // We don't own this Chrome — `browser.docker.cdp_url` is the operator's
    // "I'm managing the browser" escape hatch. Exiting would crash-loop
    // against the gateway's reconciler while fixing nothing, and CDDM
    // reconnects on its own once the operator's Chrome is back. So: report,
    // keep probing, never restart.
    canEscalate: false,
    armed: () => true,
    check: (signal) => probeCdpEndpoint(cdpUrl, signal),
    // Deliberately does not promise a restart: nothing here can deliver one.
    outageText: (reason) =>
      `The externally-managed Chrome at ${cdpUrl} is not answering (${reason}). Baybo does not ` +
      `manage that browser (it is configured via browser.docker.cdp_url), so it cannot restart ` +
      `it — whoever runs it has to bring it back. Baybo reconnects automatically once it does.`,
    recover: () => Promise.resolve(),
  };
}

function parseVncPort(raw: string | undefined, label: string): number | undefined {
  if (raw === undefined) return undefined;
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 1 || n > 65535) {
    log(`BAYBO_BROWSER_DOCKER_${label} '${raw}' is not a valid TCP port; ignoring`);
    return undefined;
  }
  return n;
}

async function main(): Promise<void> {
  const state: InstallState = { phase: "installing", percent: 0 };
  const dockerCdpUrl = envValue("BAYBO_BROWSER_DOCKER_CDP_URL");
  const dockerEnable = process.env["BAYBO_BROWSER_DOCKER_ENABLE"] === "1";
  let dockerHandle: DockerHandle | null = null;
  let dockerOpts: DockerSpawnOptions | null = null;
  let args: ServerArgs;
  let mode: "cdp_url" | "docker" | "host";

  if (dockerCdpUrl !== undefined) {
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
      dockerOpts = outcome.opts;
      state.phase = "ready";
      mode = "docker";
      // The container name + image tag are folded into the single
      // "chrome-devtools-mcp ready" summary below (CDP url already rides
      // args.browserUrl there), so no separate per-boot ready line here.
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
    { name: "baybo-browser-proxy", version: "1.0.0" },
    { capabilities: {} },
  );
  await cddmClient.connect(cddmOuter);

  const proxy = new Server(
    { name: "baybo-browser", version: "1.0.0" },
    { capabilities: { tools: {} } },
  );
  // Assigned below; the handler closes over the binding so the watchdog can be
  // built after the client it probes through.
  let watchdog: BrowserWatchdog | null = null;

  proxy.setRequestHandler(ListToolsRequestSchema, async () => {
    // The gateway's reconciler probes with exactly this request, and would
    // otherwise get a healthy answer from the in-process registry no matter
    // what state the watchdog is in — so a watchdog that stopped ticking is
    // invisible, while the phase it left behind tells the agent to keep
    // retrying. Failing here converts that into the one failure the reconciler
    // already handles: disconnect, then respawn onto a clean boot.
    if (watchdog?.isStalled() === true) {
      throw new Error(
        "browser watchdog has stopped ticking; the sidecar cannot vouch for the browser. " +
          "Respawning is the recovery — see the baybo-browser-mcp lines in the gateway log.",
      );
    }
    const cddmTools = await cddmClient.listTools();
    const visible = cddmTools.tools.filter((t) => !BLOCKED_TOOLS.has(t.name));
    return { tools: [...visible, READ_PAGE_TOOL] };
  });
  // Every call that reaches CDDM is recorded, so the watchdog can stand down
  // while the agent is actively browsing: real traffic proves the browser
  // answers better than any probe, and CDDM serialises tool calls behind one
  // mutex, so a probe fired mid-call would queue and then time out against a
  // perfectly healthy browser. Rejections that never touch the browser
  // (unknown page id, blocked tool) deliberately don't count.
  const activity = new BrowserActivity();
  const throughCddm = async <T>(run: () => Promise<T>): Promise<T> => {
    activity.begin();
    let ok = false;
    try {
      const result = await run();
      // CDDM never throws out of a tool handler — a dead browser comes back
      // as `isError` with the message in a text block — so the flag, not the
      // absence of a throw, is what says the browser actually answered.
      ok = (result as { isError?: unknown }).isError !== true;
      return result;
    } finally {
      activity.end(ok);
    }
  };

  proxy.setRequestHandler(CallToolRequestSchema, async (req) => {
    if (req.params.name === READ_PAGE_TOOL.name) {
      const raw = req.params.arguments ?? {};
      const pageIdRaw = (raw as { pageId?: unknown }).pageId;
      if (typeof pageIdRaw !== "number") {
        return proxyToolError(
          "read_page requires a numeric `pageId`. CDDM's evaluate_script " +
            "handler does not fall back to the selected page; call list_pages " +
            "to find the page id and pass it in.",
        );
      }
      return await throughCddm(() => handleReadPage(cddmClient, { pageId: pageIdRaw }));
    }
    if (BLOCKED_TOOLS.has(req.params.name)) {
      return proxyToolError(
        `Tool '${req.params.name}' is disabled in this Baybo build. ` +
          `It was hidden from tools/list — do not invoke it.`,
      );
    }
    return await throughCddm(() =>
      cddmClient.callTool({
        name: req.params.name,
        arguments: req.params.arguments,
      }),
    );
  });

  const realTransport = new StdioServerTransport();
  const transport = new GuardingTransport(realTransport, state);
  await proxy.connect(transport as unknown as StdioServerTransport);

  const chromeArgList = args.chromeArg !== undefined && args.chromeArg.length > 0 ? args.chromeArg.join(",") : "";
  const viewportStr = args.viewport !== undefined ? `${args.viewport.width}x${args.viewport.height}` : "<chrome default>";
  const target = args.browserUrl !== undefined
    ? `browserUrl=${args.browserUrl}`
    : `executable=${args.executablePath ?? "<pending install>"}`;
  const dockerStr = dockerHandle
    ? ` container=${dockerHandle.containerName} image=${dockerHandle.imageTag}`
    : "";
  log(
    `chrome-devtools-mcp ready: mode=${mode}${dockerStr} userDataDir=${args.userDataDir} ${target} ` +
      `viewport=${viewportStr} headless=${args.headless} ` +
      `sandbox=${chromeArgList.includes("--no-sandbox") ? "off" : "on"} ` +
      `telemetry=off page_id_routing=${args.experimentalPageIdRouting === true ? "on" : "off"} ` +
      `install_state=${state.phase} extra_tools=read_page ` +
      `disabled_categories=emulation,performance,extensions disabled_tools=${[...BLOCKED_TOOLS].join(",")}`,
  );

  // Browser liveness watchdog. The gateway's McpReconciler already respawns
  // this process when its stdio pipe breaks, but a dead Chrome behind a live
  // sidecar is invisible to that probe — see watchdog.ts for the split.
  const phaseGate: PhaseGate = {
    isReady: () => state.phase === "ready",
    enterRecovering: (reason) => {
      state.phase = "recovering";
      state.error = reason;
    },
    markReady: () => {
      state.phase = "ready";
      state.error = undefined;
    },
  };

  let shuttingDown = false;
  let healer: BrowserHealer;
  if (mode === "docker" && dockerOpts) {
    const opts = dockerOpts;
    healer = dockerHealer(
      cddmClient,
      args,
      opts,
      () => {
        if (!dockerHandle) throw new Error("docker mode lost its container handle");
        return dockerHandle;
      },
      (next) => {
        // A replacement that lands after teardown began would otherwise be
        // orphaned: shutdown has already snapshotted the old handle, and the
        // next-boot sweep skips containers whose owner pid is still alive.
        if (shuttingDown) {
          log(`discarding container ${next.containerName} spawned during shutdown`);
          void next.stop();
          return;
        }
        dockerHandle = next;
      },
    );
  } else if (mode === "cdp_url" && dockerCdpUrl !== undefined) {
    healer = cdpUrlHealer(dockerCdpUrl);
  } else {
    healer = hostHealer(cddmClient, activity);
  }

  watchdog = new BrowserWatchdog({
    healer,
    activity,
    phase: phaseGate,
    log: logger,
    giveUp: () => {
      watchdog?.stop();
      // Take the container down on the way out; otherwise every escalation
      // leaks one until the next boot's sweep notices. Bounded, because
      // exiting is the point — a wedged docker daemon must not prevent it.
      const cleanup = dockerHandle ? dockerHandle.stop() : Promise.resolve();
      void Promise.race([cleanup, sleep(ESCALATION_CLEANUP_TIMEOUT_MS)]).then(() => {
        process.exit(1);
      });
    },
  });
  watchdog.start();

  // Cap shutdown at the docker stop timeout (5s graceful + slack for
  // `docker rm`) so a hung container can't pin the gateway's reload
  // forever, but give the in-flight `docker stop` a real chance to
  // complete — the previous 50ms hard cutoff routinely exited before
  // `docker stop` even sent SIGTERM, leaving the container running and
  // the next-boot sweep responsible for cleanup.
  const SHUTDOWN_TIMEOUT_MS = 25_000;
  const shutdown = (): void => {
    if (shuttingDown) return;
    shuttingDown = true;
    // Stop scheduling first, then wait out any tick already running: a docker
    // repair can be mid-`spawnContainer` here, and exiting under it would
    // orphan the container it is about to create. `quiesce` rides the same
    // 25s deadline as the rest of teardown.
    watchdog?.stop();
    log("shutting down");
    const closes: Array<Promise<unknown>> = [
      watchdog?.quiesce() ?? Promise.resolve(),
      proxy.close().catch(() => undefined),
      cddmClient.close().catch(() => undefined),
      cddmServer.close().catch(() => undefined),
    ];
    if (dockerHandle) {
      // dockerHandle.stop already swallows its own errors.
      closes.push(dockerHandle.stop());
    }
    const deadline = new Promise<void>((resolve) => {
      setTimeout(() => {
        log(`shutdown deadline hit after ${SHUTDOWN_TIMEOUT_MS}ms; forcing exit`);
        resolve();
      }, SHUTDOWN_TIMEOUT_MS).unref();
    });
    void Promise.race([Promise.allSettled(closes).then(() => undefined), deadline]).then(() => {
      process.exit(0);
    });
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

main().catch((e: unknown) => {
  logger.error(`fatal: ${errText(e)}`);
  process.exit(1);
});
