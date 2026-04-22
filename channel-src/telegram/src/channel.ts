import type {
  AgentMessage,
  AgentNotice,
  ApprovalDecision,
  ApprovalRequest,
  BotStatusReport,
  Channel,
  Logger,
  StartBotCommand,
  StopBotCommand,
  UserInbound,
} from "@aura/channel-sdk";
import { Bot } from "grammy";

import { ApprovalBroker } from "./approvals.js";
import type { TelegramConfig } from "./config.js";

/**
 * Bounded in-process queue feeding `inbound()`. grammy's handlers run
 * on the bot's event loop; each inbound `message:text` pushes into
 * `pending`. The SDK drains via `inbound(signal)` until the signal
 * aborts.
 *
 * Capacity is intentionally generous — Telegram's per-bot message
 * rate is low, and dropping user input silently is worse than a brief
 * memory bump. If a hot chat ever fills the queue we log a warning
 * and drop the oldest entry.
 */
const QUEUE_CAPACITY = 1024;

/**
 * Compose an aura `user_id` for the `(bot, chat, user)` triple. Aura
 * keys sessions + memory on this value so each (bot × chat × user)
 * combination gets an isolated session — your DM with `@staging-bot`
 * never pollutes your DM with `@prod-bot`.
 */
export function userIdFor(botId: string, chatId: number, tgUserId: number): string {
  return `tg_${botId}_${chatId}_${tgUserId}`;
}

type RunningBot = {
  botId: string;
  bot: Bot;
  username?: string;
};

export class TelegramChannel implements Channel {
  readonly channelType = "telegram";

  private readonly bots = new Map<string, RunningBot>();
  private readonly approvals: ApprovalBroker;
  private readonly allowedUserIds: Set<number>;
  private readonly logger: Logger;

  /**
   * `aura_user_id` → Telegram `chat_id`. Populated on every inbound
   * message; consulted on every outbound to decide which chat to
   * reply in. Keyed by the full composite id so the same Telegram
   * user under two different bots shows up as two distinct entries.
   */
  private readonly chatByUser = new Map<string, number>();

  /**
   * `aura_user_id` → `bot_id`. Outbound frames from aura carry
   * `user_id` but no `bot_id`; the sidecar recovers the bot that
   * should deliver the reply by looking up which bot handed this
   * user to aura in the first place.
   */
  private readonly botByUser = new Map<string, string>();

  private readonly pending: UserInbound[] = [];
  private waiter: ((value: IteratorResult<UserInbound>) => void) | null = null;
  private closed = false;

  constructor(config: TelegramConfig, logger: Logger) {
    this.allowedUserIds = config.allowedUserIds;
    this.logger = logger;
    this.approvals = new ApprovalBroker(
      this.botByUser,
      this.chatByUser,
      (botId) => this.bots.get(botId)?.bot,
      logger,
    );
  }

  inbound(signal: AbortSignal): AsyncIterable<UserInbound> {
    const self = this;
    return {
      [Symbol.asyncIterator](): AsyncIterator<UserInbound> {
        let onAbort: (() => void) | null = null;
        if (signal.aborted) {
          self.closeQueue();
        } else {
          onAbort = () => self.closeQueue();
          signal.addEventListener("abort", onAbort, { once: true });
        }
        return {
          next(): Promise<IteratorResult<UserInbound>> {
            return self.nextInbound();
          },
          async return(): Promise<IteratorResult<UserInbound>> {
            if (onAbort) signal.removeEventListener("abort", onAbort);
            self.closeQueue();
            return { value: undefined, done: true };
          },
        };
      },
    };
  }

  async onMessage(msg: AgentMessage): Promise<void> {
    const chatId = this.chatByUser.get(msg.userId);
    const botId = this.botByUser.get(msg.userId);
    if (chatId === undefined || botId === undefined) {
      this.logger.warn(
        "agent message for unknown user; dropping",
        msg.userId,
      );
      return;
    }
    const running = this.bots.get(botId);
    if (!running) {
      this.logger.warn(
        "agent message for detached bot; dropping",
        botId,
        msg.userId,
      );
      return;
    }
    try {
      await running.bot.api.sendMessage(chatId, msg.content);
    } catch (err) {
      this.logger.error("sendMessage failed", err);
    }
  }

  async onNotice(notice: AgentNotice): Promise<void> {
    const chatId = this.chatByUser.get(notice.userId);
    const botId = this.botByUser.get(notice.userId);
    if (chatId === undefined || botId === undefined) {
      this.logger.debug("notice for unknown user; dropping", notice.userId);
      return;
    }
    const running = this.bots.get(botId);
    if (!running) return;
    const prefix = notice.level === "error" ? "❌" : "⚠️";
    try {
      await running.bot.api.sendMessage(chatId, `${prefix} ${notice.text}`);
    } catch (err) {
      this.logger.error("notice send failed", err);
    }
  }

  onApprovalRequested(req: ApprovalRequest): Promise<ApprovalDecision> {
    return this.approvals.request(req);
  }

  async onApprovalResolved(
    callId: string,
    decision: ApprovalDecision,
  ): Promise<void> {
    await this.approvals.notifyResolved(callId, decision);
  }

  async onStartBot(cmd: StartBotCommand): Promise<BotStatusReport> {
    const existing = this.bots.get(cmd.botId);
    if (existing) {
      // Idempotent: aura might retry a StartBot across reconciles.
      return {
        botId: cmd.botId,
        ok: true,
        message: `already running as @${existing.username ?? "?"}`,
      };
    }

    const bot = new Bot(cmd.token);
    bot.on("message:text", (ctx) => this.onTelegramText(cmd.botId, ctx));

    try {
      await bot.init();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return {
        botId: cmd.botId,
        ok: false,
        message: `init failed: ${message}`,
      };
    }

    const username = bot.botInfo?.username;
    this.bots.set(cmd.botId, { botId: cmd.botId, bot, username });
    // grammy's `start()` resolves only when polling stops; run it
    // detached and surface startup errors via the logger.
    void bot
      .start({
        onStart: (me) => {
          this.logger.info(
            `telegram bot '${cmd.botId}' polling as @${me.username}`,
          );
        },
      })
      .catch((err) => {
        this.logger.error(`bot '${cmd.botId}' polling exited`, err);
        this.bots.delete(cmd.botId);
      });

    return {
      botId: cmd.botId,
      ok: true,
      message: username ? `running as @${username}` : "running",
    };
  }

  async onStopBot(cmd: StopBotCommand): Promise<BotStatusReport> {
    const running = this.bots.get(cmd.botId);
    if (!running) {
      return {
        botId: cmd.botId,
        ok: true,
        message: "not running",
      };
    }
    this.bots.delete(cmd.botId);
    try {
      if (running.bot.isRunning()) {
        await running.bot.stop();
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return {
        botId: cmd.botId,
        ok: false,
        message: `stop failed: ${message}`,
      };
    }
    // Purge user / chat entries that belonged to this bot — outbound
    // frames after stop should find no route and drop cleanly.
    for (const [userId, botId] of this.botByUser.entries()) {
      if (botId === cmd.botId) {
        this.botByUser.delete(userId);
        this.chatByUser.delete(userId);
      }
    }
    return { botId: cmd.botId, ok: true };
  }

  async onStop(): Promise<void> {
    this.approvals.flushDenyAll();
    this.closeQueue();
    for (const running of this.bots.values()) {
      if (running.bot.isRunning()) {
        try {
          await running.bot.stop();
        } catch (err) {
          this.logger.debug("bot.stop threw", err);
        }
      }
    }
    this.bots.clear();
    this.botByUser.clear();
    this.chatByUser.clear();
  }

  // ---------------------------------------------------------------------

  private onTelegramText(botId: string, ctx: import("grammy").Context): void {
    const chat = ctx.chat;
    const from = ctx.from;
    const text = ctx.message?.text;
    if (!chat || !from || text === undefined) return;

    if (!this.isAuthorized(from.id)) {
      void this.refuseStranger(ctx);
      return;
    }

    const userId = userIdFor(botId, chat.id, from.id);
    this.chatByUser.set(userId, chat.id);
    this.botByUser.set(userId, botId);
    this.pushInbound({ sessionId: "", userId, content: text });
  }

  private isAuthorized(tgUserId: number): boolean {
    if (this.allowedUserIds.size === 0) return true;
    return this.allowedUserIds.has(tgUserId);
  }

  private async refuseStranger(ctx: import("grammy").Context): Promise<void> {
    try {
      await ctx.reply(
        "Sorry — this Aura bot is gated to a specific allowlist of Telegram accounts.",
      );
    } catch (err) {
      this.logger.debug("reply to stranger failed", err);
    }
  }

  private pushInbound(msg: UserInbound): void {
    if (this.closed) return;
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = null;
      waiter({ value: msg, done: false });
      return;
    }
    if (this.pending.length >= QUEUE_CAPACITY) {
      this.logger.warn(
        `inbound queue at capacity (${QUEUE_CAPACITY}); dropping oldest`,
      );
      this.pending.shift();
    }
    this.pending.push(msg);
  }

  private nextInbound(): Promise<IteratorResult<UserInbound>> {
    const ready = this.pending.shift();
    if (ready !== undefined) {
      return Promise.resolve({ value: ready, done: false });
    }
    if (this.closed) {
      return Promise.resolve({ value: undefined, done: true });
    }
    return new Promise<IteratorResult<UserInbound>>((resolve) => {
      this.waiter = resolve;
    });
  }

  private closeQueue(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = null;
      waiter({ value: undefined, done: true });
    }
  }
}
