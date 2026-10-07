// WebSocket transport for the browser live view (`/v1/browser/view/ws`).
//
// Text messages are JSON `ViewerDown` / `ViewerUp`; binary messages are one
// screencast frame each, `[u32 BE hdr_len][FrameHeader JSON][JPEG]`, forwarded
// verbatim from the sidecar. The gateway closes a viewer that stops pinging,
// so the cadence it announces in `ViewerDown::Link` is followed exactly.
//
// The view holds no control in M1, so a hidden tab simply disconnects: the
// last viewer leaving is what stops the screencast in the browser, and there
// is no reason to keep a background tab pulling JPEGs nobody sees.

import type { FrameHeader, ViewerDown, ViewerUp } from './browserViewTypes';
import { buildWsUrl } from './wsUrl';
import { ReconnectBackoff } from './wsReconnect';

export const BROWSER_VIEW_WS_PATH = '/v1/browser/view/ws';

const HDR_LEN_BYTES = 4;
/** Floor on the announced ping cadence, so a bogus `ping_interval_ms` can't
 *  turn the ping timer into a hot loop. */
const MIN_PING_INTERVAL_MS = 1000;
/** Missing this many ping intervals' worth of inbound traffic (frames,
 *  pongs, anything) means the socket is half-open. */
const LIVENESS_INTERVALS = 2;

export interface ScreencastFrame {
  header: FrameHeader;
  jpeg: Blob;
}

export type BrowserViewConnStatus =
  | { state: 'connecting' }
  | { state: 'connected' }
  | { state: 'disconnected'; retryInMs: number; lastError?: string }
  | { state: 'hidden' };

/** What decides whether the connection should be open. Defaults to
 *  `document`; injectable for tests. */
export interface VisibilitySource {
  readonly visibilityState: DocumentVisibilityState;
  addEventListener(type: 'visibilitychange', listener: () => void): void;
  removeEventListener(type: 'visibilitychange', listener: () => void): void;
}

export interface BrowserViewConnectionOptions {
  baseUrl: string;
  token: string;
  onMessage: (msg: ViewerDown) => void;
  onFrame: (frame: ScreencastFrame) => void;
  onStatus: (status: BrowserViewConnStatus) => void;
  visibility?: VisibilitySource;
}

/** Splits one binary WS message into its header and JPEG. `null` for a
 *  truncated message or a header that is not a `FrameHeader`. */
export function parseFramePayload(buf: ArrayBuffer): ScreencastFrame | null {
  if (buf.byteLength < HDR_LEN_BYTES) return null;
  const hdrLen = new DataView(buf).getUint32(0, false);
  const jpegStart = HDR_LEN_BYTES + hdrLen;
  if (hdrLen === 0 || jpegStart > buf.byteLength) return null;
  let header: unknown;
  try {
    header = JSON.parse(new TextDecoder().decode(new Uint8Array(buf, HDR_LEN_BYTES, hdrLen)));
  } catch {
    return null;
  }
  if (!isFrameHeader(header)) return null;
  return { header, jpeg: new Blob([buf.slice(jpegStart)], { type: 'image/jpeg' }) };
}

const FRAME_HEADER_NUMBERS = [
  'browser_gen',
  'seq',
  'captured_at_ms',
  'device_width',
  'device_height',
  'offset_top',
  'page_scale_factor',
  'scroll_offset_x',
  'scroll_offset_y',
] as const satisfies readonly (keyof FrameHeader)[];

function isFrameHeader(v: unknown): v is FrameHeader {
  if (typeof v !== 'object' || v === null) return false;
  const rec = v as Record<string, unknown>;
  return (
    typeof rec.target_id === 'string' &&
    FRAME_HEADER_NUMBERS.every((k) => typeof rec[k] === 'number')
  );
}

/** Parses one text WS message. Unknown `type`s pass through as `null` so a
 *  newer gateway can add messages without breaking an older dashboard. */
export function parseViewerDown(text: string): ViewerDown | null {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof v !== 'object' || v === null) return null;
  const type = (v as { type?: unknown }).type;
  return typeof type === 'string' && VIEWER_DOWN_TYPES.has(type) ? (v as ViewerDown) : null;
}

const VIEWER_DOWN_TYPES: ReadonlySet<string> = new Set(
  Object.keys({
    link: true,
    status: true,
    targets: true,
    stream: true,
    error: true,
    pong: true,
  } satisfies Record<ViewerDown['type'], true>),
);

export class BrowserViewConnection {
  private ws: WebSocket | null = null;
  private readonly backoff = new ReconnectBackoff();
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private pingIntervalMs = 0;
  private lastInboundAt = 0;
  private closed = false;
  private readonly visibility: VisibilitySource;
  private readonly onVisibility = (): void => this.applyVisibility();

  constructor(private readonly opts: BrowserViewConnectionOptions) {
    this.visibility = opts.visibility ?? document;
    this.visibility.addEventListener('visibilitychange', this.onVisibility);
    this.applyVisibility();
  }

  /** Tear the connection down permanently. */
  close(): void {
    this.closed = true;
    this.visibility.removeEventListener('visibilitychange', this.onVisibility);
    this.cancelRetry();
    this.detachAndCloseWs();
  }

  private applyVisibility(): void {
    if (this.closed) return;
    if (this.visibility.visibilityState === 'hidden') {
      this.cancelRetry();
      this.detachAndCloseWs();
      this.opts.onStatus({ state: 'hidden' });
      return;
    }
    if (this.ws || this.retryTimer) return;
    this.backoff.reset();
    this.connect();
  }

  private connect(): void {
    if (this.closed) return;
    this.opts.onStatus({ state: 'connecting' });
    let ws: WebSocket;
    try {
      ws = new WebSocket(buildWsUrl(this.opts.baseUrl, BROWSER_VIEW_WS_PATH, this.opts.token));
    } catch (err) {
      this.scheduleReconnect(String(err));
      return;
    }
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    ws.onopen = () => {
      // The backoff resets on the first `link` message, not here: a gateway
      // that is full upgrades, says `too_many_viewers` and closes, and
      // resetting on open would redial it every second forever.
      this.lastInboundAt = Date.now();
      this.opts.onStatus({ state: 'connected' });
    };
    ws.onmessage = (e) => this.onMessage(e);
    ws.onclose = (e) => {
      this.stopPing();
      this.ws = null;
      if (this.closed) return;
      this.scheduleReconnect(`ws close (${e.code}${e.reason ? `: ${e.reason}` : ''})`);
    };
  }

  private onMessage(e: MessageEvent): void {
    this.lastInboundAt = Date.now();
    if (e.data instanceof ArrayBuffer) {
      const frame = parseFramePayload(e.data);
      if (frame) this.opts.onFrame(frame);
      return;
    }
    if (typeof e.data !== 'string') return;
    const msg = parseViewerDown(e.data);
    if (!msg) return;
    if (msg.type === 'link') {
      this.backoff.reset();
      this.startPing(msg.ping_interval_ms);
    }
    if (msg.type === 'pong') return;
    this.opts.onMessage(msg);
  }

  private startPing(announcedMs: number): void {
    const intervalMs = Math.max(announcedMs, MIN_PING_INTERVAL_MS);
    if (this.pingTimer && intervalMs === this.pingIntervalMs) return;
    this.stopPing();
    this.pingIntervalMs = intervalMs;
    this.pingTimer = setInterval(() => this.pingTick(), intervalMs);
  }

  private stopPing(): void {
    if (this.pingTimer) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
  }

  private pingTick(): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    if (Date.now() - this.lastInboundAt > LIVENESS_INTERVALS * this.pingIntervalMs) {
      // Half-open: closing routes through onclose into the reconnect ladder.
      this.stopPing();
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      return;
    }
    this.send({ type: 'ping' });
  }

  private send(msg: ViewerUp): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify(msg));
  }

  private scheduleReconnect(reason: string): void {
    if (this.closed) return;
    const delay = this.backoff.nextDelayMs();
    this.opts.onStatus({ state: 'disconnected', retryInMs: delay, lastError: reason });
    this.cancelRetry();
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.connect();
    }, delay);
  }

  private cancelRetry(): void {
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
  }

  private detachAndCloseWs(): void {
    this.stopPing();
    const old = this.ws;
    if (!old) return;
    // Strip handlers first so this socket's deferred onclose can't race the
    // next one.
    old.onopen = null;
    old.onmessage = null;
    old.onerror = null;
    old.onclose = null;
    try {
      old.close();
    } catch {
      /* ignore */
    }
    this.ws = null;
  }
}
