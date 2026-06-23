import crypto from "node:crypto";

import type { Logger, StartBotCommand, WireAttachment } from "@baybo/channel-sdk";
import { fetchBlob } from "@baybo/channel-sdk";
import type {
  BotMediaPayload,
  BotPlatform,
  BotStartHooks,
} from "@baybo/channel-sdk/bot";

import { DEFAULT_CDN_BASE_URL } from "./auth/login-qr.js";
import {
  SESSION_EXPIRED_ERRCODE,
  assertSessionActive,
  pauseSession,
} from "./api/session-guard.js";
import { WeixinConfigManager } from "./api/config-cache.js";
import { sendMessage, sendTyping } from "./api/endpoints.js";
import { MessageItemType, MessageState, MessageType, TypingStatus } from "./api/types.js";
import { sanitizeMarkdown, StreamingMarkdownFilter } from "./messaging/markdown-filter.js";
import { sendWeixinMediaBytes } from "./messaging/send-media.js";
import { runPollLoop } from "./runtime/poll-loop.js";
import type { AuthBlob, RuntimeState, WeixinBotHandle, WeixinChat } from "./types.js";
import type { WeixinApprovals } from "./approvals.js";

function parseAuthBlob(raw: string): AuthBlob {
  const trimmed = raw.trimStart();
  if (!trimmed.startsWith("{")) {
    throw new Error(
      "weixin: StartBot token is not a JSON AuthBlob — re-register via `baybo channel add`",
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
      cdnBaseUrl: blob.cdnBaseUrl?.trim() || DEFAULT_CDN_BASE_URL,
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
          // Approval reply — swallow so baybo never sees it as a chat.
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
    const clientId = `baybo-weixin:${Date.now()}-${crypto.randomBytes(4).toString("hex")}`;
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

  async sendMedia(
    handle: WeixinBotHandle,
    chat: WeixinChat,
    payload: BotMediaPayload,
  ): Promise<void> {
    assertSessionActive(handle.accountId);
    if (!handle.state.cdnBaseUrl) {
      // Without a CDN endpoint we can't upload — surface a single
      // text-only message with the placeholder so the user sees
      // *something*. Operators on a private cloud need to set
      // `BAYBO_WEIXIN_CDN_BASE_URL` for media to actually flow.
      this.logger.warn(
        "weixin sendMedia: cdnBaseUrl unset; falling back to text",
      );
      const fallback = mediaFallbackText(payload.text, payload.attachments);
      await this.sendText(handle, chat, fallback);
      return;
    }
    const contextToken = handle.state.contextTokens.get(chat.toUserId);
    const opts = {
      baseUrl: handle.state.baseUrl,
      token: handle.state.botToken,
      ...(contextToken !== undefined ? { contextToken } : {}),
    };
    const filteredText = payload.text ? sanitizeMarkdown(payload.text) : "";
    try {
      // Each attachment ships as its own iLink message — the protocol
      // demands one item per request. The caption rides with the
      // first attachment only so the user doesn't see it duplicated.
      let captionRemaining = filteredText;
      for (const att of payload.attachments) {
        const bytes = await this.fetchAttachment(att);
        if (!bytes) continue;
        await sendWeixinMediaBytes({
          bytes,
          mimeType: att.mime_type,
          ...(att.filename ? { filename: att.filename } : {}),
          to: chat.toUserId,
          text: captionRemaining,
          opts,
          cdnBaseUrl: handle.state.cdnBaseUrl,
          logger: this.logger,
        });
        captionRemaining = "";
      }
      void this.cancelTyping(handle, chat);
    } catch (err) {
      if (isSessionExpired(err)) {
        pauseSession(handle.accountId);
        this.logger.error(
          `weixin bot '${handle.accountId}' sendMedia hit errcode ${SESSION_EXPIRED_ERRCODE}; pausing`,
        );
      }
      throw err;
    }
  }

  /** Pull blob bytes from the gateway. `null` on miss so the loop
   * skips the attachment instead of aborting the whole reply. */
  private async fetchAttachment(att: WireAttachment): Promise<Buffer | null> {
    try {
      const { bytes } = await fetchBlob(att.blob_id);
      return Buffer.from(bytes);
    } catch (err) {
      this.logger.error(
        `weixin sendMedia: fetchBlob failed blob_id=${att.blob_id} err=${String(err)}`,
      );
      return null;
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

/** Build a degraded text fallback when `sendMedia` can't actually
 * upload. Lists the attachment kinds + sizes so the user knows what
 * was meant to flow, even though it didn't. */
function mediaFallbackText(text: string, attachments: WireAttachment[]): string {
  const lines = attachments.map(
    (a) => `[${a.kind}: ${a.mime_type} ${(a.size / 1024).toFixed(0)} KiB]`,
  );
  if (text) lines.unshift(text);
  return lines.join("\n");
}

function isSessionExpired(err: unknown): boolean {
  if (!(err instanceof Error)) return false;
  return err.message.includes(`errcode ${SESSION_EXPIRED_ERRCODE}`)
    || err.message.includes(`ret=${SESSION_EXPIRED_ERRCODE}`)
    || err.message.includes(`"errcode":${SESSION_EXPIRED_ERRCODE}`)
    || err.message.includes(`"ret":${SESSION_EXPIRED_ERRCODE}`);
}
