#!/usr/bin/env node
import { homedir } from "node:os";
import { join } from "node:path";

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  type CallToolResult,
  ListToolsRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";

import { BrowserManager } from "./manager.js";
import { buildHandlers } from "./handlers.js";
import { TOOLS } from "./tools.js";

const SERVER_NAME = "browser";
const SERVER_VERSION = "0.1.0";

interface Env {
  profileDir: string;
  chromiumExecutable: string | undefined;
  noSandbox: boolean;
  extraArgs: readonly string[];
  allowLoopback: boolean;
}

function readEnv(): Env {
  const profileDir =
    process.env["AURA_BROWSER_PROFILE_DIR"] ??
    process.env["AURA_PROFILE_DIR"] ??
    defaultProfileDir();
  const chromiumExecutable = process.env["AURA_CHROMIUM_BIN"];
  const noSandbox = parseBoolEnv(process.env["AURA_BROWSER_NO_SANDBOX"]);
  const extraArgs = parseArgsEnv(process.env["AURA_BROWSER_ARGS"]);
  // Test-only escape hatch: admits 127.0.0.0/8 + ::1 so smoke tests
  // can drive a real Chromium against a local HTTP server. Production
  // never sets this.
  const allowLoopback = parseBoolEnv(process.env["AURA_BROWSER_ALLOW_LOOPBACK"]);
  return {
    profileDir,
    chromiumExecutable,
    noSandbox,
    extraArgs,
    allowLoopback,
  };
}

function parseBoolEnv(s: string | undefined): boolean {
  if (!s) return false;
  const v = s.trim().toLowerCase();
  return v === "1" || v === "true" || v === "yes" || v === "on";
}

function parseArgsEnv(raw: string | undefined): readonly string[] {
  if (!raw) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    process.stderr.write(
      `[browser-mcp] AURA_BROWSER_ARGS is not valid JSON, ignoring: ${
        e instanceof Error ? e.message : String(e)
      }\n`,
    );
    return [];
  }
  if (!Array.isArray(parsed) || !parsed.every((v) => typeof v === "string")) {
    process.stderr.write(
      `[browser-mcp] AURA_BROWSER_ARGS must be a JSON array of strings, ignoring\n`,
    );
    return [];
  }
  return parsed as readonly string[];
}

function defaultProfileDir(): string {
  const xdg = process.env["XDG_CACHE_HOME"];
  const root = xdg && xdg.length > 0 ? xdg : join(homedir(), ".cache");
  return join(root, "aura", "browser", "profile");
}

async function main(): Promise<void> {
  const env = readEnv();

  // Stderr-only logging: the gateway pipes stderr into its tracing
  // log_buffer, while stdout carries the MCP framed protocol — any
  // stray println on stdout corrupts a JSON-RPC frame and kills the
  // session.
  const log = (msg: string): void => {
    process.stderr.write(`[browser-mcp] ${msg}\n`);
  };

  const manager = new BrowserManager({
    profileDir: env.profileDir,
    chromiumExecutable: env.chromiumExecutable,
    noSandbox: env.noSandbox,
    extraArgs: env.extraArgs,
    allowLoopback: env.allowLoopback,
    // Without the WS transport's outbound channel, embedded MCP servers
    // have no clean way to push unsolicited "context_closed" events
    // back to the gateway. The downstream Rust side only logged them
    // anyway, so we drop the upcall: idle GC still reaps Playwright
    // contexts on the sidecar side; the gateway just doesn't get an
    // explicit notification.
    emitEvent: () => {},
  });

  const handlers = buildHandlers(manager);

  const server = new Server(
    { name: SERVER_NAME, version: SERVER_VERSION },
    { capabilities: { tools: {} } },
  );

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: TOOLS.map((t) => ({
      name: t.name,
      description: t.description,
      inputSchema: t.inputSchema,
      _meta: t._meta,
    })),
  }));

  server.setRequestHandler(CallToolRequestSchema, async (req): Promise<CallToolResult> => {
    const handler = handlers[req.params.name];
    if (!handler) {
      return {
        content: [{ type: "text", text: `unknown tool '${req.params.name}'` }],
        isError: true,
      };
    }
    const args = (req.params.arguments ?? {}) as Record<string, unknown>;
    if (typeof args["context_id"] !== "string" || args["context_id"].length === 0) {
      return {
        content: [
          {
            type: "text",
            text: "missing context_id — Aura's MCP client should auto-inject this; non-Aura clients must pass it explicitly",
          },
        ],
        isError: true,
      };
    }
    try {
      const out = await handler(args as Record<string, unknown> & { context_id: string });
      // The MCP SDK's `CallToolResult` is a structural superset of our
      // internal `CallResult`; the cast is a no-op at runtime.
      return out as CallToolResult;
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      const code = e instanceof Error && e.name ? e.name : "EXECUTION_ERROR";
      return {
        content: [{ type: "text", text: `${code}: ${msg}` }],
        isError: true,
      };
    }
  });

  const shutdown = async (): Promise<void> => {
    log("shutting down");
    await manager.shutdown();
    await server.close().catch(() => undefined);
    process.exit(0);
  };
  process.on("SIGINT", () => void shutdown());
  process.on("SIGTERM", () => void shutdown());

  const transport = new StdioServerTransport();
  await server.connect(transport);
  log(`mcp server '${SERVER_NAME}' v${SERVER_VERSION} ready (${TOOLS.length} tools)`);
}

main().catch((e: unknown) => {
  const msg = e instanceof Error ? e.message : String(e);
  process.stderr.write(`[browser-mcp] fatal: ${msg}\n`);
  process.exit(1);
});
