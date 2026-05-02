import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";

import type { Logger, McpReplyHandle } from "@aura/channel-sdk";
import * as lark from "@larksuiteoapi/node-sdk";

import type { UATAccessor } from "../auth/auto-auth.js";
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
      /** Per-bot UAT accessor. Optional because tenant-token tools
       * (chat info, members, message history) don't need it; UAT
       * tools (calendar, bitable, …) refuse to run when it's absent
       * and surface a configuration error to the agent. Wired by
       * `LarkPlatform` once the SDK's `secrets()` capability has
       * been negotiated. */
      uatAccessor?: UATAccessor;
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

export interface LarkWriteApprovalRequest {
  toolName: string;
  /** Plain-English one-liner of what the tool wants to do — feeds
   * the approval card body. Build at the call site, the platform
   * doesn't synthesise it. */
  summary: string;
  /** Optional richer block (e.g. listing the records that will be
   * created). The card builder fences it as code. */
  detail?: string;
}

export type LarkWriteApprovalResult =
  | { kind: "approved" }
  | { kind: "denied" }
  | { kind: "timeout" }
  | { kind: "no_context" };

export type LarkWriteApprovalHandler = (
  input: LarkResolverInput,
  request: LarkWriteApprovalRequest,
  timeoutMs: number,
) => Promise<LarkWriteApprovalResult>;

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
  /**
   * Per-call write-approval gate for destructive MCP write tools.
   * Sends an interactive [Approve] [Deny] card into the active
   * Lark conversation, waits for the user's click (or timeout).
   *
   * Tools that mutate state (delete event, overwrite doc, batch
   * create records) MUST gate behind this — without it, the OAuth
   * grant lets the agent execute any write the user has scope for,
   * which violates the user's intent (they authorised "use my Lark
   * surfaces", not "every write the LLM hallucinates is fine").
   *
   * Optional: tests can omit it; tools that hit `approveWrite`
   * without a configured handler treat it as `no_context` and
   * refuse to execute (fail closed — never silently bypass).
   */
  approveWrite?: LarkWriteApprovalHandler;
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

  /** Gate a destructive MCP write tool behind an in-chat approval
   * card. Returns `null` when the user approved (caller proceeds);
   * otherwise returns a rendered tool error explaining the outcome.
   *
   * Default timeout 5 minutes — wider than `feishu_ask_user`'s 5min
   * default because approving a write is a heavier decision than
   * answering a clarification.
   *
   * If the platform didn't wire `approveWrite`, this fails closed
   * with a clear message — the tool MUST NOT proceed. */
  private async gateWrite(
    extra: unknown,
    request: LarkWriteApprovalRequest,
    timeoutMs = 300_000,
  ): Promise<{ content: { type: "text"; text: string }[]; isError: true } | null> {
    if (!this.opts.approveWrite) {
      return toolError(
        `${request.toolName} requires an in-chat approval gate but the platform didn't configure one — refusing to write blind`,
      );
    }
    const input = extractResolverInput(extra);
    const result = await this.opts.approveWrite(input, request, timeoutMs);
    if (result.kind === "approved") return null;
    if (result.kind === "denied") {
      return toolError(
        `${request.toolName}: the user denied the write — do NOT retry; ask them to clarify what they want differently before trying anything else`,
      );
    }
    if (result.kind === "timeout") {
      return toolError(
        `${request.toolName}: approval card timed out after ${Math.round(timeoutMs / 1000)}s with no response — fall back to a non-destructive alternative or ask the user again with a clearer summary`,
      );
    }
    return toolError(
      `${request.toolName}: no Lark conversation context for the approval card; the agent must have processed at least one inbound Feishu message before a write tool can run`,
    );
  }

  /** Run a UAT-requiring Lark API call through the accessor + render
   * the result as an MCP tool reply.
   *
   * Compresses the boilerplate that every UAT tool needs: resolve
   * accessor → invoke → distinguish auth_failed from API errors →
   * render data as JSON. The handler returns the raw Lark response
   * `{ code, msg, data }`; this method pulls `data` and serialises
   * it, surfacing a non-zero `code` as a structured tool error. */
  private async runUatTool<
    T extends {
      code?: number | undefined;
      msg?: string | undefined;
      data?: unknown;
    },
  >(args: {
    active: {
      channel: lark.LarkChannel;
      chatId: string;
      platformUserId: string;
      uatAccessor?: UATAccessor;
    };
    toolName: string;
    reason: string;
    handler: (uat: string) => Promise<T>;
  }): Promise<{
    content: { type: "text"; text: string }[];
    isError?: true;
  }> {
    const { active, toolName, reason, handler } = args;
    if (!active.uatAccessor) {
      return toolError(
        `${toolName} requires UAT but the bot's OAuth pipeline isn't configured for this session`,
      );
    }
    try {
      const result = await active.uatAccessor.invoke(
        {
          userOpenId: active.platformUserId,
          chatId: active.chatId,
          reason,
        },
        handler,
      );
      if (result.kind !== "ok") {
        return toolError(authFailedMessage(toolName, result.outcome));
      }
      const res = result.result;
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
      this.opts.logger.debug(`${toolName} failed: ${String(err)}`);
      return toolError(err instanceof Error ? err.message : String(err));
    }
  }

  private resolveActive(extra: unknown):
    | {
        ok: true;
        channel: lark.LarkChannel;
        chatId: string;
        platformUserId: string;
        uatAccessor?: UATAccessor;
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
      ...(resolution.uatAccessor !== undefined && {
        uatAccessor: resolution.uatAccessor,
      }),
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

    server.registerTool(
      "feishu_search_chats",
      {
        title: "Search chats the bot is a member of",
        description:
          "Search the chats this bot can see (chats it's joined plus any public chats explicitly visible to it) by free-text query against chat name. Useful for the agent to discover its own surface — e.g. \"is there an existing #incidents chat I should post to?\". Returns chat_id, name, description, owner. Bot-scoped, NOT chat-scoped: routes to the bot inferred from `_meta.auraBotId` (or the only one in single-bot mode). Empty `query` lists all visible chats.",
        inputSchema: {
          query: z
            .string()
            .optional()
            .describe(
              "Free-text query matched against chat name (fuzzy, multilingual). Omit to list every visible chat.",
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
            .describe("Cursor from a prior call's `page_token`. Omit on first call."),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        const { channel } = active;
        try {
          const res = await channel.rawClient.im.v1.chat.search({
            params: {
              ...(args.query !== undefined && { query: args.query }),
              page_size: args.page_size ?? 20,
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
          this.opts.logger.debug(`feishu_search_chats failed: ${String(err)}`);
          return toolError(err instanceof Error ? err.message : String(err));
        }
      },
    );

    server.registerTool(
      "feishu_get_message",
      {
        title: "Fetch a single Feishu message by id",
        description:
          "Look up one Feishu message by `message_id`. Useful for resolving message references the agent sees in chat history (\"see om_xxx above\"). Bound to the active conversation: even though Feishu's API would let the bot read any message in any chat it belongs to, this tool rejects messages whose `chat_id` doesn't match the resolver's chat — otherwise a paired user could coax the agent into reading messages from other chats the bot happens to be in (mirrors the `feishu_get_chat_info` Codex #2 invariant).",
        inputSchema: {
          message_id: z
            .string()
            .min(1)
            .describe(
              "The Feishu message id (e.g. `om_xxx`). Usually pulled from a `[message_id=om_xxx]` marker in chat history.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        const { channel, chatId } = active;
        try {
          const res = await channel.rawClient.im.v1.message.get({
            path: { message_id: args.message_id },
          });
          if (typeof res.code === "number" && res.code !== 0) {
            return toolError(
              `Feishu API error ${res.code}: ${res.msg ?? "unknown"}`,
            );
          }
          const items = res.data?.items ?? [];
          const msg = items[0];
          if (!msg) {
            return toolError(
              `feishu_get_message: no message found for id ${args.message_id}`,
            );
          }
          if (msg.chat_id && msg.chat_id !== chatId) {
            return toolError(
              `feishu_get_message: message ${args.message_id} belongs to a different chat than the active conversation; refusing to leak cross-chat content`,
            );
          }
          return {
            content: [{ type: "text", text: JSON.stringify(msg, null, 2) }],
          };
        } catch (err) {
          this.opts.logger.debug(
            `feishu_get_message failed for id=${args.message_id}: ${String(err)}`,
          );
          return toolError(err instanceof Error ? err.message : String(err));
        }
      },
    );

    server.registerTool(
      "feishu_who_am_i",
      {
        title: "Identify the current user via OAuth",
        description:
          "Return the Feishu profile (name, avatar, email, open_id) of the user the agent is currently messaging. The first call in a fresh session prompts the user to authorize Aura via a chat card; later calls reuse the stored token. Useful for grounding the agent in the user's identity before generating personalised replies. Bound to the user driving the conversation — does NOT accept a target user_id, so the LLM can't probe other users' profiles.",
        inputSchema: {},
      },
      async (_args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_who_am_i",
          reason: "look up your Feishu profile",
          handler: (uat) =>
            active.channel.rawClient.authen.userInfo.get(
              {},
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_get_user",
      {
        title: "Get a Feishu user by id",
        description:
          "Look up a Feishu user by `user_id`. Returns name, email, mobile, department, status, etc. Acts under the conversing user's UAT (so visibility respects their org-chart scope, not the bot's app-level permissions). For the user themselves, prefer `feishu_who_am_i` — it doesn't require knowing the id.",
        inputSchema: {
          user_id: z
            .string()
            .min(1)
            .describe(
              "The target user id. Format depends on `user_id_type` (default open_id, e.g. `ou_…`).",
            ),
          user_id_type: z
            .enum(["open_id", "union_id", "user_id"])
            .optional()
            .describe(
              "Which id flavour `user_id` is. Defaults to `open_id`, matching what shows up everywhere else (chat members, message senders, _meta.auraUserId suffix).",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_get_user",
          reason: "look up that Feishu user's profile",
          handler: (uat) =>
            active.channel.rawClient.contact.v3.user.get(
              {
                path: { user_id: args.user_id },
                params: { user_id_type: args.user_id_type ?? "open_id" },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_search_user",
      {
        title: "Search Feishu users by keyword",
        description:
          "Search Feishu users (employees) by free-text query against name, with results ranked by org-chart proximity to the conversing user. Useful when the agent has a person's name but needs their `open_id` to mention them, send a card, or query their calendar/profile. Acts under the conversing user's UAT — visibility respects their org-chart scope. The Feishu open API for this endpoint isn't exposed in the SDK's typed surface, so the call goes via raw fetch against `${baseUrl}/open-apis/search/v1/user`.",
        inputSchema: {
          query: z
            .string()
            .min(1)
            .describe(
              "Required. Matched against name (multilingual, fuzzy, supports pinyin / prefix).",
            ),
          page_size: z
            .number()
            .int()
            .min(1)
            .max(200)
            .optional()
            .describe("Default 20, hard cap 200."),
          page_token: z
            .string()
            .optional()
            .describe(
              "Cursor from a prior call's `page_token`. Omit on the first call.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_search_user",
          reason: "search for that Feishu user",
          handler: async (uat) => {
            const params = new URLSearchParams({
              query: args.query,
              page_size: String(args.page_size ?? 20),
            });
            if (args.page_token !== undefined) {
              params.set("page_token", args.page_token);
            }
            const url = `${active.uatAccessor!.baseUrl}/open-apis/search/v1/user?${params}`;
            const resp = await fetch(url, {
              headers: { Authorization: `Bearer ${uat}` },
            });
            return (await resp.json()) as {
              code?: number;
              msg?: string;
              data?: unknown;
            };
          },
        });
      },
    );

    server.registerTool(
      "feishu_calendar",
      {
        title: "Read Feishu calendar metadata",
        description:
          "Inspect the conversing user's Feishu calendars. `action: \"primary\"` returns their primary calendar; `\"list\"` paginates all visible calendars (own + shared); `\"get\"` fetches one by id. Read-only — calendar mutation lives in `feishu_calendar_event`.",
        inputSchema: {
          action: z
            .enum(["primary", "list", "get"])
            .describe(
              "`primary` returns the user's main calendar (no other args). `list` paginates everything they can see (page_size up to 1000, default 50; pass `page_token` for the next page). `get` fetches a single calendar by `calendar_id`.",
            ),
          calendar_id: z
            .string()
            .optional()
            .describe("Required for `get`. Calendar id from a prior `list`/`primary` response."),
          page_size: z
            .number()
            .int()
            .min(1)
            .max(1000)
            .optional()
            .describe("Only used by `list`. Default 50, hard cap 1000."),
          page_token: z
            .string()
            .optional()
            .describe("Only used by `list`. Cursor from a prior call."),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (args.action === "get" && !args.calendar_id) {
          return toolError("feishu_calendar action=get requires `calendar_id`");
        }
        return this.runUatTool({
          active,
          toolName: "feishu_calendar",
          reason: "read your Feishu calendar metadata",
          handler: async (uat): Promise<LarkApiResponse> => {
            const opts = lark.withUserAccessToken(uat);
            const sdk = active.channel.rawClient.calendar;
            if (args.action === "primary") return sdk.calendar.primary({}, opts);
            if (args.action === "get") {
              return sdk.calendar.get(
                { path: { calendar_id: args.calendar_id! } },
                opts,
              );
            }
            return sdk.calendar.list(
              {
                params: {
                  ...(args.page_size !== undefined && { page_size: args.page_size }),
                  ...(args.page_token !== undefined && { page_token: args.page_token }),
                },
              },
              opts,
            );
          },
        });
      },
    );

    server.registerTool(
      "feishu_freebusy",
      {
        title: "Query Feishu calendar free/busy",
        description:
          "Look up free/busy windows in a time range for one user (or one room). When `user_id` is omitted, queries the conversing user's primary calendar. Use `feishu_freebusy_batch` for multi-user scheduling questions. Returns `freebusy_items` (`{ start_time, end_time }`); empty list means the window is free.",
        inputSchema: {
          time_min: z
            .string()
            .describe("ISO 8601 lower bound, inclusive (e.g. `2026-05-02T09:00:00+08:00`)."),
          time_max: z
            .string()
            .describe("ISO 8601 upper bound, exclusive."),
          user_id: z
            .string()
            .optional()
            .describe("Target user. Omit to query the conversing user. Mutually exclusive with `room_id`."),
          room_id: z
            .string()
            .optional()
            .describe("Room id to query a meeting room's busy windows. Mutually exclusive with `user_id`."),
          user_id_type: z
            .enum(["open_id", "union_id", "user_id"])
            .optional()
            .describe("Default open_id."),
          only_busy: z
            .boolean()
            .optional()
            .describe("If true, omit calendar entry titles — only return time ranges. Default false (titles included if you have read access)."),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (args.user_id && args.room_id) {
          return toolError("feishu_freebusy: user_id and room_id are mutually exclusive");
        }
        return this.runUatTool({
          active,
          toolName: "feishu_freebusy",
          reason: "check calendar free/busy",
          handler: (uat) =>
            active.channel.rawClient.calendar.freebusy.list(
              {
                data: {
                  time_min: args.time_min,
                  time_max: args.time_max,
                  ...(args.user_id !== undefined && { user_id: args.user_id }),
                  ...(args.room_id !== undefined && { room_id: args.room_id }),
                  ...(args.only_busy !== undefined && { only_busy: args.only_busy }),
                },
                params: {
                  user_id_type: args.user_id_type ?? "open_id",
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_freebusy_batch",
      {
        title: "Query Feishu calendar free/busy for multiple users",
        description:
          "Multi-user variant of `feishu_freebusy`. Useful for finding common free time across a group. Returns one `freebusy_lists` entry per `user_id`.",
        inputSchema: {
          time_min: z.string().describe("ISO 8601 lower bound, inclusive."),
          time_max: z.string().describe("ISO 8601 upper bound, exclusive."),
          user_ids: z
            .array(z.string().min(1))
            .min(1)
            .max(50)
            .describe("List of user ids to query (1-50)."),
          user_id_type: z
            .enum(["open_id", "union_id", "user_id"])
            .optional()
            .describe("Default open_id."),
          only_busy: z.boolean().optional(),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_freebusy_batch",
          reason: "check group calendar free/busy",
          handler: (uat) =>
            active.channel.rawClient.calendar.freebusy.batch(
              {
                data: {
                  time_min: args.time_min,
                  time_max: args.time_max,
                  user_ids: args.user_ids,
                  ...(args.only_busy !== undefined && { only_busy: args.only_busy }),
                },
                params: {
                  user_id_type: args.user_id_type ?? "open_id",
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_calendar_event",
      {
        title: "Read Feishu calendar events",
        description:
          "Inspect events in a Feishu calendar. `action: \"list\"` paginates events in a time range; `\"get\"` fetches one by id. Read-only — event mutation lives in upcoming calendar_event_create / _update / _delete tools. Returns raw Feishu event shape (start_time/end_time as `{ date | timestamp, timezone }`).",
        inputSchema: {
          action: z
            .enum(["list", "get"])
            .describe(
              "`list` paginates events in a calendar (filterable by time range). `get` fetches one event.",
            ),
          calendar_id: z
            .string()
            .min(1)
            .describe(
              "Required. Calendar id from `feishu_calendar`. Use \"primary\" via `feishu_calendar action=primary` first if you don't have a specific id.",
            ),
          event_id: z
            .string()
            .optional()
            .describe("Required for `get`. Event id from a prior `list`."),
          start_time: z
            .string()
            .optional()
            .describe(
              "Only used by `list`. ISO 8601 lower bound (events overlapping this window are returned).",
            ),
          end_time: z
            .string()
            .optional()
            .describe("Only used by `list`. ISO 8601 upper bound."),
          page_size: z
            .number()
            .int()
            .min(1)
            .max(1000)
            .optional()
            .describe("Only used by `list`. Default 50."),
          page_token: z.string().optional(),
          need_attendee: z
            .boolean()
            .optional()
            .describe(
              "Only used by `get`. If true, the response includes the attendee list (default false — keeps the payload small).",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (args.action === "get" && !args.event_id) {
          return toolError("feishu_calendar_event action=get requires `event_id`");
        }
        return this.runUatTool({
          active,
          toolName: "feishu_calendar_event",
          reason: "read your calendar events",
          handler: async (uat): Promise<LarkApiResponse> => {
            const opts = lark.withUserAccessToken(uat);
            const sdk = active.channel.rawClient.calendar.calendarEvent;
            if (args.action === "get") {
              return sdk.get(
                {
                  path: { calendar_id: args.calendar_id, event_id: args.event_id! },
                  params: {
                    ...(args.need_attendee !== undefined && { need_attendee: args.need_attendee }),
                  },
                },
                opts,
              );
            }
            return sdk.list(
              {
                path: { calendar_id: args.calendar_id },
                params: {
                  ...(args.start_time !== undefined && { start_time: args.start_time }),
                  ...(args.end_time !== undefined && { end_time: args.end_time }),
                  ...(args.page_size !== undefined && { page_size: args.page_size }),
                  ...(args.page_token !== undefined && { page_token: args.page_token }),
                },
              },
              opts,
            );
          },
        });
      },
    );

    server.registerTool(
      "feishu_wiki",
      {
        title: "Browse Feishu Wiki spaces and nodes",
        description:
          "Read the conversing user's Feishu Wiki structure. `action: \"get_space\"` fetches one wiki space by id; `\"list_nodes\"` paginates the children of a node (or the space root). Use to discover doc/sheet/bitable tokens before fetching content with `feishu_fetch_doc`.",
        inputSchema: {
          action: z.enum(["get_space", "list_nodes"]).describe("Operation."),
          space_id: z.string().min(1).describe("Required. Wiki space id."),
          parent_node_token: z
            .string()
            .optional()
            .describe(
              "Only used by `list_nodes`. If omitted, lists the space's root nodes; pass a node_token from a prior call to descend.",
            ),
          page_size: z.number().int().min(1).max(50).optional().describe("Only used by `list_nodes`. Default 10."),
          page_token: z.string().optional(),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_wiki",
          reason: "browse your Feishu Wiki",
          handler: async (uat): Promise<LarkApiResponse> => {
            const opts = lark.withUserAccessToken(uat);
            const sdk = active.channel.rawClient.wiki;
            if (args.action === "get_space") {
              return sdk.space.get(
                { path: { space_id: args.space_id } },
                opts,
              );
            }
            return sdk.spaceNode.list(
              {
                path: { space_id: args.space_id },
                params: {
                  ...(args.parent_node_token !== undefined && {
                    parent_node_token: args.parent_node_token,
                  }),
                  ...(args.page_size !== undefined && { page_size: args.page_size }),
                  ...(args.page_token !== undefined && { page_token: args.page_token }),
                },
              },
              opts,
            );
          },
        });
      },
    );

    server.registerTool(
      "feishu_search_doc",
      {
        title: "Search Feishu Docs by keyword",
        description:
          "Free-text search across the conversing user's accessible Feishu docs (docx + doc + sheet + bitable). Returns document tokens + titles. Use the returned tokens with `feishu_fetch_doc` (upcoming) to read the content. Goes through the legacy /open-apis/suite/docs-api/search/object endpoint via raw fetch — the SDK doesn't expose this in its typed surface.",
        inputSchema: {
          query: z
            .string()
            .min(1)
            .describe("Search keywords. Multilingual; matches title and body."),
          count: z
            .number()
            .int()
            .min(1)
            .max(50)
            .optional()
            .describe("How many docs to return. Default 10."),
          offset: z.number().int().min(0).optional().describe("Pagination offset for the next page."),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_search_doc",
          reason: "search your Feishu docs",
          handler: async (uat) => {
            const url = `${active.uatAccessor!.baseUrl}/open-apis/suite/docs-api/search/object`;
            const resp = await fetch(url, {
              method: "POST",
              headers: {
                Authorization: `Bearer ${uat}`,
                "Content-Type": "application/json",
              },
              body: JSON.stringify({
                search_key: args.query,
                count: args.count ?? 10,
                ...(args.offset !== undefined && { offset: args.offset }),
              }),
            });
            return (await resp.json()) as LarkApiResponse;
          },
        });
      },
    );

    server.registerTool(
      "feishu_bitable_records",
      {
        title: "List records in a Feishu Bitable table",
        description:
          "Read records from a Feishu Bitable (multi-dimensional table). Useful for querying structured data the user has set up — e.g. \"what's in my project tracker?\", \"how many open bugs in the dashboard?\". Pass `app_token` (the bitable's URL token) and `table_id`; optional `filter` / `sort` follow Feishu's bitable expression syntax (see API docs). Returns rows as `{ fields: { <name>: <value> } }` plus pagination cursors. Read-only — record CRUD is a separate tool family (deferred).",
        inputSchema: {
          app_token: z
            .string()
            .min(1)
            .describe("Bitable app token from the URL (e.g. `bascn…`)."),
          table_id: z
            .string()
            .min(1)
            .describe("Table id within the bitable (e.g. `tblxxx`)."),
          view_id: z
            .string()
            .optional()
            .describe(
              "Optional view id to inherit a view's filter/sort/field-visibility config.",
            ),
          filter: z
            .string()
            .optional()
            .describe(
              "Optional Feishu bitable filter expression (e.g. `CurrentValue.[Status]=\"Open\"`). When set, overrides the view's filter.",
            ),
          sort: z
            .string()
            .optional()
            .describe(
              "Optional sort expression (e.g. `[\"field_x ASC\", \"field_y DESC\"]` as a JSON string per Feishu spec).",
            ),
          field_names: z
            .string()
            .optional()
            .describe(
              "Optional JSON-encoded array of field names to return (e.g. `[\"Title\",\"Status\"]`). Default: all fields.",
            ),
          page_size: z
            .number()
            .int()
            .min(1)
            .max(500)
            .optional()
            .describe("Default 20, hard cap 500."),
          page_token: z.string().optional(),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_bitable_records",
          reason: "read records from your Bitable",
          handler: (uat) =>
            active.channel.rawClient.bitable.appTableRecord.list(
              {
                path: {
                  app_token: args.app_token,
                  table_id: args.table_id,
                },
                params: {
                  ...(args.view_id !== undefined && { view_id: args.view_id }),
                  ...(args.filter !== undefined && { filter: args.filter }),
                  ...(args.sort !== undefined && { sort: args.sort }),
                  ...(args.field_names !== undefined && {
                    field_names: args.field_names,
                  }),
                  ...(args.page_size !== undefined && { page_size: args.page_size }),
                  ...(args.page_token !== undefined && {
                    page_token: args.page_token,
                  }),
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_doc_comments",
      {
        title: "List comments on a Feishu doc / sheet / file",
        description:
          "Read collaboration comments left on a Feishu document, sheet, slide deck, or attached file. Useful when the agent's been asked to summarise feedback or follow up on a thread of unresolved comments. Returns top-level comments + their replies, plus solved/unsolved flags.",
        inputSchema: {
          file_token: z
            .string()
            .min(1)
            .describe("File token from the doc/sheet URL."),
          file_type: z
            .enum(["doc", "docx", "sheet", "file", "slides"])
            .describe(
              "File flavour. `docx` is the modern doc format; `doc` is the legacy one. `sheet` for spreadsheets. `slides` for decks. `file` for plain files in Drive.",
            ),
          is_solved: z
            .boolean()
            .optional()
            .describe(
              "Filter by resolved state. Omit to include both. Useful for \"any open comments still need addressing?\".",
            ),
          is_whole: z
            .boolean()
            .optional()
            .describe(
              "If true, return whole-document comments only (those not anchored to a specific selection). If false, only inline ones. Omit for both.",
            ),
          page_size: z.number().int().min(1).max(100).optional().describe("Default 20."),
          page_token: z.string().optional(),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_doc_comments",
          reason: "read comments on your Feishu doc",
          handler: (uat) =>
            active.channel.rawClient.drive.fileComment.list(
              {
                path: { file_token: args.file_token },
                params: {
                  file_type: args.file_type,
                  ...(args.is_solved !== undefined && { is_solved: args.is_solved }),
                  ...(args.is_whole !== undefined && { is_whole: args.is_whole }),
                  ...(args.page_size !== undefined && { page_size: args.page_size }),
                  ...(args.page_token !== undefined && {
                    page_token: args.page_token,
                  }),
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_sheet_read_range",
      {
        title: "Read a cell range from a Feishu spreadsheet",
        description:
          "Read raw cell values from a range in a Feishu spreadsheet. Returns the raw 2D `values` array (and optionally formula representations). Useful for the agent to inspect or summarise tabular data the user has set up. Goes through the legacy /sheets/v2/spreadsheets/{token}/values/{range} endpoint via raw fetch — the SDK's typed v3 surface doesn't expose value reads.",
        inputSchema: {
          spreadsheet_token: z
            .string()
            .min(1)
            .describe("Spreadsheet token from the URL (e.g. `shtcn…`)."),
          range: z
            .string()
            .min(1)
            .describe(
              "A1 notation range, e.g. `<sheet_id>!A1:D10`. The sheet_id prefix is required even for the first sheet — get it from `feishu_wiki list_nodes` or the URL.",
            ),
          value_render_option: z
            .enum(["ToString", "Formula", "FormattedValue", "UnformattedValue"])
            .optional()
            .describe(
              "How to render values. `ToString` (default) returns plain strings; `Formula` returns formulas as text; the others control number formatting.",
            ),
          date_time_render_option: z
            .enum(["FormattedString"])
            .optional()
            .describe(
              "Render dates/times as `FormattedString` instead of serial numbers. Defaults to serial numbers.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.runUatTool({
          active,
          toolName: "feishu_sheet_read_range",
          reason: "read cells from your Feishu spreadsheet",
          handler: async (uat) => {
            const params = new URLSearchParams();
            if (args.value_render_option !== undefined) {
              params.set("valueRenderOption", args.value_render_option);
            }
            if (args.date_time_render_option !== undefined) {
              params.set("dateTimeRenderOption", args.date_time_render_option);
            }
            const qs = params.toString() ? `?${params}` : "";
            const url = `${active.uatAccessor!.baseUrl}/open-apis/sheets/v2/spreadsheets/${encodeURIComponent(args.spreadsheet_token)}/values/${encodeURIComponent(args.range)}${qs}`;
            const resp = await fetch(url, {
              headers: { Authorization: `Bearer ${uat}` },
            });
            return (await resp.json()) as LarkApiResponse;
          },
        });
      },
    );

    this.registerDocProxyTools(server);
    this.registerWriteTools(server);
    this.registerAskUser(server);
  }

  /** Destructive write tools — every one of these gates behind
   * `gateWrite` (in-chat [Approve] [Deny] card) before touching
   * Lark. The OAuth grant lets the UAT execute any write the user
   * has scope for; the approval card is the per-call veto so the
   * agent can't act blind on its broad authorisation. */
  private registerWriteTools(server: McpServer): void {
    server.registerTool(
      "feishu_calendar_event_create",
      {
        title: "Create a Feishu calendar event",
        description:
          "Create a new event on a Feishu calendar (the user's primary, or a shared calendar they have writer/owner role on). REQUIRES IN-CHAT APPROVAL — the user sees a card with the event summary + time and must click Approve before the event is created. Use ISO 8601 timestamps as Unix-millis-strings (Lark's quirk). Skip the `attendees` field unless you specifically want notifications sent — most clarification flows are easier in chat than via calendar invite.",
        inputSchema: {
          calendar_id: z
            .string()
            .min(1)
            .describe(
              "Calendar id from `feishu_calendar action=primary` or `action=list`. Must be a calendar the user has writer/owner role on.",
            ),
          summary: z.string().min(1).max(200).describe("Event title (visible in the calendar grid)."),
          description: z
            .string()
            .max(10_000)
            .optional()
            .describe("Optional event body / agenda. Plain text or basic Markdown."),
          start_timestamp: z
            .string()
            .min(1)
            .describe(
              "Start time as a Unix-seconds-or-milliseconds string (Lark accepts both as the `timestamp` field). Use the user's local timezone unless they specify otherwise.",
            ),
          end_timestamp: z
            .string()
            .min(1)
            .describe("End time, same format as start_timestamp."),
          timezone: z
            .string()
            .optional()
            .describe(
              "IANA timezone (e.g. `Asia/Shanghai`, `America/Los_Angeles`). Default: Lark's user-account timezone.",
            ),
          attendees: z
            .array(z.string().min(1))
            .max(20)
            .optional()
            .describe(
              "List of user open_ids to invite. Leave empty for personal events — the user can add attendees in Lark UI faster than the agent can. Max 20 to bound notification fan-out.",
            ),
          visibility: z
            .enum(["default", "public", "private"])
            .optional()
            .describe("Event visibility on the calendar. Default uses the calendar's default."),
          need_notification: z
            .boolean()
            .optional()
            .describe(
              "Whether attendees receive notifications. Default true. Set false for batch-imports or test events.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!active.uatAccessor) {
          return toolError(
            "feishu_calendar_event_create requires UAT but the bot's OAuth pipeline isn't configured for this session",
          );
        }
        const summary = `Create event "${args.summary}" from ts=${args.start_timestamp} to ts=${args.end_timestamp}${args.timezone ? ` (${args.timezone})` : ""} on calendar ${args.calendar_id}`;
        const detail = [
          args.description ? `description: ${args.description.slice(0, 400)}` : null,
          args.attendees && args.attendees.length > 0
            ? `attendees: ${args.attendees.length} users (${args.attendees.slice(0, 5).join(", ")}${args.attendees.length > 5 ? ", ..." : ""})`
            : null,
        ]
          .filter((v) => v !== null)
          .join("\n");
        const blocked = await this.gateWrite(extra, {
          toolName: "feishu_calendar_event_create",
          summary,
          ...(detail.length > 0 && { detail }),
        });
        if (blocked) return blocked;

        return this.runUatTool({
          active,
          toolName: "feishu_calendar_event_create",
          reason: "create the calendar event you approved",
          handler: (uat) =>
            active.channel.rawClient.calendar.calendarEvent.create(
              {
                path: { calendar_id: args.calendar_id },
                data: {
                  summary: args.summary,
                  ...(args.description !== undefined && { description: args.description }),
                  start_time: {
                    timestamp: args.start_timestamp,
                    ...(args.timezone !== undefined && { timezone: args.timezone }),
                  },
                  end_time: {
                    timestamp: args.end_timestamp,
                    ...(args.timezone !== undefined && { timezone: args.timezone }),
                  },
                  ...(args.visibility !== undefined && { visibility: args.visibility }),
                  ...(args.need_notification !== undefined && {
                    need_notification: args.need_notification,
                  }),
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_bitable_record_create",
      {
        title: "Create a record in a Feishu Bitable table",
        description:
          "Insert one record. REQUIRES IN-CHAT APPROVAL. `fields` is a flat object keyed by field name (NOT field id) — values match Feishu's per-type encoding: strings/numbers for text/number cells, `{ text, link }` for hyperlink, arrays of `{ id }` for user-link, etc. For batch-creating many rows, do NOT loop — ask the user instead, since a per-row approval each makes for a terrible UX (a real batch tool with a single approval is upcoming, deferred until field-validation lands).",
        inputSchema: {
          app_token: z.string().min(1),
          table_id: z.string().min(1),
          fields: z
            .record(z.string(), z.unknown())
            .describe(
              "Field name → value map. Field names are the human-readable column names from the Bitable UI, not the internal `fld...` ids. Use `feishu_bitable_records` first if you don't know the schema.",
            ),
          user_id_type: z.enum(["open_id", "union_id", "user_id"]).optional(),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!active.uatAccessor) {
          return toolError(
            "feishu_bitable_record_create requires UAT but the bot's OAuth pipeline isn't configured for this session",
          );
        }
        const fieldNames = Object.keys(args.fields);
        const summary = `Create record in bitable ${args.app_token} table ${args.table_id} with ${fieldNames.length} field${fieldNames.length === 1 ? "" : "s"}`;
        const detail = JSON.stringify(args.fields, null, 2);
        const blocked = await this.gateWrite(extra, {
          toolName: "feishu_bitable_record_create",
          summary,
          detail,
        });
        if (blocked) return blocked;

        return this.runUatTool({
          active,
          toolName: "feishu_bitable_record_create",
          reason: "create the bitable record you approved",
          handler: (uat) =>
            active.channel.rawClient.bitable.appTableRecord.create(
              {
                path: { app_token: args.app_token, table_id: args.table_id },
                data: {
                  // The SDK types this `fields` Record with the giant
                  // value-shape union; an as-cast is the only path
                  // that matches without a 100-line type assertion.
                  fields: args.fields as Record<string, never>,
                },
                params: {
                  user_id_type: args.user_id_type ?? "open_id",
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_bitable_record_update",
      {
        title: "Update a Feishu Bitable record",
        description:
          "Update one record's fields by `record_id`. REQUIRES IN-CHAT APPROVAL. Only the fields in `fields` are written; unspecified fields keep their current values. Use `feishu_bitable_records` to find the record_id first if the agent only has a row content match.",
        inputSchema: {
          app_token: z.string().min(1),
          table_id: z.string().min(1),
          record_id: z.string().min(1).describe("Record id (e.g. `rec...`) — get from a prior `feishu_bitable_records` call."),
          fields: z
            .record(z.string(), z.unknown())
            .describe(
              "Partial field update map. Only listed fields change; others keep their values.",
            ),
          user_id_type: z.enum(["open_id", "union_id", "user_id"]).optional(),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!active.uatAccessor) {
          return toolError(
            "feishu_bitable_record_update requires UAT but the bot's OAuth pipeline isn't configured for this session",
          );
        }
        const fieldNames = Object.keys(args.fields);
        const summary = `Update record ${args.record_id} in bitable ${args.app_token} table ${args.table_id} — touches ${fieldNames.length} field${fieldNames.length === 1 ? "" : "s"}: ${fieldNames.join(", ")}`;
        const blocked = await this.gateWrite(extra, {
          toolName: "feishu_bitable_record_update",
          summary,
          detail: JSON.stringify(args.fields, null, 2),
        });
        if (blocked) return blocked;

        return this.runUatTool({
          active,
          toolName: "feishu_bitable_record_update",
          reason: "update the bitable record you approved",
          handler: (uat) =>
            active.channel.rawClient.bitable.appTableRecord.update(
              {
                path: {
                  app_token: args.app_token,
                  table_id: args.table_id,
                  record_id: args.record_id,
                },
                data: {
                  fields: args.fields as Record<string, never>,
                },
                params: {
                  user_id_type: args.user_id_type ?? "open_id",
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_bitable_record_delete",
      {
        title: "Delete a Feishu Bitable record",
        description:
          "Delete one record by `record_id`. REQUIRES IN-CHAT APPROVAL. Irreversible (Bitable doesn't expose undo for record-level deletes via OAPI). Use `feishu_bitable_records` to verify the record content first; the agent should pass an optional `preview` so the user can sanity-check what's being deleted on the approval card.",
        inputSchema: {
          app_token: z.string().min(1),
          table_id: z.string().min(1),
          record_id: z.string().min(1),
          preview: z
            .string()
            .max(500)
            .optional()
            .describe(
              "Short human-readable description of the record being deleted (e.g. `Title=\"Q3 review\", Status=\"Open\"`). Highly recommended — without it the user only sees the opaque record_id on the approval card.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!active.uatAccessor) {
          return toolError(
            "feishu_bitable_record_delete requires UAT but the bot's OAuth pipeline isn't configured for this session",
          );
        }
        const labelled = args.preview ? `record (${args.preview})` : "record";
        const summary = `⚠️ DELETE ${labelled} (record_id=${args.record_id}) from bitable ${args.app_token} table ${args.table_id}. This is irreversible.`;
        const blocked = await this.gateWrite(extra, {
          toolName: "feishu_bitable_record_delete",
          summary,
        });
        if (blocked) return blocked;

        return this.runUatTool({
          active,
          toolName: "feishu_bitable_record_delete",
          reason: "delete the bitable record you approved",
          handler: (uat) =>
            active.channel.rawClient.bitable.appTableRecord.delete(
              {
                path: {
                  app_token: args.app_token,
                  table_id: args.table_id,
                  record_id: args.record_id,
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_calendar_event_update",
      {
        title: "Update an existing Feishu calendar event",
        description:
          "Patch one event's fields by `event_id`. REQUIRES IN-CHAT APPROVAL. Only the fields you pass are written; unspecified fields keep their values. Common uses: reschedule (`start_timestamp`+`end_timestamp`), retitle (`summary`), edit description. For complex changes (attendee add/remove, recurrence rules), use Lark UI — those have nuanced semantics worth a human touch.",
        inputSchema: {
          calendar_id: z.string().min(1),
          event_id: z.string().min(1),
          summary: z
            .string()
            .max(200)
            .optional()
            .describe("New event title. Pass undefined to keep the current title."),
          description: z.string().max(10_000).optional(),
          start_timestamp: z
            .string()
            .optional()
            .describe(
              "New start as Unix-seconds-or-millis string. If you change start, almost always change end too — Feishu doesn't auto-shift the duration.",
            ),
          end_timestamp: z.string().optional(),
          timezone: z
            .string()
            .optional()
            .describe(
              "IANA timezone for the new times. Required if you set start/end and the original timezone was unusual.",
            ),
          need_notification: z
            .boolean()
            .optional()
            .describe("Whether attendees get a change notification. Default true."),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!active.uatAccessor) {
          return toolError(
            "feishu_calendar_event_update requires UAT but the bot's OAuth pipeline isn't configured for this session",
          );
        }
        const changes: string[] = [];
        if (args.summary !== undefined) changes.push(`summary="${args.summary}"`);
        if (args.description !== undefined) changes.push("description");
        if (args.start_timestamp !== undefined) changes.push(`start=${args.start_timestamp}`);
        if (args.end_timestamp !== undefined) changes.push(`end=${args.end_timestamp}`);
        if (args.timezone !== undefined) changes.push(`tz=${args.timezone}`);
        if (changes.length === 0) {
          return toolError(
            "feishu_calendar_event_update: no fields to update — pass at least one of summary/description/start_timestamp/end_timestamp/timezone/need_notification",
          );
        }
        const summary = `Update event ${args.event_id} on calendar ${args.calendar_id}: ${changes.join(", ")}`;
        const blocked = await this.gateWrite(extra, {
          toolName: "feishu_calendar_event_update",
          summary,
        });
        if (blocked) return blocked;

        return this.runUatTool({
          active,
          toolName: "feishu_calendar_event_update",
          reason: "update the calendar event you approved",
          handler: (uat) =>
            active.channel.rawClient.calendar.calendarEvent.patch(
              {
                path: {
                  calendar_id: args.calendar_id,
                  event_id: args.event_id,
                },
                data: {
                  ...(args.summary !== undefined && { summary: args.summary }),
                  ...(args.description !== undefined && {
                    description: args.description,
                  }),
                  ...(args.start_timestamp !== undefined && {
                    start_time: {
                      timestamp: args.start_timestamp,
                      ...(args.timezone !== undefined && { timezone: args.timezone }),
                    },
                  }),
                  ...(args.end_timestamp !== undefined && {
                    end_time: {
                      timestamp: args.end_timestamp,
                      ...(args.timezone !== undefined && { timezone: args.timezone }),
                    },
                  }),
                  ...(args.need_notification !== undefined && {
                    need_notification: args.need_notification,
                  }),
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );

    server.registerTool(
      "feishu_calendar_event_delete",
      {
        title: "Delete a Feishu calendar event",
        description:
          "Delete an event from the user's calendar. REQUIRES IN-CHAT APPROVAL — the user sees the event id + summary on a card and must click Approve. Irreversible; recurring events are deleted in their entirety (use a different mechanism for single-occurrence cancellation if Lark exposes one). Use `feishu_calendar_event action=get` first to fetch the event details for the approval card.",
        inputSchema: {
          calendar_id: z.string().min(1).describe("Calendar id."),
          event_id: z.string().min(1).describe("Event id to delete."),
          summary: z
            .string()
            .max(200)
            .optional()
            .describe(
              "Optional event title — included in the approval card so the user can sanity-check what's being deleted. Highly recommended to fetch via `feishu_calendar_event action=get` first.",
            ),
          need_notification: z
            .boolean()
            .optional()
            .describe(
              "Whether attendees receive a deletion notification. Default true.",
            ),
        },
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!active.uatAccessor) {
          return toolError(
            "feishu_calendar_event_delete requires UAT but the bot's OAuth pipeline isn't configured for this session",
          );
        }
        const labelled = args.summary ? `"${args.summary}"` : "event";
        const summary = `⚠️ DELETE ${labelled} (event_id=${args.event_id}) from calendar ${args.calendar_id}. This is irreversible.`;
        const blocked = await this.gateWrite(extra, {
          toolName: "feishu_calendar_event_delete",
          summary,
        });
        if (blocked) return blocked;

        return this.runUatTool({
          active,
          toolName: "feishu_calendar_event_delete",
          reason: "delete the calendar event you approved",
          handler: (uat) =>
            active.channel.rawClient.calendar.calendarEvent.delete(
              {
                path: {
                  calendar_id: args.calendar_id,
                  event_id: args.event_id,
                },
                params: {
                  ...(args.need_notification !== undefined && {
                    need_notification: args.need_notification ? "true" : "false",
                  }),
                },
              },
              lark.withUserAccessToken(uat),
            ),
        });
      },
    );
  }

  /** Three doc tools that proxy through Feishu's hosted MCP gateway
   * (`mcp.feishu.cn`) — the canonical OAPI for fetching/creating/
   * updating cloud docs as Markdown isn't exposed in the regular
   * `/open-apis/...` surface, so openclaw uses the hosted MCP and we
   * follow suit. JSON-RPC envelope ride is wired up here once and
   * each tool just stamps its own arguments. */
  private registerDocProxyTools(server: McpServer): void {
    const fetchDoc = z.object({
      doc_id: z
        .string()
        .min(1)
        .describe(
          "Doc id or full URL — the gateway parses both. For wiki-hosted docs, pass the wiki node URL.",
        ),
      offset: z
        .number()
        .int()
        .min(0)
        .optional()
        .describe("Character offset for paginating large docs."),
      limit: z
        .number()
        .int()
        .min(1)
        .optional()
        .describe(
          "Max characters this call. Use only when the doc is large enough that a single fetch would blow the agent's context.",
        ),
    });
    server.registerTool(
      "feishu_fetch_doc",
      {
        title: "Fetch a Feishu cloud doc as Markdown",
        description:
          "Read a Feishu cloud doc (docx / doc) and return its title + body as Markdown. Goes through Feishu's hosted MCP gateway (`mcp.feishu.cn` / `mcp.larksuite.com`) — the gateway has the doc-to-Markdown conversion logic. Supports offset/limit pagination for large docs.",
        inputSchema: fetchDoc.shape,
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        return this.callDocProxyTool(active, "fetch-doc", "feishu_fetch_doc", "fetch your Feishu doc", args);
      },
    );

    const createDoc = z.object({
      markdown: z.string().optional().describe("Doc body as Markdown."),
      title: z.string().optional().describe("Doc title."),
      folder_token: z
        .string()
        .optional()
        .describe(
          "Parent folder token (mutually exclusive with `wiki_node` / `wiki_space`).",
        ),
      wiki_node: z
        .string()
        .optional()
        .describe("Parent wiki node token or URL (mutually exclusive)."),
      wiki_space: z
        .string()
        .optional()
        .describe("Wiki space id (mutually exclusive). Special value `my_library`."),
      task_id: z
        .string()
        .optional()
        .describe(
          "If a previous create returned async — pass its task_id to poll status instead of starting a new doc.",
        ),
    });
    server.registerTool(
      "feishu_create_doc",
      {
        title: "Create a Feishu doc from Markdown",
        description:
          "Create a Feishu cloud doc from Markdown content. Requires `markdown` + `title` unless `task_id` is set (which polls a previous async create). Destination is `folder_token` OR `wiki_node` OR `wiki_space` — at most one. Goes through Feishu's hosted MCP gateway.",
        inputSchema: createDoc.shape,
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!args.task_id) {
          if (!args.markdown || !args.title) {
            return toolError(
              "feishu_create_doc: without `task_id`, both `markdown` and `title` are required",
            );
          }
          const dests = [args.folder_token, args.wiki_node, args.wiki_space].filter(
            (v) => v !== undefined,
          );
          if (dests.length > 1) {
            return toolError(
              "feishu_create_doc: `folder_token`, `wiki_node`, and `wiki_space` are mutually exclusive — pass at most one",
            );
          }
        }
        return this.callDocProxyTool(active, "create-doc", "feishu_create_doc", "create your Feishu doc", args);
      },
    );

    const updateDoc = z.object({
      doc_id: z.string().optional().describe("Doc id or URL — required unless `task_id` is set."),
      markdown: z.string().optional().describe("New Markdown content (required for every mode except `delete_range`)."),
      mode: z
        .enum([
          "overwrite",
          "append",
          "replace_range",
          "replace_all",
          "insert_before",
          "insert_after",
          "delete_range",
        ])
        .describe(
          "Update mode. `overwrite` replaces the whole doc; `append` adds to the end; the `*_range` / `insert_*` / `delete_range` modes need a selection (see below).",
        ),
      selection_with_ellipsis: z
        .string()
        .optional()
        .describe(
          "Locate selection by `start...end` text snippets. Required for replace_range / insert_before / insert_after / delete_range when not using selection_by_title.",
        ),
      selection_by_title: z
        .string()
        .optional()
        .describe(
          "Locate selection by heading (e.g. `## My Section`). Mutually exclusive with selection_with_ellipsis.",
        ),
      new_title: z.string().optional().describe("Optionally rename the doc."),
      task_id: z.string().optional().describe("Poll previous async update."),
    });
    server.registerTool(
      "feishu_update_doc",
      {
        title: "Update a Feishu doc with Markdown",
        description:
          "Mutate an existing Feishu cloud doc. Modes: `overwrite` / `append` / `replace_range` / `replace_all` / `insert_before` / `insert_after` / `delete_range`. The selection-based modes need either `selection_with_ellipsis` (start...end snippet) or `selection_by_title` (heading text). Use `task_id` to poll a previous async update.",
        inputSchema: updateDoc.shape,
      },
      async (args, extra) => {
        const active = this.resolveActive(extra);
        if (!active.ok) return active.reply;
        if (!args.task_id) {
          if (!args.doc_id) {
            return toolError(
              "feishu_update_doc: without `task_id`, `doc_id` is required",
            );
          }
          const needsSelection =
            args.mode === "replace_range" ||
            args.mode === "insert_before" ||
            args.mode === "insert_after" ||
            args.mode === "delete_range";
          if (needsSelection) {
            const hasEllipsis = args.selection_with_ellipsis !== undefined;
            const hasTitle = args.selection_by_title !== undefined;
            if ((hasEllipsis && hasTitle) || (!hasEllipsis && !hasTitle)) {
              return toolError(
                `feishu_update_doc: mode=${args.mode} requires exactly one of \`selection_with_ellipsis\` or \`selection_by_title\``,
              );
            }
          }
          if (args.mode !== "delete_range" && !args.markdown) {
            return toolError(
              `feishu_update_doc: mode=${args.mode} requires \`markdown\``,
            );
          }
        }
        // Destructive modes (overwrite, replace_*, delete_range) gate
        // behind in-chat approval — they can erase content the user
        // didn't ask to lose. Additive modes (append, insert_*) skip
        // the gate, since they only ADD content (still under the user's
        // OAuth grant). task_id mode is async polling, not a write,
        // so it skips too.
        const isDestructive =
          !args.task_id &&
          (args.mode === "overwrite" ||
            args.mode === "replace_all" ||
            args.mode === "replace_range" ||
            args.mode === "delete_range");
        if (isDestructive) {
          const summary = describeUpdateDoc(args);
          const blocked = await this.gateWrite(extra, {
            toolName: "feishu_update_doc",
            summary,
            ...(args.markdown !== undefined && {
              detail: args.markdown.slice(0, 1500),
            }),
          });
          if (blocked) return blocked;
        }
        return this.callDocProxyTool(active, "update-doc", "feishu_update_doc", "update your Feishu doc", args);
      },
    );
  }

  /** POST a `tools/call` JSON-RPC envelope to Feishu's hosted MCP
   * gateway. The gateway URL derives from the bot's baseUrl by
   * substituting the `open.` host prefix with `mcp.` (open.feishu.cn
   * → mcp.feishu.cn). The UAT rides in the `X-Lark-MCP-UAT` header
   * (gateway-specific, not Bearer); `X-Lark-MCP-Allowed-Tools` scopes
   * the call to a single tool name as defense in depth. */
  private async callDocProxyTool(
    active: {
      channel: lark.LarkChannel;
      chatId: string;
      platformUserId: string;
      uatAccessor?: UATAccessor;
    },
    upstreamName: string,
    toolName: string,
    reason: string,
    args: Record<string, unknown>,
  ): Promise<{ content: { type: "text"; text: string }[]; isError?: true }> {
    if (!active.uatAccessor) {
      return toolError(
        `${toolName} requires UAT but the bot's OAuth pipeline isn't configured for this session`,
      );
    }
    const baseUrl = active.uatAccessor.baseUrl;
    const mcpUrl = `${deriveMcpHost(baseUrl)}/mcp`;
    try {
      const result = await active.uatAccessor.invoke(
        {
          userOpenId: active.platformUserId,
          chatId: active.chatId,
          reason,
        },
        async (uat) => {
          const resp = await fetch(mcpUrl, {
            method: "POST",
            headers: {
              "Content-Type": "application/json",
              "X-Lark-MCP-UAT": uat,
              "X-Lark-MCP-Allowed-Tools": upstreamName,
            },
            body: JSON.stringify({
              jsonrpc: "2.0",
              id: `${toolName}-${Date.now()}`,
              method: "tools/call",
              params: { name: upstreamName, arguments: args },
            }),
          });
          if (!resp.ok) {
            const text = await resp.text();
            throw new Error(
              `MCP gateway HTTP ${resp.status}: ${text.slice(0, 500)}`,
            );
          }
          const body = (await resp.json()) as McpJsonRpcResponse;
          if ("error" in body && body.error) {
            return {
              code: body.error.code,
              msg: body.error.message,
              data: body.error.data ?? null,
            } satisfies LarkApiResponse;
          }
          // Upstream `result` is itself an MCP tool reply
          // `{ content: [{ type, text }, ...] }`. Pass through to the
          // agent as-is (we just stash it in `data` for runUatTool's
          // JSON serialiser).
          return { code: 0, data: body.result ?? null } satisfies LarkApiResponse;
        },
      );
      if (result.kind !== "ok") {
        return toolError(authFailedMessage(toolName, result.outcome));
      }
      const res = result.result;
      if (typeof res.code === "number" && res.code !== 0) {
        return toolError(
          `Feishu MCP gateway error ${res.code}: ${res.msg ?? "unknown"}`,
        );
      }
      return {
        content: [
          { type: "text", text: JSON.stringify(res.data ?? {}, null, 2) },
        ],
      };
    } catch (err) {
      this.opts.logger.debug(`${toolName} failed: ${String(err)}`);
      return toolError(err instanceof Error ? err.message : String(err));
    }
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

/** One-line description for the `feishu_update_doc` approval card.
 * Spells out which destructive mode the agent picked + the doc id +
 * (for selection-based modes) the selection target so the user can
 * sanity-check before approving. */
function describeUpdateDoc(args: {
  doc_id?: string | undefined;
  mode: string;
  selection_with_ellipsis?: string | undefined;
  selection_by_title?: string | undefined;
  new_title?: string | undefined;
}): string {
  const mode = args.mode;
  let where = "";
  if (args.selection_with_ellipsis !== undefined) {
    where = ` at "${args.selection_with_ellipsis.slice(0, 80)}"`;
  } else if (args.selection_by_title !== undefined) {
    where = ` at heading "${args.selection_by_title.slice(0, 80)}"`;
  }
  const titleSuffix = args.new_title !== undefined ? ` (rename → "${args.new_title}")` : "";
  if (mode === "overwrite" || mode === "replace_all") {
    return `⚠️ ${mode.toUpperCase()} doc ${args.doc_id} — replaces the entire current contents${titleSuffix}.`;
  }
  if (mode === "delete_range") {
    return `⚠️ DELETE_RANGE in doc ${args.doc_id}${where} — irreversible.`;
  }
  return `⚠️ ${mode.toUpperCase()} in doc ${args.doc_id}${where}${titleSuffix}.`;
}

/** Common Lark OAPI response shape. Tools whose handler unions
 * multiple SDK calls (different `data` shapes per branch) declare the
 * handler return as `Promise<LarkApiResponse>` so TypeScript can pick
 * a single type for the union without forcing each tool to assert. */
type LarkApiResponse = {
  code?: number | undefined;
  msg?: string | undefined;
  data?: unknown;
};

/** JSON-RPC response from Feishu's hosted MCP gateway
 * (`mcp.feishu.cn` / `mcp.larksuite.com`). `result` carries the
 * upstream tool's `{ content: [...] }` payload on success; `error`
 * carries `{ code, message, data? }` on failure. */
interface McpJsonRpcResponse {
  jsonrpc: "2.0";
  id?: string | number | null;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

/** Derive Feishu's hosted MCP gateway base URL from the bot's
 * `baseUrl`: `https://open.feishu.cn` → `https://mcp.feishu.cn`,
 * `https://open.larksuite.com` → `https://mcp.larksuite.com`, and
 * by-convention `open.X` → `mcp.X` for private-cloud / regional
 * hosts. Falls back to the input on URL parse error. */
function deriveMcpHost(baseUrl: string): string {
  try {
    const u = new URL(baseUrl);
    if (u.hostname.startsWith("open.")) {
      return `${u.protocol}//${u.hostname.replace(/^open\./, "mcp.")}`;
    }
    return baseUrl;
  } catch {
    return baseUrl;
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

/** Translate an `AuthFlowOutcome` non-`ok` variant into an LLM-readable
 * error string. Spelled out so each failure mode tells the agent what
 * to do next (retry, give up, ask the user differently). */
function authFailedMessage(
  toolName: string,
  outcome: import("../auth/auth-flow.js").AuthFlowOutcome,
): string {
  switch (outcome.kind) {
    case "denied":
      return `${toolName}: the user declined the authorization request — fall back to a tool that doesn't require their OAuth, or ask them to reconsider`;
    case "expired":
      return `${toolName}: the authorization page timed out before the user approved — ask the user again to retry, which will start a fresh sign-in`;
    case "cancelled":
      return `${toolName}: authorization was cancelled (the bot is shutting down) — try again later`;
    case "error":
      return `${toolName}: authorization flow failed: ${outcome.message}`;
    case "ok":
      return `${toolName}: authorization succeeded but tool dispatch returned an unexpected ok branch`;
  }
}
