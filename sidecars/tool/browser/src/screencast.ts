// Live JPEG screencast of the agent's browser for the web dashboard viewer.
//
// One followed stream (plan K6): the page the agent last touched
// (`noteAgentPage`, fed from the proxy's tool calls), falling back to CDDM's
// selected page. The hub reaches the browser only through the `McpContext`
// handed over by `cddm_tap.ts` and only through public puppeteer calls
// (`page.createCDPSession()`), so it:
// - never calls a CDDM tool, never takes CDDM's tool mutex and never touches
//   `BrowserActivity` — viewer traffic must not read as agent activity to the
//   watchdog;
// - never launches Chrome: with no context yet, the stream is `idle`;
// - opens no CDP session while nobody is watching (`start` / `stop` are
//   driven by the gateway's viewer count over the link).
//
// Chrome ignores ack pacing (spike finding (f): a 100 ms ack delay still gave
// 30 fps), so every frame is acked at once. The fps cap is enforced twice:
// `everyNthFrame` asks Chrome to produce only about `fps` frames per second,
// so the CDP pipe and the event loop never carry the full ~60 fps of an
// animating page (spike finding (c)); dropping here is the hard cap on top.
// The newest dropped frame is kept and sent when the next slot opens, so a
// page that goes still after a burst still shows its final state.

import type { BrowserMode } from "./generated/BrowserMode.js";
import type { BrowserPhase } from "./generated/BrowserPhase.js";
import type { BrowserStatus } from "./generated/BrowserStatus.js";
import type { FrameHeader } from "./generated/FrameHeader.js";
import type { LinkLimits } from "./generated/LinkLimits.js";
import type { StreamState } from "./generated/StreamState.js";
import type { StreamStatus } from "./generated/StreamStatus.js";
import type { TargetInfo } from "./generated/TargetInfo.js";
import type { TargetsMsg } from "./generated/TargetsMsg.js";
import type { UnavailableReason } from "./generated/UnavailableReason.js";
import { errText, type SidecarLogger } from "./log.js";

export const DEFAULT_QUALITY = 60;
export const DEFAULT_MAX_WIDTH = 1280;
export const DEFAULT_MAX_HEIGHT = 1280;
export const DEFAULT_FPS = 10;
/** Host mode shares the CDP pipe and the event loop with CDDM's own calls. */
export const HOST_FPS = 5;
/** The compositor's frame rate an animating page screencasts at (spike finding (c)). */
export const CHROME_SCREENCAST_FPS = 60;

/** Ceiling on `createCDPSession` + `Page.startScreencast` against a wedged browser. */
const ATTACH_TIMEOUT_MS = 5_000;

/** Bursts of target events (a navigation fires several) collapse into one `Targets`. */
const TARGETS_COALESCE_MS = 100;

const PAGE_TARGET_TYPE = "page";
const HIDDEN_URL_PREFIXES = ["chrome://", "devtools://"] as const;

const CDP_START_SCREENCAST = "Page.startScreencast";
const CDP_STOP_SCREENCAST = "Page.stopScreencast";
const CDP_SCREENCAST_FRAME_ACK = "Page.screencastFrameAck";
const CDP_SCREENCAST_FRAME = "Page.screencastFrame";
const CDP_SCREENCAST_VISIBILITY = "Page.screencastVisibilityChanged";
const BROWSER_TARGET_EVENTS = ["targetcreated", "targetdestroyed", "targetchanged"] as const;
const BROWSER_DISCONNECTED = "disconnected";

// ---------------------------------------------------------------------
// Ports: the slice of puppeteer / CDDM the hub uses, so tests can fake it.
// ---------------------------------------------------------------------

type Listener = (payload: unknown) => void;

export interface CdpSessionPort {
  send(method: string, params?: Record<string, unknown>): Promise<unknown>;
  on(event: string, handler: Listener): unknown;
  off(event: string, handler: Listener): unknown;
  detach(): Promise<void>;
}

/**
 * A puppeteer `Target`. Puppeteer exposes neither the CDP target id nor the
 * title publicly; `_getTargetInfo()` / `_targetId` are read when present
 * (puppeteer-core 25.1.0, bundled by the pinned CDDM) and their absence only
 * drops the target from the list.
 */
export interface TargetPort {
  type(): string;
  url(): string;
}

export interface PagePort {
  createCDPSession(): Promise<CdpSessionPort>;
  isClosed(): boolean;
  target(): TargetPort;
}

export interface BrowserPort {
  /** Puppeteer's `Browser.connected`; `false` once the browser is gone. */
  readonly connected?: boolean;
  targets(): TargetPort[];
  on(event: string, handler: Listener): unknown;
  off(event: string, handler: Listener): unknown;
}

/** The read-only slice of CDDM's `McpContext`. */
export interface ContextPort {
  browser: BrowserPort;
  getPageById(pageId: number): { pptrPage: PagePort };
  getSelectedPptrPage(): PagePort;
}

export interface ScreencastSink {
  status(msg: BrowserStatus): void;
  targets(msg: TargetsMsg): void;
  stream(msg: StreamStatus): void;
  availability(unavailable: UnavailableReason | null): void;
  frame(header: FrameHeader, jpeg: Buffer): void;
}

export interface Clock {
  now(): number;
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

const realClock: Clock = {
  now: () => performance.now(),
  setTimeout: (fn, ms) => {
    const t = setTimeout(fn, ms);
    t.unref();
    return t;
  },
  clearTimeout: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
};

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null;
}

/** Duck-type CDDM's `McpContext`; `null` when its shape is not the one we know. */
export function asContextPort(ctx: unknown): ContextPort | null {
  if (!isObject(ctx)) return null;
  const browser = ctx["browser"];
  if (
    !isObject(browser) ||
    typeof browser["targets"] !== "function" ||
    typeof browser["on"] !== "function" ||
    typeof browser["off"] !== "function" ||
    typeof ctx["getPageById"] !== "function" ||
    typeof ctx["getSelectedPptrPage"] !== "function"
  ) {
    return null;
  }
  return ctx as unknown as ContextPort;
}

interface RawTargetInfo {
  targetId: string;
  url: string;
  title: string;
}

function rawTargetInfo(target: TargetPort): RawTargetInfo | null {
  const t = target as TargetPort & { _getTargetInfo?: () => unknown; _targetId?: unknown };
  let info: unknown;
  try {
    info = typeof t._getTargetInfo === "function" ? t._getTargetInfo() : undefined;
  } catch {
    info = undefined;
  }
  const fromInfo = isObject(info) ? info : {};
  const id = typeof fromInfo["targetId"] === "string" ? fromInfo["targetId"] : t._targetId;
  if (typeof id !== "string" || id.length === 0) return null;
  let url: string;
  try {
    url = typeof fromInfo["url"] === "string" ? fromInfo["url"] : t.url();
  } catch {
    return null;
  }
  const title = typeof fromInfo["title"] === "string" ? fromInfo["title"] : "";
  return { targetId: id, url, title };
}

const LONE_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;
const REPLACEMENT_CHAR = "\uFFFD";

function isHighSurrogate(unit: number): boolean {
  return unit >= 0xd800 && unit <= 0xdbff;
}

/**
 * Clamp `text` to `max` UTF-16 units and make it well-formed. A cut can
 * split a surrogate pair and a page can set a lone surrogate on purpose
 * (`document.title = "\ud800"`); either way `JSON.stringify` emits a lone
 * `\udXXX` escape that serde rejects, which would lose the whole message.
 */
export function clampText(text: string, max: number): string {
  const end = Math.max(0, max);
  let cut = text.slice(0, end);
  if (end > 0 && end < text.length && isHighSurrogate(text.charCodeAt(end - 1))) {
    // The cut split a pair: drop its first half rather than mangle it.
    cut = cut.slice(0, -1);
  }
  return cut.replace(LONE_SURROGATE, REPLACEMENT_CHAR);
}

/** A listable page target, clamped to `limits`; `null` when it must not be shown. */
export function toTargetInfo(target: TargetPort, limits: LinkLimits): TargetInfo | null {
  let type: string;
  try {
    type = target.type();
  } catch {
    return null;
  }
  if (type !== PAGE_TARGET_TYPE) return null;
  const raw = rawTargetInfo(target);
  if (raw === null) return null;
  if (raw.targetId.length > limits.max_target_id_chars) return null;
  if (HIDDEN_URL_PREFIXES.some((p) => raw.url.startsWith(p))) return null;
  return {
    target_id: raw.targetId,
    url: clampText(raw.url, limits.max_target_url_chars),
    title: clampText(raw.title, limits.max_text_chars),
  };
}

/** `Page.startScreencast`'s `everyNthFrame` that brings Chrome's rate down to about `fps`. */
export function everyNthFrame(fps: number): number {
  return Math.max(1, Math.round(CHROME_SCREENCAST_FPS / fps));
}

function finite(v: unknown, fallback: number): number {
  return typeof v === "number" && Number.isFinite(v) ? v : fallback;
}

/** Resolve `p`, or `undefined` after `ms`; a late value is handed to `late`. */
async function within<T>(
  p: Promise<T>,
  ms: number,
  clock: Clock,
  late: (value: T) => void,
): Promise<T | undefined> {
  let timedOut = false;
  let handle: unknown;
  const timeout = new Promise<undefined>((resolve) => {
    handle = clock.setTimeout(() => {
      timedOut = true;
      resolve(undefined);
    }, ms);
  });
  try {
    return await Promise.race([
      p.then((value) => {
        if (timedOut) late(value);
        return value;
      }),
      timeout,
    ]);
  } finally {
    clock.clearTimeout(handle);
  }
}

interface PendingFrame {
  data: string;
  metadata: Record<string, unknown>;
}

interface ActiveStream {
  session: CdpSessionPort;
  page: PagePort;
  info: TargetInfo;
  gen: number;
  fps: number;
  lastSentAt: number;
  pending: PendingFrame | null;
  timer: unknown;
  onFrame: Listener;
  onVisibility: Listener;
}

export interface ScreencastHubConfig {
  sink: ScreencastSink;
  log: SidecarLogger;
  mode: BrowserMode;
  /** Injected in tests; defaults to `performance.now()` + unref'd timers. */
  clock?: Clock;
}

export class ScreencastHub {
  readonly #sink: ScreencastSink;
  readonly #log: SidecarLogger;
  readonly #clock: Clock;

  #mode: BrowserMode;
  #phase: BrowserPhase = { type: "idle" };
  #gen = 0;
  #seq = 0;
  #ctx: ContextPort | null = null;
  /** `#ctx`'s browser fired `disconnected`; nothing may be opened on it. */
  #ctxDisconnected = false;
  #unavailable: UnavailableReason | null = null;
  #agentPageId: number | undefined;
  #wanted = false;
  #closed = false;
  #limits: LinkLimits | null = null;
  #active: ActiveStream | null = null;
  #chain: Promise<void> = Promise.resolve();
  #targetsTimer: unknown = null;
  #unwatchBrowser: (() => void) | null = null;
  #lastStatus = "";
  #lastTargets = "";
  #lastStream = "";
  #streamState: StreamState | null = null;
  #oversizeLogged = false;

  constructor(config: ScreencastHubConfig) {
    this.#sink = config.sink;
    this.#log = config.log;
    this.#mode = config.mode;
    this.#clock = config.clock ?? realClock;
  }

  /** A new CDDM context, i.e. a new or replaced `Browser`. */
  attach(ctx: unknown): Promise<void> {
    if (this.#closed) return this.#chain;
    const port = asContextPort(ctx);
    if (port === null) {
      this.#log.warn("screencast: CDDM context has an unexpected shape; viewer stays idle");
      return this.#chain;
    }
    if (port === this.#ctx) return this.#chain;
    this.#unwatchBrowser?.();
    this.#unwatchBrowser = null;
    // The old browser's stream must not be reported under the new gen.
    this.#detach();
    this.#ctx = port;
    this.#ctxDisconnected = false;
    this.#gen = (this.#gen + 1) >>> 0;
    // Page ids belong to the previous browser.
    this.#agentPageId = undefined;
    this.#watchBrowser(port);
    this.#emitStatus();
    this.#emitTargets();
    return this.#reconcile();
  }

  noteAgentPage(pageId: number): void {
    if (this.#closed || pageId === this.#agentPageId) return;
    this.#agentPageId = pageId;
    this.#scheduleTargets();
    if (this.#wanted) void this.#reconcile();
  }

  setStatus(mode: BrowserMode, phase: BrowserPhase): Promise<void> {
    const wasRecovering = this.#phase.type === "recovering";
    const fps = this.fps;
    this.#mode = mode;
    this.#phase = phase;
    this.#emitStatus();
    if (wasRecovering !== (phase.type === "recovering") || fps !== this.fps) {
      return this.#reconcile();
    }
    return this.#chain;
  }

  setAvailability(unavailable: UnavailableReason | null): Promise<void> {
    this.#unavailable = unavailable;
    if (this.#limits !== null) this.#sink.availability(unavailable);
    return this.#reconcile();
  }

  /** The link is up and the gateway announced its limits: resend all state. */
  linkUp(limits: LinkLimits): void {
    this.#limits = limits;
    this.#lastStatus = "";
    this.#lastTargets = "";
    this.#lastStream = "";
    this.#sink.availability(this.#unavailable);
    this.#emitStatus();
    this.#emitTargets();
  }

  linkDown(): Promise<void> {
    this.#limits = null;
    this.#wanted = false;
    return this.#reconcile();
  }

  start(): Promise<void> {
    if (this.#wanted) return this.#chain;
    this.#wanted = true;
    this.#lastStream = "";
    return this.#reconcile();
  }

  stop(): Promise<void> {
    if (!this.#wanted) return this.#chain;
    this.#wanted = false;
    return this.#reconcile();
  }

  close(): Promise<void> {
    this.#closed = true;
    this.#wanted = false;
    this.#unwatchBrowser?.();
    this.#unwatchBrowser = null;
    if (this.#targetsTimer !== null) this.#clock.clearTimeout(this.#targetsTimer);
    this.#targetsTimer = null;
    this.#detach();
    return this.#chain;
  }

  get fps(): number {
    return this.#mode === "host" ? HOST_FPS : DEFAULT_FPS;
  }

  // -------------------------------------------------------------------

  #reconcile(): Promise<void> {
    this.#chain = this.#chain
      .then(() => this.#reconcileOnce())
      .catch((e: unknown) => {
        this.#log.warn(`screencast: ${errText(e)}`);
      });
    return this.#chain;
  }

  async #reconcileOnce(): Promise<void> {
    if (this.#closed || !this.#wanted || this.#limits === null) {
      this.#detach();
      return;
    }
    if (this.#unavailable !== null) {
      this.#detach();
      this.#emitStream("unavailable", null);
      return;
    }
    const ctx = this.#ctx;
    if (ctx === null) {
      this.#detach();
      this.#emitStream("idle", null);
      return;
    }
    // Puppeteer leaves a page "open" when its browser disconnects, so the
    // followed page would still resolve to a dead session on a frozen frame.
    if (this.#phase.type === "recovering" || !this.#isConnected(ctx)) {
      this.#detach();
      this.#emitStream("paused", null);
      return;
    }
    const page = this.#followedPage(ctx);
    if (page === null) {
      this.#detach();
      this.#emitStream("target_gone", null);
      return;
    }
    const active = this.#active;
    if (
      active !== null &&
      active.page === page &&
      active.gen === this.#gen &&
      active.fps === this.fps
    ) {
      return;
    }
    this.#detach();
    await this.#open(ctx, page);
  }

  #isConnected(ctx: ContextPort): boolean {
    if (ctx !== this.#ctx || this.#ctxDisconnected) return false;
    try {
      return ctx.browser.connected !== false;
    } catch {
      return false;
    }
  }

  #followedPage(ctx: ContextPort): PagePort | null {
    const open = (page: PagePort | undefined): PagePort | null => {
      try {
        return page !== undefined && !page.isClosed() ? page : null;
      } catch {
        return null;
      }
    };
    if (this.#agentPageId !== undefined) {
      try {
        const page = open(ctx.getPageById(this.#agentPageId).pptrPage);
        if (page !== null) return page;
      } catch {
        // Unknown or closed id: fall back to the selected page.
      }
    }
    try {
      return open(ctx.getSelectedPptrPage());
    } catch {
      return null;
    }
  }

  async #open(ctx: ContextPort, page: PagePort): Promise<void> {
    const limits = this.#limits;
    if (limits === null) return;
    let info: TargetInfo | null;
    try {
      info = toTargetInfo(page.target(), limits);
    } catch {
      info = null;
    }
    if (info === null) {
      this.#emitStream("target_gone", null);
      return;
    }
    const discard = (s: CdpSessionPort): void => {
      void s.detach().catch(() => undefined);
    };
    let session: CdpSessionPort | undefined;
    try {
      session = await within(page.createCDPSession(), ATTACH_TIMEOUT_MS, this.#clock, discard);
    } catch (e) {
      this.#log.debug(`screencast: createCDPSession failed: ${errText(e)}`);
    }
    if (session === undefined) {
      this.#emitStream("target_gone", null);
      return;
    }
    if (!this.#wanted || this.#closed || !this.#isConnected(ctx)) {
      discard(session);
      return;
    }
    const s = session;
    const fps = this.fps;
    const active: ActiveStream = {
      session: s,
      page,
      info,
      gen: this.#gen,
      fps,
      lastSentAt: Number.NEGATIVE_INFINITY,
      pending: null,
      timer: null,
      onFrame: (payload) => this.#onFrame(active, payload),
      onVisibility: (payload) => {
        if (this.#active !== active || !isObject(payload)) return;
        this.#emitStream(payload["visible"] === false ? "background" : "live", active.info);
      },
    };
    s.on(CDP_SCREENCAST_FRAME, active.onFrame);
    s.on(CDP_SCREENCAST_VISIBILITY, active.onVisibility);
    this.#active = active;
    let started = false;
    try {
      const res = await within(
        s.send(CDP_START_SCREENCAST, {
          format: "jpeg",
          quality: DEFAULT_QUALITY,
          maxWidth: DEFAULT_MAX_WIDTH,
          maxHeight: DEFAULT_MAX_HEIGHT,
          everyNthFrame: everyNthFrame(fps),
        }),
        ATTACH_TIMEOUT_MS,
        this.#clock,
        () => undefined,
      );
      started = res !== undefined;
    } catch (e) {
      this.#log.debug(`screencast: startScreencast failed: ${errText(e)}`);
    }
    if (this.#active !== active) return;
    if (active.gen !== this.#gen || !this.#isConnected(ctx)) {
      this.#detach();
      return;
    }
    if (!started) {
      this.#detach();
      this.#emitStream("target_gone", null);
      return;
    }
    this.#emitStream("live", info);
    this.#emitTargets();
  }

  #detach(): void {
    const active = this.#active;
    if (active === null) return;
    this.#active = null;
    if (active.timer !== null) this.#clock.clearTimeout(active.timer);
    active.timer = null;
    active.pending = null;
    const s = active.session;
    try {
      s.off(CDP_SCREENCAST_FRAME, active.onFrame);
      s.off(CDP_SCREENCAST_VISIBILITY, active.onVisibility);
    } catch {
      // A torn-down session may already have dropped its emitter.
    }
    // Fire-and-forget: a wedged browser must not park the reconcile chain.
    void s
      .send(CDP_STOP_SCREENCAST)
      .catch(() => undefined)
      .then(() => s.detach())
      .catch(() => undefined);
  }

  #onFrame(active: ActiveStream, payload: unknown): void {
    if (this.#active !== active || !isObject(payload)) return;
    const sessionId = payload["sessionId"];
    if (typeof sessionId === "number") {
      void active.session
        .send(CDP_SCREENCAST_FRAME_ACK, { sessionId })
        .catch(() => undefined);
    }
    const data = payload["data"];
    const metadata = payload["metadata"];
    if (typeof data !== "string" || !isObject(metadata)) return;
    const frame: PendingFrame = { data, metadata };
    if (active.timer !== null) {
      active.pending = frame;
      return;
    }
    const interval = 1000 / active.fps;
    const since = this.#clock.now() - active.lastSentAt;
    if (since >= interval) {
      this.#sendFrame(active, frame);
      return;
    }
    active.pending = frame;
    active.timer = this.#clock.setTimeout(() => {
      active.timer = null;
      const next = active.pending;
      active.pending = null;
      if (next !== null && this.#active === active) this.#sendFrame(active, next);
    }, interval - since);
  }

  #sendFrame(active: ActiveStream, frame: PendingFrame): void {
    active.lastSentAt = this.#clock.now();
    const limits = this.#limits;
    if (limits === null) return;
    const jpeg = Buffer.from(frame.data, "base64");
    if (jpeg.length === 0 || jpeg.length > limits.max_frame_bytes) {
      if (!this.#oversizeLogged) {
        this.#oversizeLogged = true;
        this.#log.warn(
          `screencast: dropping a ${jpeg.length}-byte frame (limit ${limits.max_frame_bytes})`,
        );
      }
      return;
    }
    const m = frame.metadata;
    const timestampS = m["timestamp"];
    const header: FrameHeader = {
      target_id: active.info.target_id,
      browser_gen: active.gen,
      seq: this.#seq,
      captured_at_ms:
        typeof timestampS === "number" && Number.isFinite(timestampS)
          ? timestampS * 1000
          : Date.now(),
      device_width: finite(m["deviceWidth"], 0),
      device_height: finite(m["deviceHeight"], 0),
      offset_top: finite(m["offsetTop"], 0),
      page_scale_factor: finite(m["pageScaleFactor"], 1),
      scroll_offset_x: finite(m["scrollOffsetX"], 0),
      scroll_offset_y: finite(m["scrollOffsetY"], 0),
    };
    this.#seq = (this.#seq + 1) >>> 0;
    this.#sink.frame(header, jpeg);
  }

  #watchBrowser(ctx: ContextPort): void {
    const browser = ctx.browser;
    const onTargets: Listener = (target) => {
      this.#scheduleTargets();
      if (this.#active === null || !isObject(target)) return;
      // The followed tab closed or navigated: re-resolve it now rather than
      // at the next coalesced refresh.
      const raw = rawTargetInfo(target as unknown as TargetPort);
      if (raw !== null && raw.targetId === this.#active.info.target_id) {
        void this.#reconcile();
      }
    };
    const onDisconnected: Listener = () => {
      if (this.#ctx !== ctx) return;
      this.#ctxDisconnected = true;
      this.#detach();
      this.#emitTargets();
      void this.#reconcile();
    };
    try {
      for (const ev of BROWSER_TARGET_EVENTS) browser.on(ev, onTargets);
      browser.on(BROWSER_DISCONNECTED, onDisconnected);
    } catch (e) {
      this.#log.warn(`screencast: cannot watch browser targets: ${errText(e)}`);
    }
    this.#unwatchBrowser = () => {
      try {
        for (const ev of BROWSER_TARGET_EVENTS) browser.off(ev, onTargets);
        browser.off(BROWSER_DISCONNECTED, onDisconnected);
      } catch {
        // Best effort on a browser that is already gone.
      }
    };
  }

  #scheduleTargets(): void {
    if (this.#targetsTimer !== null || this.#closed) return;
    this.#targetsTimer = this.#clock.setTimeout(() => {
      this.#targetsTimer = null;
      this.#emitTargets();
    }, TARGETS_COALESCE_MS);
  }

  #emitStatus(): void {
    const limits = this.#limits;
    if (limits === null) return;
    const phase: BrowserPhase =
      this.#phase.type === "failed"
        ? { type: "failed", error: clampText(this.#phase.error, limits.max_text_chars) }
        : this.#phase;
    const msg: BrowserStatus = { mode: this.#mode, phase, browser_gen: this.#gen };
    const key = JSON.stringify(msg);
    if (key === this.#lastStatus) return;
    this.#lastStatus = key;
    this.#sink.status(msg);
  }

  #emitTargets(): void {
    const limits = this.#limits;
    if (limits === null) return;
    const ctx = this.#ctx;
    const targets: TargetInfo[] = [];
    let followed: string | null = null;
    if (ctx !== null && this.#isConnected(ctx)) {
      let all: TargetPort[] = [];
      try {
        all = ctx.browser.targets();
      } catch {
        all = [];
      }
      for (const t of all) {
        if (targets.length >= limits.max_targets) break;
        const info = toTargetInfo(t, limits);
        if (info !== null) targets.push(info);
      }
      const active = this.#active;
      if (active !== null) {
        followed = active.info.target_id;
        const fresh = targets.find((t) => t.target_id === followed);
        if (fresh !== undefined && (fresh.url !== active.info.url || fresh.title !== active.info.title)) {
          active.info = fresh;
          this.#emitStream(this.#streamState ?? "live", fresh);
        }
      } else {
        const page = this.#followedPage(ctx);
        if (page !== null) {
          try {
            followed = toTargetInfo(page.target(), limits)?.target_id ?? null;
          } catch {
            followed = null;
          }
        }
      }
    }
    const msg: TargetsMsg = { browser_gen: this.#gen, targets, followed };
    const key = JSON.stringify(msg);
    if (key === this.#lastTargets) return;
    this.#lastTargets = key;
    this.#sink.targets(msg);
  }

  #emitStream(state: StreamState, target: TargetInfo | null): void {
    if (this.#limits === null) return;
    const msg: StreamStatus = { state, browser_gen: this.#gen, target };
    const key = JSON.stringify(msg);
    if (key === this.#lastStream) return;
    this.#lastStream = key;
    this.#streamState = state;
    this.#sink.stream(msg);
  }
}
