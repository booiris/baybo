// The sidecar's end of the browser-view link: a unix stream socket the
// gateway listens on (it binds before spawning this process) and this
// sidecar dials. Framing and message shapes are owned by
// `crates/browser-view` (`codec.rs`, `wire.rs`); the types come from its
// ts-rs output in `src/generated/`.
//
//   [u32 BE total_len][u8 kind][body]      total_len = 1 + body length
//   kind 0: body = one JSON LinkUp / LinkDown
//   kind 1: body = [u32 BE hdr_len][FrameHeader JSON][JPEG]   (sidecar → gateway only)
//
// Handshake: we send `Hello` first, the gateway answers `HelloAck` (whose
// limits we adopt) or `HelloReject`. Only one link may be live (first wins),
// so `already_connected` is retried with backoff — it is what a respawned
// sidecar sees while the gateway is still noticing the old link closed —
// while `unauthorized` / `protocol_mismatch` can never succeed and stop the
// link for good.
//
// Backpressure: once `socket.write()` returns false, frames are dropped
// (only the newest is kept) until `'drain'`. JSON control messages are
// always written: they are small and each one is state the viewer needs.

import { randomUUID } from "node:crypto";
import { createConnection } from "node:net";

import type { FrameHeader } from "./generated/FrameHeader.js";
import type { LinkDown } from "./generated/LinkDown.js";
import type { LinkLimits } from "./generated/LinkLimits.js";
import type { LinkUp } from "./generated/LinkUp.js";
import type { BrowserStatus } from "./generated/BrowserStatus.js";
import type { StreamStatus } from "./generated/StreamStatus.js";
import type { TargetsMsg } from "./generated/TargetsMsg.js";
import type { UnavailableReason } from "./generated/UnavailableReason.js";
import { errText, type SidecarLogger } from "./log.js";
import type { ScreencastSink } from "./screencast.js";

export const ENV_LINK_SOCKET = "BAYBO_BROWSER_LINK_SOCKET";
export const ENV_LINK_SECRET = "BAYBO_BROWSER_LINK_SECRET";

// Framing constants mirrored from `crates/browser-view/src/limits.rs` and
// `codec.rs`. They are protocol, not tunables: ts-rs exports types only, and
// the limits that *are* tunable arrive in `HelloAck`. The Rust test
// `limits::tests::sidecar_mirrors_match` fails when these drift.
export const BROWSER_LINK_PROTOCOL_VERSION = 1;
export const KIND_JSON = 0;
export const KIND_FRAME = 1;
export const LEN_PREFIX_BYTES = 4;
export const KIND_BYTES = 1;
export const MAX_FRAME_HEADER_BYTES = 4096;

/**
 * Inbound JSON cap until `HelloAck` announces the real one. Every
 * gateway → sidecar message is a few hundred bytes at most.
 */
export const PRE_ACK_MAX_JSON_BYTES = 64 * 1024;

const RECONNECT_BASE_MS = 250;
const RECONNECT_MAX_MS = 10_000;
/**
 * Doubling steps the backoff exponent stops counting at. Only bounds the
 * counter (`2 ** n` stays finite); `RECONNECT_MAX_MS` caps the delay long
 * before.
 */
const MAX_BACKOFF_EXPONENT = 30;
/**
 * How long we wait for `HelloAck`. Longer than the gateway's own `Hello`
 * deadline (`limits.rs` `HELLO_TIMEOUT`), so a slow gateway is never cut
 * off by our side first.
 */
const HELLO_ACK_TIMEOUT_MS = 10_000;

const LINK_DOWN_TYPES: ReadonlySet<string> = new Set(
  Object.keys({
    hello_ack: true,
    hello_reject: true,
    start_screencast: true,
    stop_screencast: true,
  } satisfies Record<LinkDown["type"], true>),
);

export interface LinkEnv {
  socketPath: string;
  secret: string;
}

/**
 * Read the link configuration and remove it from `env`, so neither value
 * reaches a child process (Chrome is spawned by puppeteer with
 * `process.env`). Both are always deleted; `null` means the feature is off.
 */
export function takeLinkEnv(env: NodeJS.ProcessEnv): LinkEnv | null {
  const socketPath = env[ENV_LINK_SOCKET];
  const secret = env[ENV_LINK_SECRET];
  delete env[ENV_LINK_SOCKET];
  delete env[ENV_LINK_SECRET];
  if (socketPath === undefined || socketPath.length === 0) return null;
  if (secret === undefined || secret.length === 0) return null;
  return { socketPath, secret };
}

export class LinkCodecError extends Error {
  override readonly name = "LinkCodecError";
}

function message(kind: number, parts: Buffer[]): Buffer {
  const bodyLen = parts.reduce((n, p) => n + p.length, 0);
  const prefix = Buffer.allocUnsafe(LEN_PREFIX_BYTES + KIND_BYTES);
  prefix.writeUInt32BE(KIND_BYTES + bodyLen, 0);
  prefix.writeUInt8(kind, LEN_PREFIX_BYTES);
  return Buffer.concat([prefix, ...parts]);
}

export function encodeJson(msg: LinkUp, maxJsonBytes: number): Buffer {
  const body = Buffer.from(JSON.stringify(msg), "utf8");
  if (body.length > maxJsonBytes) {
    throw new LinkCodecError(`json message of ${body.length} bytes exceeds ${maxJsonBytes}`);
  }
  return message(KIND_JSON, [body]);
}

export function encodeFrame(header: FrameHeader, jpeg: Buffer, maxFrameBytes: number): Buffer {
  const headerJson = Buffer.from(JSON.stringify(header), "utf8");
  if (headerJson.length > MAX_FRAME_HEADER_BYTES) {
    throw new LinkCodecError(
      `frame header of ${headerJson.length} bytes exceeds ${MAX_FRAME_HEADER_BYTES}`,
    );
  }
  if (jpeg.length === 0) throw new LinkCodecError("empty jpeg");
  if (jpeg.length > maxFrameBytes) {
    throw new LinkCodecError(`jpeg of ${jpeg.length} bytes exceeds ${maxFrameBytes}`);
  }
  const headerLen = Buffer.allocUnsafe(LEN_PREFIX_BYTES);
  headerLen.writeUInt32BE(headerJson.length, 0);
  return message(KIND_FRAME, [headerLen, headerJson, jpeg]);
}

export type DecodedLinkDown = { ok: LinkDown } | { malformed: string };

/**
 * Incremental decoder for gateway → sidecar messages. Lengths are checked
 * as soon as their bytes arrive, before anything is buffered for the body.
 * Framing violations throw {@link LinkCodecError} (the link is then
 * unusable); a well-framed message we cannot parse comes back `malformed`.
 */
export class LinkDownDecoder {
  #buf: Buffer = Buffer.alloc(0);
  maxJsonBytes: number;

  constructor(maxJsonBytes: number) {
    this.maxJsonBytes = maxJsonBytes;
  }

  push(chunk: Buffer): DecodedLinkDown[] {
    this.#buf = this.#buf.length === 0 ? chunk : Buffer.concat([this.#buf, chunk]);
    const out: DecodedLinkDown[] = [];
    for (;;) {
      if (this.#buf.length < LEN_PREFIX_BYTES) break;
      const len = this.#buf.readUInt32BE(0);
      if (len === 0) throw new LinkCodecError("empty message");
      if (len - KIND_BYTES > this.maxJsonBytes) {
        throw new LinkCodecError(`message of ${len} bytes exceeds ${this.maxJsonBytes}`);
      }
      if (this.#buf.length < LEN_PREFIX_BYTES + KIND_BYTES) break;
      const kind = this.#buf.readUInt8(LEN_PREFIX_BYTES);
      if (kind !== KIND_JSON) throw new LinkCodecError(`unexpected message kind ${kind}`);
      const total = LEN_PREFIX_BYTES + len;
      if (this.#buf.length < total) break;
      const body = this.#buf.subarray(LEN_PREFIX_BYTES + KIND_BYTES, total);
      this.#buf = this.#buf.subarray(total);
      out.push(parseLinkDown(body));
    }
    return out;
  }
}

function parseLinkDown(body: Buffer): DecodedLinkDown {
  let value: unknown;
  try {
    value = JSON.parse(body.toString("utf8"));
  } catch (e) {
    return { malformed: errText(e) };
  }
  if (typeof value !== "object" || value === null) return { malformed: "not an object" };
  const type = (value as { type?: unknown }).type;
  if (typeof type !== "string" || !LINK_DOWN_TYPES.has(type)) {
    return { malformed: `unknown type ${JSON.stringify(type)}` };
  }
  return { ok: value as LinkDown };
}

/** What the link drives on the screencast side. */
export interface LinkHandler {
  linkUp(limits: LinkLimits): void;
  linkDown(): void;
  startScreencast(): void;
  stopScreencast(): void;
}

/** The slice of `net.Socket` the link uses; faked in tests. */
export interface LinkSocket {
  write(data: Buffer): boolean;
  on(event: string, listener: (...args: never[]) => void): unknown;
  once(event: string, listener: (...args: never[]) => void): unknown;
  destroy(): void;
}

export interface ViewLinkConfig {
  socketPath: string;
  secret: string;
  handler: LinkHandler;
  log: SidecarLogger;
  /** Injected in tests; defaults to `net.createConnection`. */
  connect?: (path: string) => LinkSocket;
  reconnectBaseMs?: number;
  reconnectMaxMs?: number;
}

type LinkState = "down" | "handshaking" | "up";

export class ViewLink implements ScreencastSink {
  readonly #socketPath: string;
  readonly #secret: string;
  readonly #handler: LinkHandler;
  readonly #log: SidecarLogger;
  readonly #connect: (path: string) => LinkSocket;
  readonly #reconnectBaseMs: number;
  readonly #reconnectMaxMs: number;
  readonly #bootId = randomUUID();

  #socket: LinkSocket | null = null;
  #state: LinkState = "down";
  #limits: LinkLimits | null = null;
  #decoder = new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES);
  #attempts = 0;
  #reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  #helloTimer: ReturnType<typeof setTimeout> | null = null;
  #blocked = false;
  #pendingFrame: { header: FrameHeader; jpeg: Buffer } | null = null;
  #outageLogged = false;
  #closed = false;

  constructor(config: ViewLinkConfig) {
    this.#socketPath = config.socketPath;
    this.#secret = config.secret;
    this.#handler = config.handler;
    this.#log = config.log;
    this.#connect = config.connect ?? ((path) => createConnection(path));
    this.#reconnectBaseMs = config.reconnectBaseMs ?? RECONNECT_BASE_MS;
    this.#reconnectMaxMs = config.reconnectMaxMs ?? RECONNECT_MAX_MS;
  }

  get isUp(): boolean {
    return this.#state === "up";
  }

  start(): void {
    if (this.#closed || this.#socket !== null || this.#reconnectTimer !== null) return;
    this.#dial();
  }

  /** Stops reconnecting first, then drops the socket. Idempotent. */
  shutdown(): Promise<void> {
    this.#closed = true;
    if (this.#reconnectTimer !== null) clearTimeout(this.#reconnectTimer);
    this.#reconnectTimer = null;
    this.#socket?.destroy();
    return Promise.resolve();
  }

  // ScreencastSink -------------------------------------------------------

  status(msg: BrowserStatus): void {
    this.#sendJson({ type: "status", ...msg });
  }

  targets(msg: TargetsMsg): void {
    this.#sendJson({ type: "targets", ...msg });
  }

  stream(msg: StreamStatus): void {
    this.#sendJson({ type: "stream", ...msg });
  }

  availability(unavailable: UnavailableReason | null): void {
    this.#sendJson({ type: "availability", unavailable });
  }

  frame(header: FrameHeader, jpeg: Buffer): void {
    if (this.#state !== "up") return;
    if (this.#blocked) {
      this.#pendingFrame = { header, jpeg };
      return;
    }
    this.#writeFrame(header, jpeg);
  }

  // ---------------------------------------------------------------------

  #dial(): void {
    let socket: LinkSocket;
    try {
      socket = this.#connect(this.#socketPath);
    } catch (e) {
      this.#noteOutage(`cannot dial ${this.#socketPath}: ${errText(e)}`);
      this.#scheduleReconnect();
      return;
    }
    this.#socket = socket;
    this.#state = "handshaking";
    this.#decoder = new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES);
    let lastError = "";
    socket.on("data", (chunk: Buffer) => this.#onData(socket, chunk));
    socket.on("error", (e: Error) => {
      lastError = errText(e);
    });
    socket.on("close", () => this.#onClose(socket, lastError));
    this.#helloTimer = setTimeout(() => {
      if (this.#socket === socket && this.#state === "handshaking") {
        lastError = "no HelloAck from the gateway";
        socket.destroy();
      }
    }, HELLO_ACK_TIMEOUT_MS);
    this.#helloTimer.unref();
    const hello: LinkUp = {
      type: "hello",
      protocol: BROWSER_LINK_PROTOCOL_VERSION,
      secret: this.#secret,
      boot_id: this.#bootId,
      pid: process.pid,
      capabilities: ["screencast"],
    };
    this.#write(encodeJson(hello, PRE_ACK_MAX_JSON_BYTES));
  }

  #onData(socket: LinkSocket, chunk: Buffer): void {
    if (socket !== this.#socket) return;
    let items: DecodedLinkDown[];
    try {
      items = this.#decoder.push(chunk);
    } catch (e) {
      this.#log.warn(`view link: protocol error from gateway (${errText(e)}); reconnecting`);
      socket.destroy();
      return;
    }
    for (const item of items) {
      if (socket !== this.#socket) return;
      if ("malformed" in item) {
        this.#log.debug(`view link: skipping malformed gateway message: ${item.malformed}`);
        continue;
      }
      this.#handle(socket, item.ok);
    }
  }

  #handle(socket: LinkSocket, msg: LinkDown): void {
    switch (msg.type) {
      case "hello_ack": {
        if (this.#state !== "handshaking") return;
        if (msg.protocol !== BROWSER_LINK_PROTOCOL_VERSION) {
          this.#giveUp(`gateway speaks link protocol ${msg.protocol}, we speak ${BROWSER_LINK_PROTOCOL_VERSION}`);
          return;
        }
        this.#clearHelloTimer();
        this.#state = "up";
        this.#limits = msg.limits;
        this.#decoder.maxJsonBytes = msg.limits.max_json_bytes;
        this.#attempts = 0;
        if (this.#outageLogged) this.#log.info("view link to gateway restored");
        this.#outageLogged = false;
        this.#handler.linkUp(msg.limits);
        return;
      }
      case "hello_reject":
        if (msg.reason === "already_connected") {
          this.#log.debug("view link: gateway still holds a previous link; retrying");
          socket.destroy();
          return;
        }
        this.#giveUp(`gateway rejected the view link (${msg.reason})`);
        return;
      case "start_screencast":
        if (this.#state === "up") this.#handler.startScreencast();
        return;
      case "stop_screencast":
        if (this.#state === "up") this.#handler.stopScreencast();
        return;
    }
  }

  #giveUp(reason: string): void {
    this.#log.error(`${reason}; browser viewer disabled until the sidecar restarts`);
    this.#closed = true;
    this.#socket?.destroy();
  }

  #onClose(socket: LinkSocket, lastError: string): void {
    if (socket !== this.#socket) return;
    const wasUp = this.#state === "up";
    this.#socket = null;
    this.#state = "down";
    this.#limits = null;
    this.#blocked = false;
    this.#pendingFrame = null;
    this.#clearHelloTimer();
    if (wasUp) this.#handler.linkDown();
    if (this.#closed) return;
    this.#noteOutage(lastError.length > 0 ? lastError : "connection closed");
    this.#scheduleReconnect();
  }

  #noteOutage(reason: string): void {
    // Once per outage, not per attempt: the gateway budgets this sidecar's
    // stderr lines, and a retry loop would spend that budget.
    if (this.#outageLogged) return;
    this.#outageLogged = true;
    this.#log.warn(`view link to gateway unavailable (${reason}); reconnecting in the background`);
  }

  #scheduleReconnect(): void {
    if (this.#closed || this.#reconnectTimer !== null) return;
    const ceiling = Math.min(this.#reconnectMaxMs, this.#reconnectBaseMs * 2 ** this.#attempts);
    this.#attempts = Math.min(this.#attempts + 1, MAX_BACKOFF_EXPONENT);
    const delay = ceiling / 2 + Math.random() * (ceiling / 2);
    this.#reconnectTimer = setTimeout(() => {
      this.#reconnectTimer = null;
      if (!this.#closed) this.#dial();
    }, delay);
    this.#reconnectTimer.unref();
  }

  #clearHelloTimer(): void {
    if (this.#helloTimer !== null) clearTimeout(this.#helloTimer);
    this.#helloTimer = null;
  }

  #sendJson(msg: LinkUp): void {
    const limits = this.#limits;
    if (this.#state !== "up" || limits === null) return;
    let buf: Buffer;
    try {
      buf = encodeJson(msg, limits.max_json_bytes);
    } catch (e) {
      this.#log.warn(`view link: dropping ${msg.type} message: ${errText(e)}`);
      return;
    }
    this.#write(buf);
  }

  #writeFrame(header: FrameHeader, jpeg: Buffer): void {
    const limits = this.#limits;
    if (limits === null) return;
    let buf: Buffer;
    try {
      buf = encodeFrame(header, jpeg, limits.max_frame_bytes);
    } catch (e) {
      this.#log.debug(`view link: dropping frame: ${errText(e)}`);
      return;
    }
    if (this.#write(buf)) return;
    this.#blocked = true;
    const socket = this.#socket;
    socket?.once("drain", () => {
      if (socket !== this.#socket) return;
      this.#blocked = false;
      const next = this.#pendingFrame;
      this.#pendingFrame = null;
      if (next !== null && this.#state === "up") this.#writeFrame(next.header, next.jpeg);
    });
  }

  /** `false` when the socket is congested (or gone). */
  #write(buf: Buffer): boolean {
    const socket = this.#socket;
    if (socket === null) return false;
    try {
      return socket.write(buf);
    } catch (e) {
      this.#log.debug(`view link: write failed: ${errText(e)}`);
      return false;
    }
  }
}
