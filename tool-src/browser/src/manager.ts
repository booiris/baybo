import { randomBytes } from "node:crypto";
import { mkdirSync, realpathSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, resolve as resolvePath, sep } from "node:path";

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
  // Linux Brave / Edge / Vivaldi / Opera
  ".config/BraveSoftware",
  ".config/microsoft-edge",
  ".config/vivaldi",
  ".config/opera",
  // Snap / Flatpak Chromium variants
  "snap/chromium/common/chromium",
  "snap/firefox/common/.mozilla/firefox",
  ".var/app/org.chromium.Chromium",
  ".var/app/com.google.Chrome",
  ".var/app/com.brave.Browser",
  ".var/app/com.microsoft.Edge",
  ".var/app/org.mozilla.firefox",
  // macOS Chrome / Chromium / Brave / Edge / Vivaldi / Opera
  "Library/Application Support/Google/Chrome",
  "Library/Application Support/Chromium",
  "Library/Application Support/BraveSoftware",
  "Library/Application Support/Microsoft Edge",
  "Library/Application Support/Vivaldi",
  "Library/Application Support/com.operasoftware.Opera",
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
   * Run Chromium in headless mode. Default true. Flip to false for
   * local debugging when you want to see the agent's browsing —
   * only useful with a desktop session ($DISPLAY / Wayland);
   * headed mode in a server / CI env fails with `Missing X server`.
   */
  headless?: boolean | undefined;
  /**
   * Extra Chromium command-line arguments, appended verbatim to the
   * launch flags. Each entry must include the leading `-` / `--`
   * (e.g. `"--lang=en-US"`). Empty by default.
   */
  extraArgs?: readonly string[] | undefined;
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
    // Operator-supplied extras go LAST so they can override any
    // defaults the gateway inserted above (e.g. an explicit
    // `--enable-gpu` reverses our `--disable-gpu`).
    if (this.opts.extraArgs && this.opts.extraArgs.length > 0) {
      args.push(...this.opts.extraArgs);
    }
    // headless defaults true; only the operator opting out of
    // headless via `browser.headless = false` flips it.
    const headless = this.opts.headless !== false;
    const launchOptions: LaunchOptions = { headless, args };
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
    // Two checks against two paths:
    //  1. The lexical resolve (handles `..`) — catches an explicit
    //     path under a known browser profile dir.
    //  2. The realpath of the deepest existing parent + the missing
    //     tail — catches symlink-based escape (operator points at
    //     `/tmp/aura-trick` which is a symlink to `~/.config/google-chrome`).
    // Either match → refuse to start.
    const lexical = resolvePath(dir);
    const real = realResolveExistingParent(lexical);
    for (const candidate of new Set([lexical, real])) {
      const rel = candidate.startsWith(home + sep)
        ? candidate.slice(home.length + 1)
        : null;
      if (!rel) continue;
      for (const forbidden of FORBIDDEN_PROFILE_PREFIXES) {
        if (rel === forbidden || rel.startsWith(forbidden + sep)) {
          throw new Error(
            `browser sidecar refuses to use system browser profile dir ` +
              `(lexical='${lexical}', real='${real}'); ` +
              `set AURA_BROWSER_PROFILE_DIR to a path under \$XDG_CACHE_HOME/aura/browser/...`,
          );
        }
      }
    }
  }
}

/**
 * Resolve symlinks for the deepest existing ancestor of `dir`, then
 * re-append the missing tail. The profile dir typically doesn't
 * exist yet (created by `mkdirSync` after this check), so a plain
 * `fs.realpathSync(dir)` would throw ENOENT. Walking up to find the
 * deepest existing parent lets us still detect the case where the
 * operator symlinks `~/aura-cache → ~/.config/google-chrome`.
 */
function realResolveExistingParent(p: string): string {
  let head = p;
  const tail: string[] = [];
  while (head && head !== sep && head !== dirname(head)) {
    try {
      statSync(head);
      // Exists. realpath this prefix and append the tail.
      const realHead = realpathSync(head);
      return tail.length === 0 ? realHead : resolvePath(realHead, ...tail.reverse());
    } catch {
      // Not yet created — walk up.
      tail.push(head.slice(dirname(head).length + 1));
      head = dirname(head);
    }
  }
  return p;
}
