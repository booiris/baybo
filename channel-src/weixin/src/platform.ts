import crypto from "node:crypto";

import type { Logger, StartBotCommand } from "@aura/channel-sdk";
import type {
  BotPlatform,
  BotStartHooks,
} from "@aura/channel-sdk/bot";

import {
  SESSION_EXPIRED_ERRCODE,
  assertSessionActive,
  pauseSession,
} from "./api/session-guard.js";
import { WeixinConfigManager } from "./api/config-cache.js";
import { sendMessage, sendTyping } from "./api/endpoints.js";
import { MessageItemType, MessageState, MessageType, TypingStatus } from "./api/types.js";
import { sanitizeMarkdown, StreamingMarkdownFilter } from "./messaging/markdown-filter.js";
import { runPollLoop } from "./runtime/poll-loop.js";
import type { AuthBlob, RuntimeState, WeixinBotHandle, WeixinChat } from "./types.js";
import type { WeixinApprovals } from "./approvals.js";

function parseAuthBlob(raw: string): AuthBlob {
  const trimmed = raw.trimStart();
  if (!trimmed.startsWith("{")) {
    throw new Error(
      "weixin: StartBot token is not a JSON AuthBlob — re-register via `aura channel add`",
    );
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch (e) {
    throw new Error(`weixin: failed to parse AuthBlob JSON: ${String(e)}`);
  }
  if (
    !parsed
    || typeof parsed !== "object"
    || (parsed as AuthBlob).version !== 1
    || typeof (parsed as AuthBlob).botToken !== "string"
    || typeof (parsed as AuthBlob).baseUrl !== "string"
    || typeof (parsed as AuthBlob).accountId !== "string"
  ) {
    throw new Error("weixin: AuthBlob missing required fields (version=1, botToken, baseUrl, accountId)");
  }
  return parsed as AuthBlob;
}

export class WeixinPlatform implements BotPlatform<WeixinBotHandle, WeixinChat> {
  /**
   * `approvals` is optional so the platform can be constructed without
   * the approvals broker for tests. In production `index.ts` wires the
   * same instance here and into `BotChannel` so the poll loop can
   * intercept approval replies before they reach the agent.
   */
  constructor(
    private readonly logger: Logger,
    private readonly approvals?: WeixinApprovals,
  ) {}

  chatKey(chat: WeixinChat): string {
    return chat.toUserId;
  }

  async startBot(
    cmd: StartBotCommand,
    hooks: BotStartHooks<WeixinBotHandle, WeixinChat>,
  ): Promise<{ handle: WeixinBotHandle; username: string; waitForExit: Promise<void> }> {
    const blob = parseAuthBlob(cmd.token);

    const log = this.logger;
    const configMgr = new WeixinConfigManager(
      { baseUrl: blob.baseUrl, token: blob.botToken },
      (msg) => log.debug(`[${cmd.botId}] ${msg}`),
    );

    const state: RuntimeState = {
      accountId: cmd.botId,
      botToken: blob.botToken,
      baseUrl: blob.baseUrl,
      cdnBaseUrl: blob.cdnBaseUrl ?? "",
      userId: blob.userId,
      getUpdatesBuf: "",
      configMgr,
      mdFilter: new StreamingMarkdownFilter(),
      abort: new AbortController(),
      contextTokens: new Map(),
    };
    const handle: WeixinBotHandle = { accountId: cmd.botId, state };

    await hooks.attach(handle);

    const botId = cmd.botId;
    const approvals = this.approvals;
    const wrappedHooks: BotStartHooks<WeixinBotHandle, WeixinChat> = {
      attach: hooks.attach,
      emit: (ev) => {
        if (approvals?.tryResolveInbound(botId, ev.chat, ev.content)) {
          // Approval reply — swallow so aura never sees it as a chat.
          return;
        }
        hooks.emit(ev);
      },
    };
    const waitForExit = runPollLoop(handle, wrappedHooks, this.logger).catch((err) => {
      this.logger.error(`weixin poll loop crashed for '${cmd.botId}': ${String(err)}`);
    });

    return { handle, username: blob.userId || cmd.botId, waitForExit };
  }

  async stopBot(handle: WeixinBotHandle): Promise<void> {
    handle.state.abort.abort();
  }

  async sendText(
    handle: WeixinBotHandle,
    chat: WeixinChat,
    text: string,
  ): Promise<void> {
    assertSessionActive(handle.accountId);
    const filtered = sanitizeMarkdown(text);
    const contextToken = handle.state.contextTokens.get(chat.toUserId);
    // iLink uses `client_id` for server-side dedup. If omitted, sends
    // succeed at HTTP level but never reach the user — the server
    // treats them as duplicates of a sentinel "no id" slot.
    const clientId = `aura-weixin:${Date.now()}-${crypto.randomBytes(4).toString("hex")}`;
    try {
      await sendMessage({
        baseUrl: handle.state.baseUrl,
        token: handle.state.botToken,
        body: {
          msg: {
            from_user_id: "",
            to_user_id: chat.toUserId,
            client_id: clientId,
            message_type: MessageType.BOT,
            message_state: MessageState.FINISH,
            item_list: [{ type: MessageItemType.TEXT, text_item: { text: filtered } }],
            ...(contextToken !== undefined ? { context_token: contextToken } : {}),
          },
        },
      });
      // Tell iLink to stop showing "typing…" on the peer's chat.
      // Without this, the server-side TTL (~several seconds) lingers
      // after the reply is already delivered.
      void this.cancelTyping(handle, chat);
    } catch (err) {
      if (isSessionExpired(err)) {
        pauseSession(handle.accountId);
        this.logger.error(
          `weixin bot '${handle.accountId}' sendMessage hit errcode ${SESSION_EXPIRED_ERRCODE}; pausing`,
        );
      }
      throw err;
    }
  }

  private async cancelTyping(
    handle: WeixinBotHandle,
    chat: WeixinChat,
  ): Promise<void> {
    const contextToken = handle.state.contextTokens.get(chat.toUserId);
    try {
      const cfg = await handle.state.configMgr.getForUser(
        chat.toUserId,
        contextToken,
      );
      if (!cfg.typingTicket) return;
      await sendTyping({
        baseUrl: handle.state.baseUrl,
        token: handle.state.botToken,
        body: {
          ilink_user_id: chat.toUserId,
          typing_ticket: cfg.typingTicket,
          status: TypingStatus.CANCEL,
        },
      });
    } catch (err) {
      this.logger.debug(`weixin cancelTyping ignored error: ${String(err)}`);
    }
  }

  async notifyTyping(handle: WeixinBotHandle, chat: WeixinChat): Promise<void> {
    if (!handle.state.contextTokens.has(chat.toUserId)) return;
    try {
      const contextToken = handle.state.contextTokens.get(chat.toUserId);
      const cfg = await handle.state.configMgr.getForUser(
        chat.toUserId,
        contextToken,
      );
      if (!cfg.typingTicket) return;
      await sendTyping({
        baseUrl: handle.state.baseUrl,
        token: handle.state.botToken,
        body: {
          ilink_user_id: chat.toUserId,
          typing_ticket: cfg.typingTicket,
          status: TypingStatus.TYPING,
        },
      });
    } catch (err) {
      this.logger.debug(`weixin notifyTyping ignored error: ${String(err)}`);
    }
  }
}

function isSessionExpired(err: unknown): boolean {
  if (!(err instanceof Error)) return false;
  return err.message.includes(`errcode ${SESSION_EXPIRED_ERRCODE}`)
    || err.message.includes(`ret=${SESSION_EXPIRED_ERRCODE}`)
    || err.message.includes(`"errcode":${SESSION_EXPIRED_ERRCODE}`)
    || err.message.includes(`"ret":${SESSION_EXPIRED_ERRCODE}`);
}
