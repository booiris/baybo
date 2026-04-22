import type {
  ApprovalDecision,
  ApprovalRequest,
  Logger,
} from "@aura/channel-sdk";
import type { Bot, Context } from "grammy";
import { InlineKeyboard } from "grammy";

type PendingEntry = {
  botId: string;
  chatId: number;
  messageId: number;
  resolve: (decision: ApprovalDecision) => void;
};

const CALLBACK_PREFIX = "aura:approve:";

type BotLookup = (botId: string) => Bot | undefined;

/**
 * Interactive approval broker.
 *
 * Each `Frame::ApprovalRequested` from aura becomes an inline-keyboard
 * prompt in the originating Telegram chat. A button tap resolves the
 * SDK's detached promise with the chosen decision; the SDK encodes it
 * as `ResolveApproval` on the wire.
 *
 * Routing: aura's `ApprovalRequest.userId` is the sidecar-composed
 * `tg_<bot>_<chat>_<user>` id. The broker uses `botByUser` to recover
 * which bot originated the user, then `lookupBot(botId)` to find the
 * running grammy instance that should post the prompt. Callback
 * queries from the user arrive on whatever bot posted the prompt, so
 * each bot gets its own `callbackQuery` handler installed by the
 * broker on first use.
 */
export class ApprovalBroker {
  private readonly pending = new Map<string, PendingEntry>();
  private readonly botsWithHandler = new Set<string>();

  constructor(
    private readonly botByUser: Map<string, string>,
    private readonly chatByUser: Map<string, number>,
    private readonly lookupBot: BotLookup,
    private readonly logger: Logger,
  ) {}

  /**
   * Post an approval prompt and wait for the user's tap. Fails closed
   * (resolves with `"deny"`) when the user / bot can't be located or
   * Telegram refuses the send — matches the SDK's behavior for handler
   * errors.
   */
  async request(req: ApprovalRequest): Promise<ApprovalDecision> {
    const botId = this.botByUser.get(req.userId);
    const chatId = this.chatByUser.get(req.userId);
    const bot = botId !== undefined ? this.lookupBot(botId) : undefined;
    if (!bot || chatId === undefined || botId === undefined) {
      this.logger.warn(
        "approval request with no routable chat; auto-denying",
        req.userId,
        req.callId,
      );
      return "deny";
    }
    this.ensureCallbackHandler(botId, bot);

    const text = formatPrompt(req);
    const keyboard = new InlineKeyboard()
      .text("Approve", `${CALLBACK_PREFIX}approve:${req.callId}`)
      .text("Approve always", `${CALLBACK_PREFIX}approve_always:${req.callId}`)
      .row()
      .text("Deny", `${CALLBACK_PREFIX}deny:${req.callId}`);

    let messageId: number;
    try {
      const sent = await bot.api.sendMessage(chatId, text, {
        reply_markup: keyboard,
      });
      messageId = sent.message_id;
    } catch (err) {
      this.logger.error("send approval prompt failed; auto-denying", err);
      return "deny";
    }

    return new Promise<ApprovalDecision>((resolve) => {
      this.pending.set(req.callId, { botId, chatId, messageId, resolve });
    });
  }

  async notifyResolved(
    callId: string,
    decision: ApprovalDecision,
  ): Promise<void> {
    const entry = this.pending.get(callId);
    if (!entry) return;
    this.pending.delete(callId);
    entry.resolve(decision);
    const bot = this.lookupBot(entry.botId);
    if (!bot) return;
    try {
      await bot.api.editMessageText(
        entry.chatId,
        entry.messageId,
        `${decisionIcon(decision)} ${decisionLabel(decision)}`,
      );
    } catch (err) {
      this.logger.debug("edit approval prompt on resolve failed", err);
    }
  }

  flushDenyAll(): void {
    for (const [, entry] of this.pending) {
      entry.resolve("deny");
    }
    this.pending.clear();
    this.botsWithHandler.clear();
  }

  private ensureCallbackHandler(botId: string, bot: Bot): void {
    if (this.botsWithHandler.has(botId)) return;
    this.botsWithHandler.add(botId);
    bot.callbackQuery(
      new RegExp(`^${CALLBACK_PREFIX}`),
      (ctx) => this.handleCallback(ctx),
    );
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
  const preview = req.paramsPreview.trim();
  const suffix = preview.length > 0 ? `\n\n${preview}` : "";
  return `🔐 Aura wants to run \`${req.tool}\`.${suffix}`;
}
