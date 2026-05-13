import type {
  ApprovalDecision,
  ApprovalRequest,
  Logger,
  ResourceAccess,
} from "@aura/channel-sdk";
import type { BotRoute, PlatformApprovals } from "@aura/channel-sdk/bot";
import type { Bot, Context } from "grammy";
import { InlineKeyboard } from "grammy";

import {
  sendWithMarkdownFallback,
  threadOpts,
  type TelegramChat,
} from "./platform.js";

type PendingEntry = {
  botId: string;
  handle: Bot;
  chat: TelegramChat;
  messageId: number;
  resolve: (decision: ApprovalDecision) => void;
};

const CALLBACK_PREFIX = "aura:approve:";

/**
 * Interactive approval broker.
 *
 * Each `Frame::ApprovalRequested` from aura becomes an inline-keyboard
 * prompt in the originating Telegram chat. A button tap resolves the
 * SDK's detached promise with the chosen decision; the SDK encodes it
 * as `ResolveApproval` on the wire.
 *
 * The SDK hands us the already-resolved {@link BotRoute} so we never
 * need to touch its routing tables. The bot handle is captured into
 * the pending entry, so `onResolved` can edit the prompt even after
 * the user's route has been purged by a StopBot.
 */
export class TelegramApprovals
  implements PlatformApprovals<Bot, TelegramChat>
{
  private readonly pending = new Map<string, PendingEntry>();

  constructor(private readonly logger: Logger) {}

  attach(bot: Bot): void {
    bot.callbackQuery(
      new RegExp(`^${CALLBACK_PREFIX}`),
      (ctx) => this.handleCallback(ctx),
    );
  }

  async onRequested(
    req: ApprovalRequest,
    route: BotRoute<Bot, TelegramChat> | null,
  ): Promise<ApprovalDecision> {
    if (!route) {
      this.logger.warn(
        "approval request with no routable chat; auto-denying",
        req.userId,
        req.callId,
      );
      return "deny";
    }

    const text = formatPrompt(req);
    const keyboard = new InlineKeyboard()
      .text("Approve", `${CALLBACK_PREFIX}approve:${req.callId}`)
      .text("Approve always", `${CALLBACK_PREFIX}approve_always:${req.callId}`)
      .row()
      .text("Deny", `${CALLBACK_PREFIX}deny:${req.callId}`);

    let messageId: number;
    try {
      const sent = await sendWithMarkdownFallback(
        text,
        (msg, parseOpts) =>
          route.handle.api.sendMessage(route.chat.chatId, msg, {
            ...parseOpts,
            reply_markup: keyboard,
            ...threadOpts(route.chat),
          }),
        this.logger,
      );
      messageId = sent.message_id;
    } catch (err) {
      this.logger.error("send approval prompt failed; auto-denying", err);
      return "deny";
    }

    return new Promise<ApprovalDecision>((resolve) => {
      this.pending.set(req.callId, {
        botId: route.botId,
        handle: route.handle,
        chat: route.chat,
        messageId,
        resolve,
      });
    });
  }

  onBotStopped(botId: string): void {
    for (const [callId, entry] of this.pending) {
      if (entry.botId !== botId) continue;
      this.pending.delete(callId);
      entry.resolve("deny");
    }
  }

  async onResolved(
    callId: string,
    decision: ApprovalDecision,
  ): Promise<void> {
    const entry = this.pending.get(callId);
    if (!entry) return;
    this.pending.delete(callId);
    entry.resolve(decision);
    try {
      // editMessageText identifies the target by (chat_id, message_id);
      // thread is already encoded in the original message, so no
      // message_thread_id is needed here.
      await entry.handle.api.editMessageText(
        entry.chat.chatId,
        entry.messageId,
        `${decisionIcon(decision)} ${decisionLabel(decision)}`,
      );
    } catch (err) {
      this.logger.debug("edit approval prompt on resolve failed", err);
    }
  }

  onStop(): void {
    for (const [, entry] of this.pending) {
      entry.resolve("deny");
    }
    this.pending.clear();
  }

  private async handleCallback(ctx: Context): Promise<void> {
    const data = ctx.callbackQuery?.data;
    if (!data) return;
    const rest = data.slice(CALLBACK_PREFIX.length);
    const colon = rest.indexOf(":");
    if (colon < 0) {
      await ctx.answerCallbackQuery({ text: "invalid payload" });
      return;
    }
    const action = rest.slice(0, colon);
    const callId = rest.slice(colon + 1);
    const decision = toDecision(action);
    if (!decision) {
      await ctx.answerCallbackQuery({ text: "invalid action" });
      return;
    }

    const entry = this.pending.get(callId);
    if (!entry) {
      await ctx.answerCallbackQuery({ text: "already resolved" });
      return;
    }

    this.pending.delete(callId);
    entry.resolve(decision);
    await ctx.answerCallbackQuery({ text: decisionLabel(decision) });
    try {
      await ctx.editMessageText(
        `${decisionIcon(decision)} ${decisionLabel(decision)}`,
      );
    } catch (err) {
      this.logger.debug("edit approval prompt on tap failed", err);
    }
  }
}

function toDecision(action: string): ApprovalDecision | null {
  if (
    action === "approve" ||
    action === "approve_always" ||
    action === "deny"
  ) {
    return action;
  }
  return null;
}

function decisionLabel(decision: ApprovalDecision): string {
  switch (decision) {
    case "approve":
      return "Approved";
    case "approve_always":
      return "Approved (always)";
    case "deny":
      return "Denied";
  }
}

function decisionIcon(decision: ApprovalDecision): string {
  switch (decision) {
    case "approve":
    case "approve_always":
      return "✅";
    case "deny":
      return "🚫";
  }
}

function formatPrompt(req: ApprovalRequest): string {
  const lines = [`🔐 Aura wants to run \`${req.tool}\`.`];
  const body = formatBody(req);
  if (body.length > 0) {
    lines.push("", ...body);
  }
  return lines.join("\n");
}

// Render the "what is being approved" section. Accesses are the
// security-relevant primary surface (one bullet per controlled
// resource). `description` is the tool's optional human-readable
// label. We fall back to `paramsPreview` only when neither was
// supplied — many tools (snapshot / get_images / …) declare no
// resources and no label, in which case the JSON shape is the only
// signal we have.
function formatBody(req: ApprovalRequest): string[] {
  const out: string[] = [];
  for (const acc of req.accesses) {
    out.push(`  • ${formatAccess(acc)}`);
  }
  const desc = req.description?.trim();
  if (desc !== undefined && desc.length > 0) {
    out.push(`  ${desc}`);
  }
  if (out.length > 0) return out;
  const preview = req.paramsPreview.trim();
  return preview.length > 0 ? [preview] : [];
}

function formatAccess(acc: ResourceAccess): string {
  switch (acc.kind) {
    case "read_file":
      return `will read ${acc.path}`;
    case "write_file":
      return `will write ${acc.path}`;
    case "http":
      return acc.host === "*"
        ? "needs network access"
        : `will reach ${acc.host}`;
    case "exec_command":
      return `will run: ${acc.command}`;
    case "env":
      return acc.vars.length === 0
        ? "will read environment"
        : `will read env: ${acc.vars.join(", ")}`;
  }
}
