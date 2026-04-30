import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { resolve as resolvePath, sep } from "node:path";

import {
  type Browser,
  type BrowserContext,
  type Dialog,
  type LaunchOptions,
  type Page,
  chromium,
} from "playwright";

import { resolveAndCheck } from "./network_policy.js";
import { Supervisor, type SupervisorSnapshot } from "./supervisor.js";

const IDLE_TIMEOUT_MS = 10 * 60 * 1000;
const IDLE_SCAN_MS = 60 * 1000;
const MAX_CONTEXTS = 20;

/**
 * Profile dir whitelist — the agent's browser data MUST live under
 * Aura's cache, never under the user's regular browser profile. We
 * refuse to start if `userDataDir` resolves under any of these
 * platform default profile roots, even if the operator points
 * `AURA_BROWSER_PROFILE_DIR` there.
 */
const FORBIDDEN_PROFILE_PREFIXES = [
  // Linux Chrome / Chromium
  ".config/google-chrome",
  ".config/chromium",
  // macOS Chrome / Chromium
  "Library/Application Support/Google/Chrome",
  "Library/Application Support/Chromium",
  // Firefox (any platform)
  ".mozilla/firefox",
  "Library/Application Support/Firefox",
];

export interface ManagerOptions {
  /** Resolved profile directory; created lazily on first launch. */
  profileDir: string;
  /** Optional override for the chromium binary; falls back to Playwright's bundled one. */
  chromiumExecutable?: string | undefined;
  /**
   * When true, launch Chromium with `--no-sandbox`. Default false:
   * Chromium's user-namespace sandbox is the floor between page
   * code and the host. Disable only when the operator already runs
   * the sidecar inside an outer sandbox (rootless docker, gVisor,
   * etc.) — the gateway logs a warn line on every launch in that
   * mode.
   */
  noSandbox?: boolean | undefined;
  /**
   * Test-only: allow navigations / subresources to 127.0.0.0/8 and
   * ::1. Mirrors `aura_security::is_blocked_ip(allow_loopback)` so
   * smoke tests can bind a local HTTP server and drive a real
   * Chromium against it. Production callers MUST leave this off —
   * the supervisor reads `AURA_BROWSER_ALLOW_LOOPBACK` env, which
   * is documented as test-only and never set in normal startup.
   */
  allowLoopback?: boolean | undefined;
  emitEvent: (name: string, data: unknown) => void;
}

export interface SessionState {
  ctx: BrowserContext;
  page: Page;
  supervisor: Supervisor;
  lastUsedAt: number;
  url: string;
  /**
   * Nonce-suffixed attribute name used to label interactive elements
   * for the agent's `@eN` refs (e.g. `data-aura-ref-7f3a8b9c2d1e4506`).
   * Generated per context — a malicious page can't statically
   * pre-pollute the attribute because they can't predict the suffix.
   * The pre-walk sweep + locator uniqueness check defeat the dynamic
   * copy-attribute attack at click time.
   */
  refAttr: string;
}

/**
 * Owns one Playwright `Browser` instance per sidecar process and lazily
 * creates per-Aura-session `BrowserContext`s keyed by `context_id`.
 * Idle GC reaps contexts that haven't been used for 10 min.
 */
export class BrowserManager {
  private browser: Browser | null = null;
  private readonly contexts = new Map<string, SessionState>();
  private gcTimer: NodeJS.Timeout | null = null;

  constructor(private readonly opts: ManagerOptions) {
    this.assertSafeProfileDir(opts.profileDir);
  }

  /**
   * Whether navigations / subresources to loopback addresses are
   * allowed. Test-only escape hatch — production stays false.
   */
  allowLoopback(): boolean {
    return Boolean(this.opts.allowLoopback);
  }

  /** Lazily start chromium + start the idle scanner. */
  private async ensureBrowser(): Promise<Browser> {
    if (this.browser !== null) return this.browser;
    mkdirSync(this.opts.profileDir, { recursive: true });
    // Chromium's user-namespace sandbox is the floor between
    // attacker-controlled page code and the gateway user. We keep
    // it ON by default and only honour `--no-sandbox` when the
    // operator explicitly opts in (for rootless-docker / gVisor /
    // CI environments where the outer container already contains
    // the renderer).
    const args = ["--disable-gpu"];
    if (this.opts.noSandbox) {
      args.push("--no-sandbox");
      process.stderr.write(
        "[browser] WARNING: --no-sandbox enabled (AURA_BROWSER_NO_SANDBOX=1). " +
          "Renderer escapes have access to the gateway user — only run with " +
          "an outer sandbox (rootless docker / gVisor / etc.).\n",
      );
    }
    const launchOptions: LaunchOptions = { headless: true, args };
    if (this.opts.chromiumExecutable) {
      launchOptions.executablePath = this.opts.chromiumExecutable;
    }
    this.browser = await chromium.launch(launchOptions);
    this.startGc();
    return this.browser;
  }

  /** Acquire (or create) the per-session state. */
  async acquire(contextId: string): Promise<SessionState> {
    const existing = this.contexts.get(contextId);
    if (existing) {
      existing.lastUsedAt = Date.now();
      return existing;
    }
    if (this.contexts.size >= MAX_CONTEXTS) {
      const err = new Error(
        `browser sidecar: max ${MAX_CONTEXTS} contexts reached (idle GC will free some shortly)`,
      );
      err.name = "TOO_MANY_CONTEXTS";
      throw err;
    }
    const browser = await this.ensureBrowser();
    const ctx = await browser.newContext({
      // Per-Aura-session isolation: separate cookies, localStorage,
      // IndexedDB, service workers. Playwright defaults to a fresh
      // ephemeral storage state per context, which is exactly what
      // we want.
      viewport: { width: 1280, height: 800 },
    });
    // SSRF floor: every navigation, redirect, and page-initiated
    // subresource request goes through `resolveAndCheck`. Blocked
    // hosts are aborted at the network layer before Chromium opens
    // a socket. Non-http(s) schemes that bypass page.route (data:,
    // javascript:) are blocked at the pre-navigation gate in
    // handlers.ts::navigate.
    const allowLoopback = this.allowLoopback();
    await ctx.route("**/*", async (route, request) => {
      const url = request.url();
      let parsed: URL;
      try {
        parsed = new URL(url);
      } catch {
        await route.abort("addressunreachable");
        return;
      }
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
        await route.abort("addressunreachable");
        return;
      }
      try {
        await resolveAndCheck(parsed.hostname, allowLoopback);
      } catch {
        // No-log, no-detail abort — the agent already saw the
        // pre-nav rejection or the snapshot; per-subresource we
        // don't want a noisy log line per blocked image.
        await route.abort("addressunreachable");
        return;
      }
      await route.continue();
    });
    const page = await ctx.newPage();
    const supervisor = new Supervisor();
    supervisor.attach(page);
    page.on("dialog", (dialog: Dialog) => {
      void supervisor.onDialog(dialog).catch((e: unknown) => {
        const msg = e instanceof Error ? e.message : String(e);
        // Defensive: a dialog handler crash must not nuke the process.
        process.stderr.write(`supervisor onDialog failed: ${msg}\n`);
      });
    });
    const refAttr = `data-aura-ref-${randomBytes(8).toString("hex")}`;
    const state: SessionState = {
      ctx,
      page,
      supervisor,
      lastUsedAt: Date.now(),
      url: "",
      refAttr,
    };
    this.contexts.set(contextId, state);
    return state;
  }

  /** Drop a context (idle GC or explicit close). */
  async release(contextId: string): Promise<void> {
    const state = this.contexts.get(contextId);
    if (!state) return;
    this.contexts.delete(contextId);
    try {
      await state.ctx.close();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      process.stderr.write(`browser ctx close failed: ${msg}\n`);
    }
    this.opts.emitEvent("context_closed", { context_id: contextId });
  }

  /** Snapshot of the supervisor state — used by `browser_snapshot`. */
  supervisorSnapshot(contextId: string): SupervisorSnapshot | null {
    const s = this.contexts.get(contextId);
    return s ? s.supervisor.snapshot() : null;
  }

  async shutdown(): Promise<void> {
    if (this.gcTimer) {
      clearInterval(this.gcTimer);
      this.gcTimer = null;
    }
    for (const id of [...this.contexts.keys()]) {
      await this.release(id);
    }
    if (this.browser) {
      try {
        await this.browser.close();
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        process.stderr.write(`browser close failed: ${msg}\n`);
      }
      this.browser = null;
    }
  }

  private startGc(): void {
    if (this.gcTimer) return;
    this.gcTimer = setInterval(() => {
      const now = Date.now();
      for (const [id, state] of this.contexts.entries()) {
        if (now - state.lastUsedAt > IDLE_TIMEOUT_MS) {
          void this.release(id);
        }
      }
    }, IDLE_SCAN_MS);
    this.gcTimer.unref();
  }

  /**
   * Refuse to start if the operator points the agent's browser at a
   * platform default profile (Chrome / Chromium / Firefox). The
   * isolation guarantee is non-negotiable: cookies and credentials in
   * the user's normal browser must NEVER bleed into the agent's
   * browsing.
   */
  private assertSafeProfileDir(dir: string): void {
    const home = homedir();
    const resolved = resolvePath(dir);
    const homeRel = resolved.startsWith(home + sep)
      ? resolved.slice(home.length + 1)
      : null;
    if (homeRel) {
      for (const forbidden of FORBIDDEN_PROFILE_PREFIXES) {
        if (homeRel === forbidden || homeRel.startsWith(forbidden + sep)) {
          throw new Error(
            `browser sidecar refuses to use system browser profile dir '${resolved}'; ` +
              `set AURA_BROWSER_PROFILE_DIR to a path under \$XDG_CACHE_HOME/aura/browser/...`,
          );
        }
      }
    }
  }
}
