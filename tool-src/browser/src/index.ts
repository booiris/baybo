#!/usr/bin/env node
import { homedir } from "node:os";
import { join } from "node:path";

import WebSocket from "ws";

import { BrowserManager } from "./manager.js";
import { buildHandlers } from "./handlers.js";
import { PROTOCOL_VERSION, decode, dispatch, encode, type ToolFrame } from "./rpc.js";

const RECONNECT_MIN_MS = 500;
const RECONNECT_MAX_MS = 30 * 1000;
const PING_INTERVAL_MS = 30 * 1000;

interface Env {
  wsUrl: string;
  wsToken: string;
  sidecarId: string;
  profileDir: string;
  chromiumExecutable: string | undefined;
  noSandbox: boolean;
  allowLoopback: boolean;
}

function readEnv(): Env {
  const wsUrl = process.env["AURA_TOOL_WS_URL"];
  const wsToken = process.env["AURA_TOOL_WS_TOKEN"];
  if (!wsUrl) throw new Error("AURA_TOOL_WS_URL is not set");
  if (!wsToken) throw new Error("AURA_TOOL_WS_TOKEN is not set");
  const sidecarId = process.env["AURA_TOOL_SIDECAR_ID"] ?? "browser";
  const profileDir =
    process.env["AURA_BROWSER_PROFILE_DIR"] ??
    process.env["AURA_PROFILE_DIR"] ??
    defaultProfileDir();
  const chromiumExecutable = process.env["AURA_CHROMIUM_BIN"];
  const noSandbox = parseBoolEnv(process.env["AURA_BROWSER_NO_SANDBOX"]);
  // Test-only: allows the in-sidecar SSRF policy to admit 127.0.0.0/8
  // + ::1 so a smoke test can drive a real Chromium against a local
  // HTTP server. Production never sets this.
  const allowLoopback = parseBoolEnv(process.env["AURA_BROWSER_ALLOW_LOOPBACK"]);
  return {
    wsUrl,
    wsToken,
    sidecarId,
    profileDir,
    chromiumExecutable,
    noSandbox,
    allowLoopback,
  };
}

function parseBoolEnv(s: string | undefined): boolean {
  if (!s) return false;
  const v = s.trim().toLowerCase();
  return v === "1" || v === "true" || v === "yes" || v === "on";
}

function defaultProfileDir(): string {
  const xdg = process.env["XDG_CACHE_HOME"];
  const root = xdg && xdg.length > 0 ? xdg : join(homedir(), ".cache");
  return join(root, "aura", "browser", "profile");
}

async function main(): Promise<void> {
  const env = readEnv();

  // Declare connection-state vars BEFORE constructing BrowserManager.
  // The manager's constructor receives an `emitEvent` closure that
  // references `pendingEvents` / `flushEvents`; if either of those
  // were declared after `new BrowserManager(...)`, any synchronous
  // emit during construction would hit a TDZ ReferenceError. Even
  // though no current path emits synchronously, the ordering is the
  // load-bearing invariant — keep them above the constructor call.
  let socket: WebSocket | null = null;
  let backoff = RECONNECT_MIN_MS;
  let pingTimer: NodeJS.Timeout | null = null;
  const pendingEvents: ToolFrame[] = [];
  const PENDING_EVENTS_MAX = 256;

  const manager = new BrowserManager({
    profileDir: env.profileDir,
    chromiumExecutable: env.chromiumExecutable,
    noSandbox: env.noSandbox,
    allowLoopback: env.allowLoopback,
    emitEvent: (name, data) => {
      // Bounded queue: a long disconnect during a chatty session
      // (dialog floods, console.error storms) would otherwise grow
      // unbounded across reconnects. Drop oldest on overflow.
      if (pendingEvents.length >= PENDING_EVENTS_MAX) {
        pendingEvents.shift();
      }
      pendingEvents.push({ kind: "event", name, data });
      flushEvents();
    },
  });
  const handlers = buildHandlers(manager);

  function flushEvents(): void {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    while (pendingEvents.length > 0) {
      const frame = pendingEvents.shift()!;
      try {
        socket.send(encode(frame));
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        process.stderr.write(`tool-ws send failed: ${msg}\n`);
      }
    }
  }

  function send(frame: ToolFrame): void {
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    try {
      socket.send(encode(frame));
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      process.stderr.write(`tool-ws send failed: ${msg}\n`);
    }
  }

  const shutdown = async (): Promise<void> => {
    if (pingTimer) clearInterval(pingTimer);
    socket?.close();
    await manager.shutdown();
    process.exit(0);
  };
  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());

  while (true) {
    socket = new WebSocket(env.wsUrl, {
      // Same custom header the channel-ws sidecars use; the gateway's
      // channel-auth middleware accepts it for tool-ws too.
      headers: { "x-aura-channel-token": env.wsToken },
    });

    let resolveExit: () => void;
    const exited = new Promise<void>((res) => {
      resolveExit = res;
    });

    socket.binaryType = "nodebuffer";

    socket.on("open", () => {
      const reg: ToolFrame = {
        kind: "register",
        token: env.wsToken,
        sidecar_id: env.sidecarId,
        protocol_version: PROTOCOL_VERSION,
      };
      send(reg);
    });

    socket.on("message", (data: WebSocket.RawData) => {
      let frame: ToolFrame;
      try {
        const bytes =
          data instanceof Buffer
            ? data
            : Array.isArray(data)
            ? Buffer.concat(data)
            : Buffer.from(data as ArrayBuffer);
        frame = decode(bytes);
      } catch (e) {
        const msg = e instanceof Error ? e.message : String(e);
        process.stderr.write(`tool-ws decode failed: ${msg}\n`);
        return;
      }
      void handleFrame(frame);
    });

    async function handleFrame(frame: ToolFrame): Promise<void> {
      switch (frame.kind) {
        case "register_ack": {
          if (!frame.ok) {
            process.stderr.write(`tool-ws register rejected: ${frame.reason ?? "(no reason)"}\n`);
            socket?.close();
            return;
          }
          process.stdout.write(`tool-ws registered as ${env.sidecarId}\n`);
          backoff = RECONNECT_MIN_MS;
          flushEvents();
          if (pingTimer) clearInterval(pingTimer);
          pingTimer = setInterval(() => send({ kind: "ping", ts: Date.now() }), PING_INTERVAL_MS);
          pingTimer.unref?.();
          return;
        }
        case "rpc_request": {
          const reply = await dispatch(handlers, frame);
          send(reply);
          return;
        }
        case "ping": {
          send({ kind: "pong", ts: frame.ts });
          return;
        }
        case "pong":
        case "event":
        case "rpc_result":
        case "rpc_error":
          return;
        default:
          process.stderr.write(`tool-ws ignoring unexpected frame kind\n`);
      }
    }

    socket.on("close", () => {
      if (pingTimer) {
        clearInterval(pingTimer);
        pingTimer = null;
      }
      socket = null;
      resolveExit();
    });

    socket.on("error", (e: Error) => {
      process.stderr.write(`tool-ws error: ${e.message}\n`);
    });

    await exited;

    await sleep(backoff);
    backoff = Math.min(backoff * 2, RECONNECT_MAX_MS);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((res) => setTimeout(res, ms));
}

main().catch((e: unknown) => {
  const msg = e instanceof Error ? e.message : String(e);
  process.stderr.write(`tool-ws sidecar fatal: ${msg}\n`);
  process.exit(1);
});
