import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";

import type { Logger, McpReplyHandle } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

import { TunnelMcpTransport } from "./transport.js";

/** Resolves a live `LarkChannel` instance the tools should run against.
 * Today the resolver returns whichever bot is registered first; slice
 * 2B routes by `_meta.auraSessionId` once the agent-side rmcp transport
 * is in place. Returns `null` when no bot is connected — the tool
 * surfaces the error to the agent in that case. */
export type LarkChannelResolver = () => lark.LarkChannel | null;

export interface LarkMcpServerOpts {
  logger: Logger;
  channelResolver: LarkChannelResolver;
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
      async (args) => {
        const channel = this.opts.channelResolver();
        if (!channel) {
          return toolError(
            "no Lark bot is currently connected; register one with `aura channel add lark` and retry",
          );
        }
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
  }
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
