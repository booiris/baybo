import type { Logger, StartBotCommand } from "@aura/channel-sdk";
import type {
  BotInboundEvent,
  BotPlatform,
  BotStartHooks,
} from "@aura/channel-sdk/bot";
import { Bot, type Context } from "grammy";

/**
 * Telegram conversation address. `chatId` is the chat/supergroup;
 * `threadId` (aka `message_thread_id`) is the forum topic in a
 * topics-enabled supergroup. In DMs and non-forum groups, `threadId`
 * is absent. Two messages from the same user in the same supergroup
 * but different topics must be treated as separate sessions, so
 * `threadId` participates in both `composeUserId` and every outbound
 * send.
 */
export interface TelegramChat {
  chatId: number;
  threadId?: number;
}

export class TelegramPlatform implements BotPlatform<Bot, TelegramChat> {
  constructor(private readonly logger: Logger) {}

  async sendText(bot: Bot, chat: TelegramChat, text: string): Promise<void> {
    await bot.api.sendMessage(
      chat.chatId,
      text,
      chat.threadId !== undefined ? { message_thread_id: chat.threadId } : {},
    );
  }

  /**
   * The thread id must be forwarded explicitly — `sendChatAction`
   * applies to a chat, and in a forum-style supergroup the indicator
   * would otherwise surface in the main topic instead of the one the
   * user is actually posting in.
   */
  async notifyTyping(bot: Bot, chat: TelegramChat): Promise<void> {
    await bot.api.sendChatAction(
      chat.chatId,
      "typing",
      chat.threadId !== undefined ? { message_thread_id: chat.threadId } : {},
    );
  }

  async stopBot(bot: Bot): Promise<void> {
    if (bot.isRunning()) await bot.stop();
  }

  async startBot(
    cmd: StartBotCommand,
    hooks: BotStartHooks<Bot, TelegramChat>,
  ): Promise<{ handle: Bot; username?: string; waitForExit: Promise<void> }> {
    const bot = new Bot(cmd.token);
    bot.on("message:text", (ctx) => this.handleInboundText(ctx, hooks.emit));
    // grammy freezes its middleware tree at bot.start(); approvals
    // must register their callbackQuery handler before that.
    await hooks.attach(bot);
    await bot.init();
    const username = bot.botInfo?.username;
    // grammy's bot.start() resolves when polling terminates — exactly
    // the contract the SDK expects for `waitForExit`, so hand it
    // through directly instead of wrapping it in a callback.
    const waitForExit = bot.start({
      onStart: (me) => {
        this.logger.info(
          `telegram bot '${cmd.botId}' polling as @${me.username}`,
        );
      },
    });
    return username !== undefined
      ? { handle: bot, username, waitForExit }
      : { handle: bot, waitForExit };
  }

  private handleInboundText(
    ctx: Context,
    emit: (ev: BotInboundEvent<TelegramChat>) => void,
  ): void {
    const chat = ctx.chat;
    const from = ctx.from;
    const text = ctx.message?.text;
    if (!chat || !from || text === undefined) return;
    const threadId = ctx.message?.message_thread_id;
    const address: TelegramChat =
      threadId !== undefined
        ? { chatId: chat.id, threadId }
        : { chatId: chat.id };
    emit({
      chat: address,
      platformUserId: String(from.id),
      content: text,
    });
  }
}
