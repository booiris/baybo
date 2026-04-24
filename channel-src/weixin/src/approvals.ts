import crypto from "node:crypto";

import type { ApprovalDecision, ApprovalRequest, Logger } from "@aura/channel-sdk";
import type {
  BotRoute,
  PlatformApprovals,
} from "@aura/channel-sdk/bot";

import { sendMessage } from "./api/endpoints.js";
import { MessageItemType, MessageState, MessageType } from "./api/types.js";
import type { WeixinBotHandle, WeixinChat } from "./types.js";

/**
 * How long we keep a pending approval before auto-denying. Aura has
 * its own approval timeout higher up, but we still clean up here so a
 * never-answered prompt doesn't hold a chat slot forever if the
 * upstream timer drifts.
 */
const APPROVAL_TIMEOUT_MS = 10 * 60 * 1000;

interface PendingEntry {
  readonly callId: string;
  readonly botId: string;
  readonly handle: WeixinBotHandle;
  readonly chat: WeixinChat;
  readonly resolve: (decision: ApprovalDecision) => void;
  readonly timeout: ReturnType<typeof setTimeout>;
}

function chatKey(botId: string, chat: WeixinChat): string {
  return `${botId}|${chat.toUserId}`;
}

/**
 * Parse a user's reply against the approval keyword set. Case-
 * insensitive. Returns `null` when the message isn't an approval reply
 * at all — the poll loop then forwards it to the agent unchanged.
 */
export function matchDecision(raw: string): ApprovalDecision | null {
  const t = raw.trim().toLowerCase();
  if (!t) return null;
  if (t === "yes" || t === "y" || t === "ok" || t === "approve") {
    return "approve";
  }
  if (t === "always" || t === "approve_always") {
    return "approve_always";
  }
  if (t === "no" || t === "n" || t === "deny" || t === "reject") {
    return "deny";
  }
  return null;
}

/**
 * Aura ships `paramsPreview` as a compact JSON blob (see
 * `aura_tools::approval::preview_params`), truncated mid-string with a
 * trailing `…` when it would otherwise overflow. We try to pretty-
 * print; if truncation broke parsing or the preview isn't JSON at all,
 * fall back to the raw string. Returns the string ready to paste into
 * a message body (no wrapping quotes added).
 */
function formatParamsPreview(raw: string): string {
  const trimmed = raw.trim();
  if (!trimmed) return "";
  try {
    const parsed: unknown = JSON.parse(trimmed);
    return JSON.stringify(parsed, null, 2);
  } catch {
    return trimmed;
  }
}

/**
 * Text-based approval broker for the Weixin channel.
 *
 * iLink has no inline-button UI like Telegram, so we send a plain-text
 * prompt listing the acceptable reply keywords, then rely on the
 * platform's poll loop calling {@link tryResolveInbound} before
 * forwarding the next message to Aura. A non-matching reply falls
 * through to the agent unchanged — we intentionally don't swallow it,
 * so the user can redirect the conversation mid-approval if they want
 * to abandon the tool call.
 */
export class WeixinApprovals
  implements PlatformApprovals<WeixinBotHandle, WeixinChat> {
  private readonly pending = new Map<string, PendingEntry>();

  constructor(private readonly logger: Logger) { }

  async onRequested(
    req: ApprovalRequest,
    route: BotRoute<WeixinBotHandle, WeixinChat> | null,
  ): Promise<ApprovalDecision> {
    if (!route) {
      this.logger.warn(
        `weixin approval: no route for callId=${req.callId}; auto-denying`,
      );
      return "deny";
    }

    const key = chatKey(route.botId, route.chat);
    // One outstanding approval per (bot, user). If a second request
    // arrives before the first is answered, drop the old one rather
    // than queueing — aura serializes tool calls per session, so this
    // path should be rare, and a silent queue would just strand the
    // user on a stale prompt.
    const prev = this.pending.get(key);
    if (prev) {
      this.logger.warn(
        `weixin approval: superseding pending callId=${prev.callId} for ${key}`,
      );
      clearTimeout(prev.timeout);
      this.pending.delete(key);
      prev.resolve("deny");
    }

    const promise = new Promise<ApprovalDecision>((resolve) => {
      const timeout = setTimeout(() => {
        const entry = this.pending.get(key);
        if (!entry || entry.callId !== req.callId) return;
        this.pending.delete(key);
        this.logger.warn(
          `weixin approval: timed out callId=${req.callId} for ${key}; auto-denying`,
        );
        resolve("deny");
      }, APPROVAL_TIMEOUT_MS);

      this.pending.set(key, {
        callId: req.callId,
        botId: route.botId,
        handle: route.handle,
        chat: route.chat,
        resolve,
        timeout,
      });
    });

    // Fire-and-forget: if the prompt send fails, we still keep the
    // pending entry alive so a retried inbound or a manual reply can
    // still resolve it. The SDK cancels typing before calling us, so
    // we don't worry about that here.
    void this.sendPrompt(route.handle, route.chat, req).catch((err: unknown) => {
      this.logger.error(`weixin approval prompt send failed: ${String(err)}`);
    });

    return promise;
  }

  async onResolved(
    callId: string,
    decision: ApprovalDecision,
  ): Promise<void> {
    // Find the entry by callId (we key by user, not callId). If it's
    // still pending at this point the user never replied in time;
    // resolve it with the decision aura handed us.
    let matched: { key: string; entry: PendingEntry } | null = null;
    for (const [key, entry] of this.pending) {
      if (entry.callId === callId) {
        matched = { key, entry };
        break;
      }
    }
    if (!matched) return;
    clearTimeout(matched.entry.timeout);
    this.pending.delete(matched.key);
    // Only resolve the promise if it hasn't already been (race with a
    // user reply). The promise is idempotent via closure; calling
    // resolve twice is a no-op in JS.
    matched.entry.resolve(decision);
    try {
      await this.sendDecisionEcho(matched.entry, decision);
    } catch (err) {
      this.logger.debug(
        `weixin approval echo send failed: ${String(err)}`,
      );
    }
  }

  onBotStopped(botId: string): void {
    for (const [key, entry] of this.pending) {
      if (entry.botId !== botId) continue;
      clearTimeout(entry.timeout);
      this.pending.delete(key);
      entry.resolve("deny");
    }
  }

  onStop(): void {
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timeout);
      entry.resolve("deny");
    }
    this.pending.clear();
  }

  /**
   * Called by the platform's poll loop before forwarding inbound text
   * to the SDK. Returns `true` iff the message matched an outstanding
   * approval prompt, in which case the poll loop must NOT forward it
   * to the agent.
   */
  tryResolveInbound(botId: string, chat: WeixinChat, text: string): boolean {
    const key = chatKey(botId, chat);
    const entry = this.pending.get(key);
    if (!entry) return false;
    const decision = matchDecision(text);
    if (!decision) return false;
    clearTimeout(entry.timeout);
    this.pending.delete(key);
    entry.resolve(decision);
    return true;
  }

  private async sendPrompt(
    handle: WeixinBotHandle,
    chat: WeixinChat,
    req: ApprovalRequest,
  ): Promise<void> {
    const args = formatParamsPreview(req.paramsPreview);
    const lines: string[] = [
      "🔐 Tool approval required",
      "",
      `Tool: \`${req.tool}\``,
    ];
    if (args) {
      lines.push("", "Arguments:", args);
    }
    lines.push("", "Reply: yes / always / no");
    const text = lines.join("\n");
    const clientId = `aura-weixin-approval:${Date.now()}-${crypto
      .randomBytes(4)
      .toString("hex")}`;
    const contextToken = handle.state.contextTokens.get(chat.toUserId);
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
          item_list: [{ type: MessageItemType.TEXT, text_item: { text } }],
          ...(contextToken !== undefined ? { context_token: contextToken } : {}),
        },
      },
    });
  }

  private async sendDecisionEcho(
    entry: PendingEntry,
    decision: ApprovalDecision,
  ): Promise<void> {
    const label =
      decision === "approve"
        ? "✅ Approved"
        : decision === "approve_always"
          ? "✅ Approved (always)"
          : "🚫 Denied";
    const clientId = `aura-weixin-approval-echo:${Date.now()}-${crypto
      .randomBytes(4)
      .toString("hex")}`;
    const contextToken = entry.handle.state.contextTokens.get(entry.chat.toUserId);
    await sendMessage({
      baseUrl: entry.handle.state.baseUrl,
      token: entry.handle.state.botToken,
      body: {
        msg: {
          from_user_id: "",
          to_user_id: entry.chat.toUserId,
          client_id: clientId,
          message_type: MessageType.BOT,
          message_state: MessageState.FINISH,
          item_list: [{ type: MessageItemType.TEXT, text_item: { text: label } }],
          ...(contextToken !== undefined ? { context_token: contextToken } : {}),
        },
      },
    });
  }
}
