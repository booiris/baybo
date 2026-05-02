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

/** Result of resolving the Lark conversation an MCP tool call should
 * run against. The `ok` variant carries the full conversation
 * context — channel (bot) + Feishu chat id + platform user id. Tools
 * that look up chat-bound resources (e.g. `feishu_get_chat_info`)
 * MUST use the resolved `chatId` rather than accepting a chat id
 * from the LLM-supplied tool args; otherwise a paired user could
 * coax the agent into reading metadata for any chat the bot can
 * access (Codex review). The `ambiguous` variant remains the
 * fail-closed fallback for multi-bot deployments where the call
 * carried no `auraBotId`. */
export type LarkChannelResolution =
  | {
      kind: "ok";
      channel: lark.LarkChannel;
      chatId: string;
      platformUserId: string;
    }
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

  private resolveActive(extra: unknown):
    | {
        ok: true;
        channel: lark.LarkChannel;
        chatId: string;
        platformUserId: string;
      }
    | {
        ok: false;
        reply: { content: { type: "text"; text: string }[]; isError: true };
      } {
    const input = extractResolverInput(extra);
    const resolution = this.opts.channelResolver(input);
    if (resolution.kind === "none") {
      return {
        ok: false,
        reply: toolError(
          "no active Lark conversation for this session; the agent must have processed at least one inbound Feishu message before this tool can run",
        ),
      };
    }
    if (resolution.kind === "ambiguous") {
      return {
        ok: false,
        reply: toolError(
          `multi-bot routing requires an \`auraBotId\` on the call (${resolution.bot_count} bots live); none was provided and we will not silently pick one`,
        ),
      };
    }
    return {
      ok: true,
      channel: resolution.channel,
      chatId: resolution.chatId,
      platformUserId: resolution.platformUserId,
    };
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
        title: "Get info on the current Feishu chat",
        description:
          "Look up basic metadata (name, type, owner, member count) for the Feishu chat the user is currently messaging the agent in. Useful for grounding the agent in conversation context (e.g. \"this is a 12-person engineering group\" vs a 1:1 DM). Does NOT accept an arbitrary chat id — the lookup is bound to the active conversation so a paired user can't drive the bot to disclose metadata for any chat it happens to belong to.",
        inputSchema: {},
      },
      async (_args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        const { channel, chatId } = active;
        try {
          const info = await channel.getChatInfo(chatId);
          return {
            content: [{ type: "text", text: JSON.stringify(info, null, 2) }],
          };
        } catch (err) {
          this.opts.logger.debug(
            `feishu_get_chat_info failed for chat=${chatId}: ${String(err)}`,
          );
          return toolError(err instanceof Error ? err.message : String(err));
        }
      },
    );

    server.registerTool(
      "feishu_list_chat_members",
      {
        title: "List members of the current Feishu chat",
        description:
          "Return members of the Feishu chat the user is currently messaging the agent in: name + user id per member, plus a cursor for pagination. Useful for grounding the agent in who's in the room (group composition, attribution). The Feishu API excludes bots from the result by contract. Bound to the active conversation — does NOT accept an arbitrary chat id (Codex review).",
        inputSchema: {
          member_id_type: z
            .enum(["open_id", "union_id", "user_id"])
            .optional()
            .describe(
              "Which user-id flavour to return per member. Defaults to open_id, matching the suffix in `_meta.auraUserId`.",
            ),
          page_size: z
            .number()
            .int()
            .min(1)
            .max(100)
            .optional()
            .describe("Page size, default 20, hard cap 100."),
          page_token: z
            .string()
            .optional()
            .describe(
              "Cursor from a prior call's `page_token`. Omit on the first call; `has_more=false` means you've drained the list.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        const { channel, chatId } = active;
        try {
          const res = await channel.rawClient.im.v1.chatMembers.get({
            path: { chat_id: chatId },
            params: {
              member_id_type: args.member_id_type ?? "open_id",
              ...(args.page_size !== undefined && { page_size: args.page_size }),
              ...(args.page_token !== undefined && {
                page_token: args.page_token,
              }),
            },
          });
          if (typeof res.code === "number" && res.code !== 0) {
            return toolError(
              `Feishu API error ${res.code}: ${res.msg ?? "unknown"}`,
            );
          }
          return {
            content: [
              { type: "text", text: JSON.stringify(res.data ?? {}, null, 2) },
            ],
          };
        } catch (err) {
          this.opts.logger.debug(
            `feishu_list_chat_members failed for chat=${chatId}: ${String(err)}`,
          );
          return toolError(err instanceof Error ? err.message : String(err));
        }
      },
    );

    server.registerTool(
      "feishu_get_chat_history",
      {
        title: "Read recent messages from the current Feishu chat",
        description:
          "Return up to `limit` recent messages from the Feishu chat the user is currently messaging the agent in. The bot must already be a member of the chat. Bound to the active conversation — does NOT accept an arbitrary chat id, so a paired user can't coax the agent to read history from another chat the bot happens to be in. Returns messages in raw Feishu shape (msg_type + body.content as a JSON string per type); the agent should parse content based on msg_type. For long histories, drive pagination via `page_token` from the previous response.",
        inputSchema: {
          limit: z
            .number()
            .int()
            .min(1)
            .max(50)
            .optional()
            .describe(
              "Cap on messages returned this call. Default 20, hard cap 50 to keep payload bounded — paginate via `page_token` for more.",
            ),
          sort_type: z
            .enum(["ByCreateTimeAsc", "ByCreateTimeDesc"])
            .optional()
            .describe(
              "Order. Defaults to `ByCreateTimeDesc` (newest first) — usually what you want for context grounding.",
            ),
          page_token: z
            .string()
            .optional()
            .describe(
              "Cursor from a prior call's `page_token`. Omit on first call.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        const { channel, chatId } = active;
        try {
          const res = await channel.rawClient.im.v1.message.list({
            params: {
              container_id_type: "chat",
              container_id: chatId,
              sort_type: args.sort_type ?? "ByCreateTimeDesc",
              page_size: args.limit ?? 20,
              ...(args.page_token !== undefined && {
                page_token: args.page_token,
              }),
            },
          });
          if (typeof res.code === "number" && res.code !== 0) {
            return toolError(
              `Feishu API error ${res.code}: ${res.msg ?? "unknown"}`,
            );
          }
          return {
            content: [
              { type: "text", text: JSON.stringify(res.data ?? {}, null, 2) },
            ],
          };
        } catch (err) {
          this.opts.logger.debug(
            `feishu_get_chat_history failed for chat=${chatId}: ${String(err)}`,
          );
          return toolError(err instanceof Error ? err.message : String(err));
        }
      },
    );

    this.registerAskUser(server);
  }

  private registerAskUser(server: McpServer): void {
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
