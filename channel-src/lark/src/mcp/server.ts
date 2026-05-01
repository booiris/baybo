import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";

import type { Logger, McpReplyHandle } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

import { TunnelMcpTransport } from "./transport.js";

/** Aura-internal `_meta` keys forwarded on every `tools/call`.
 *
 * - `auraBotId`: bot tenant id (e.g. Lark `app_id`) the originating
 *   user reached us through. Set on inbound sidecar messages from
 *   slice 2F-a; lets multi-bot deployments route the call to the
 *   right tenant directly.
 * - `auraSessionId`: Aura session id, mostly for traceability. Could
 *   feed a sidecar-side session→bot lookup if we ever need one.
 * - `auraUserId`: composed Aura user id
 *   (`<channel>_<bot>_<chatKey>_<userId>`). Tools that need to reach
 *   back into the platform conversation (e.g. `feishu_ask_user`)
 *   look this up against a sidecar-local cache that
 *   `LarkPlatform.dispatchInbound` populates from each inbound
 *   message.
 *
 * All optional — TUI / HTTP sessions don't ride a sidecar bot, so
 * the resolver still needs the slice 2A fallback (single-bot `ok`,
 * multi-bot `ambiguous`). */
export interface LarkResolverInput {
  auraBotId?: string;
  auraSessionId?: string;
  auraUserId?: string;
}

/** Result of resolving which `LarkChannel` an MCP tool call should
 * run against. The `ambiguous` variant is the fail-closed fallback
 * when no `auraBotId` is supplied AND multiple bots are connected;
 * it intentionally rejects the call rather than silently routing
 * through whichever bot we picked first (cross-tenant leak). */
export type LarkChannelResolution =
  | { kind: "ok"; channel: lark.LarkChannel }
  | { kind: "none" }
  | { kind: "ambiguous"; bot_count: number };

export type LarkChannelResolver = (
  input: LarkResolverInput,
) => LarkChannelResolution;

export interface LarkAskUserResult {
  kind: "ok" | "no_context" | "timeout";
  text?: string;
}

export type LarkAskUserHandler = (
  input: LarkResolverInput,
  prompt: string,
  timeoutMs: number,
) => Promise<LarkAskUserResult>;

export interface LarkMcpServerOpts {
  logger: Logger;
  channelResolver: LarkChannelResolver;
  /**
   * Implementation of `feishu_ask_user`. Sends the prompt into the
   * Lark conversation associated with `_meta.auraUserId` and waits
   * for the user's reply (or a timeout). Splits out so this module
   * doesn't reach into platform-side state directly. Optional —
   * tests that don't exercise ask-user can omit it; the default
   * surfaces `no_context` so the tool still returns a clean error.
   */
  askUser?: LarkAskUserHandler;
}

interface TunnelSession {
  server: McpServer;
  transport: TunnelMcpTransport;
}

/**
 * Per-(channel, sidecar) MCP host. One sidecar process exposes a
 * single `LarkMcpServer` instance; the agent side opens one tunnel
 * per concurrent MCP session and JSON-RPC envelopes route through
 * `accept(tunnelId, payload, reply)`.
 *
 * Every new `tunnel_id` spins up a fresh `McpServer` + transport
 * pair. The tunnels are independent: tearing down one (sidecar
 * disconnect, agent-side close) doesn't disturb the others. Today
 * teardown happens lazily — the gateway's `drain_for_channel` drops
 * receivers on disconnect; sidecar restart loses any in-flight state
 * which is the right behaviour for stateless tools.
 */
export class LarkMcpServer {
  private readonly tunnels = new Map<string, TunnelSession>();

  constructor(private readonly opts: LarkMcpServerOpts) {}

  /**
   * Route one inbound envelope to its tunnel session. First envelope
   * for a `tunnel_id` creates the session (server + transport); each
   * subsequent envelope feeds the existing transport.
   */
  async accept(
    tunnelId: string,
    payload: Uint8Array,
    reply: McpReplyHandle,
  ): Promise<void> {
    let session = this.tunnels.get(tunnelId);
    if (!session) {
      session = await this.openSession(reply);
      this.tunnels.set(tunnelId, session);
    }
    session.transport.feed(payload);
  }

  /** Drop all tunnel sessions. Called on `stopBot` / `onStop` so a
   * shutdown doesn't leak server instances. The transports' `close`
   * fires the SDK's onclose callback so any pending request handlers
   * unwind cleanly. */
  async shutdown(): Promise<void> {
    const sessions = [...this.tunnels.values()];
    this.tunnels.clear();
    for (const s of sessions) {
      await s.server.close().catch(() => undefined);
      await s.transport.close().catch(() => undefined);
    }
  }

  private async openSession(reply: McpReplyHandle): Promise<TunnelSession> {
    const transport = new TunnelMcpTransport(reply);
    const server = new McpServer(
      { name: "aura-lark", version: "0.1.0" },
      { capabilities: { tools: {} } },
    );
    this.registerTools(server);
    await server.connect(transport);
    return { server, transport };
  }

  private registerTools(server: McpServer): void {
    server.registerTool(
      "feishu_get_chat_info",
      {
        title: "Get Feishu chat info",
        description:
          "Look up basic metadata (name, type, owner, member count) for a Lark / Feishu chat by its `chatId`. The bot must already be a member of the chat — Feishu rejects lookups across the bot's tenant boundary.",
        inputSchema: {
          chat_id: z
            .string()
            .min(1)
            .describe("Feishu chat id (e.g. `oc_xxx` for groups, `p2p_xxx` for DMs)"),
        },
      },
      async (args, extra) => {
        const input = extractResolverInput(extra);
        const resolution = this.opts.channelResolver(input);
        if (resolution.kind === "none") {
          return toolError(
            "no Lark bot is currently connected; register one with `aura channel add lark` and retry",
          );
        }
        if (resolution.kind === "ambiguous") {
          return toolError(
            `multi-bot routing requires an \`auraBotId\` on the call (${resolution.bot_count} bots live); none was provided and we will not silently pick one`,
          );
        }
        const channel = resolution.channel;
        try {
          const info = await channel.getChatInfo(args.chat_id);
          return {
            content: [
              {
                type: "text",
                text: JSON.stringify(info, null, 2),
              },
            ],
          };
        } catch (err) {
          this.opts.logger.debug(
            `feishu_get_chat_info failed for chat=${args.chat_id}: ${String(err)}`,
          );
          return toolError(
            err instanceof Error ? err.message : String(err),
          );
        }
      },
    );

    server.registerTool(
      "feishu_ask_user",
      {
        title: "Ask the user a follow-up question",
        description:
          "Send a question into the same Feishu chat the user reached you through, then wait up to `timeout_seconds` for their next message and return its text. Use sparingly — only when the user must clarify something the agent cannot infer or look up. Times out cleanly if the user doesn't reply, in which case you should fall back to a best-effort answer instead of asking again.",
        inputSchema: {
          prompt: z
            .string()
            .min(1)
            .describe(
              "The question to send. Be concise — Feishu chats are conversational, so favour a single sentence over a multi-paragraph wall.",
            ),
          timeout_seconds: z
            .number()
            .int()
            .min(5)
            .max(600)
            .optional()
            .describe(
              "Seconds to wait for a reply before timing out. Defaults to 300 (5 minutes); cap is 600 (10 minutes). The cap MUST stay strictly less than the agent's own sidecar-MCP timeout (660s) — if the agent gave up first, this tool's pending waiter would consume the user's late reply as a stale answer and silently drop it from the conversation (Codex review).",
            ),
        },
      },
      async (args, extra) => {
        const input = extractResolverInput(extra);
        const timeoutMs = (args.timeout_seconds ?? 300) * 1000;
        const askUser =
          this.opts.askUser ??
          (async () => ({ kind: "no_context" as const }));
        const result = await askUser(input, args.prompt, timeoutMs);
        if (result.kind === "no_context") {
          return toolError(
            "feishu_ask_user has no Lark conversation to send the prompt into for this session — the agent must have processed at least one inbound Feishu message before it can ask back",
          );
        }
        if (result.kind === "timeout") {
          return toolError(
            `feishu_ask_user timed out after ${args.timeout_seconds ?? 300}s with no reply; proceed with a best-effort answer instead of retrying`,
          );
        }
        return {
          content: [{ type: "text", text: result.text ?? "" }],
        };
      },
    );
  }
}

function extractResolverInput(extra: unknown): LarkResolverInput {
  const meta = (extra as { _meta?: unknown } | undefined)?._meta;
  const m = meta as
    | { auraBotId?: unknown; auraSessionId?: unknown; auraUserId?: unknown }
    | undefined;
  const input: LarkResolverInput = {};
  if (typeof m?.auraBotId === "string") input.auraBotId = m.auraBotId;
  if (typeof m?.auraSessionId === "string") input.auraSessionId = m.auraSessionId;
  if (typeof m?.auraUserId === "string") input.auraUserId = m.auraUserId;
  return input;
}

function toolError(message: string): {
  content: { type: "text"; text: string }[];
  isError: true;
} {
  return {
    content: [{ type: "text", text: message }],
    isError: true,
  };
}
