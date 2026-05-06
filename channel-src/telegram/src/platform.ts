import type {
  Logger,
  StartBotCommand,
  WireAttachment,
} from "@aura/channel-sdk";
import { fetchBlob, uploadBlob } from "@aura/channel-sdk";
import type {
  BotInboundEvent,
  BotMediaPayload,
  BotPlatform,
  BotStartHooks,
  SlashCommandSpec,
} from "@aura/channel-sdk/bot";
import { composeAuraUserId } from "@aura/channel-sdk/bot";
import { Bot, type Context, GrammyError, HttpError } from "grammy";

import {
  downloadTelegramFile,
  pickInboundMedia,
  type TelegramInboundMedia,
} from "./media/inbound.js";
import { sendTelegramAttachment } from "./media/outbound.js";
import { markdownToTelegram, TELEGRAM_PARSE_MODE } from "./markdown.js";

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
    await sendWithMarkdownFallback(
      text,
      (msg, parseOpts) =>
        bot.api.sendMessage(chat.chatId, msg, {
          ...parseOpts,
          ...threadOpts(chat),
        }),
      this.logger,
    );
  }

  /**
   * The thread id must be forwarded explicitly — `sendChatAction`
   * applies to a chat, and in a forum-style supergroup the indicator
   * would otherwise surface in the main topic instead of the one the
   * user is actually posting in.
   */
  async notifyTyping(bot: Bot, chat: TelegramChat): Promise<void> {
    await bot.api.sendChatAction(chat.chatId, "typing", threadOpts(chat));
  }

  async stopBot(bot: Bot): Promise<void> {
    if (bot.isRunning()) await bot.stop();
  }

  async registerSlashCommands(
    bot: Bot,
    commands: ReadonlyArray<SlashCommandSpec>,
  ): Promise<void> {
    // setMyCommands replaces the bot's command list; passing an empty
    // array would clear it, which is never what the SDK wants here
    // (it skips this method entirely on an empty manifest).
    await bot.api.setMyCommands(
      commands.map((c) => ({ command: c.command, description: c.description })),
    );
  }

  async startBot(
    cmd: StartBotCommand,
    hooks: BotStartHooks<Bot, TelegramChat>,
  ): Promise<{ handle: Bot; username?: string; waitForExit: Promise<void> }> {
    // sensitiveLogs makes grammy fold the underlying fetch error's
    // message into HttpError.message instead of dropping it. Bot tokens
    // never appear in those messages, so the "sensitive" name is
    // misleading here — the real win is that "sendPhoto failed!"
    // becomes "sendPhoto failed! ConnectTimeoutError: ...".
    const bot = new Bot(cmd.token, { client: { sensitiveLogs: true } });
    bot.on("message", (ctx) =>
      this.handleInboundMessage(bot, cmd.botId, ctx, hooks.emit),
    );
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

  async sendMedia(
    bot: Bot,
    chat: TelegramChat,
    payload: BotMediaPayload,
  ): Promise<void> {
    // Caption rides with the first attachment only so the user
    // doesn't see it duplicated across a multi-photo reply.
    let captionRemaining = payload.text;
    for (const att of payload.attachments) {
      const bytes = await this.openAttachmentBytes(att);
      if (!bytes) continue;
      try {
        await sendTelegramAttachment(bot, chat, att, bytes, captionRemaining);
        captionRemaining = "";
      } catch (err) {
        this.logger.error(
          `telegram sendMedia failed for kind=${att.kind} blob_id=${att.blob_id}: ${describeError(err)}`,
        );
      }
    }
    // If every attachment failed to fetch but a caption remains, fall
    // back to a plain text send so the conversation doesn't go silent.
    if (captionRemaining) {
      try {
        await this.sendText(bot, chat, captionRemaining);
      } catch (err) {
        this.logger.error(
          `telegram sendMedia text fallback failed: ${describeError(err)}`,
        );
      }
    }
  }

  // Buffer the whole blob before handing it to grammy. The streaming
  // path (fetchBlobStream → grammy InputFile around a web ReadableStream)
  // tripped over `yield* stream` inside grammy's multipart generator on
  // bun — grammy expects the stream to expose `[Symbol.asyncIterator]`
  // and got `undefined is not a function` instead. Bot API caps photos
  // at 10 MB and documents at 50 MB anyway, so peak memory is bounded.
  private async openAttachmentBytes(
    att: WireAttachment,
  ): Promise<Uint8Array | null> {
    try {
      const { bytes } = await fetchBlob(att.blob_id);
      return bytes;
    } catch (err) {
      this.logger.error(
        `telegram sendMedia: fetchBlob failed blob_id=${att.blob_id} err=${describeError(err)}`,
      );
      return null;
    }
  }

  private async handleInboundMessage(
    bot: Bot,
    botId: string,
    ctx: Context,
    emit: (ev: BotInboundEvent<TelegramChat>) => void,
  ): Promise<void> {
    const chat = ctx.chat;
    const from = ctx.from;
    const message = ctx.message;
    if (!chat || !from || !message) return;
    const threadId = message.message_thread_id;
    const address: TelegramChat =
      threadId !== undefined
        ? { chatId: chat.id, threadId }
        : { chatId: chat.id };

    // grammy acks each batch via offset, so a duplicate is rare in
    // steady-state — but a sidecar restart between long-poll fetch and
    // ack window can replay messages the server still holds. Use
    // `update.update_id` (bot-scoped, globally unique) as the dedup key
    // so the gateway can drop replays.
    const platformMsgId = String(ctx.update.update_id);

    const media = pickInboundMedia(message);
    if (media) {
      // Caption (text accompanying the media) or empty when the user
      // sent the media bare. We still upload the bytes either way so
      // the agent gets the full payload.
      const caption = message.caption ?? "";
      // The blob upload header must carry the SAME composite aura
      // user id `BotChannel.ingest` will use when emitting the inbound
      // event — both run through the gateway's pairing gate, and the
      // text-message identity is what the user already approved.
      // Passing the raw `from.id` would land the upload in a fresh
      // Pending row even when the chat is already paired.
      const auraUserId = composeAuraUserId(
        "telegram",
        botId,
        address,
        String(from.id),
      );
      const attachment = await this.downloadAndUpload(
        bot,
        botId,
        auraUserId,
        media,
      );
      if (!attachment) {
        // Surface a stub so the user's intent isn't silently lost.
        // This lands as a regular text inbound — not perfect, but
        // strictly better than dropping the turn entirely. The SDK
        // logs the resulting inbound event; the warn we emit inside
        // downloadAndUpload's "skipped" path tells the operator why
        // the agent saw text instead of media.
        const stub = mediaFallbackText(media);
        emit({
          chat: address,
          platformUserId: String(from.id),
          content: caption ? `${caption}\n${stub}` : stub,
          platformMsgId,
        });
        return;
      }
      emit({
        chat: address,
        platformUserId: String(from.id),
        content: caption,
        platformMsgId,
        attachments: [attachment],
      });
      return;
    }

    const text = message.text;
    if (text === undefined) return;
    emit({
      chat: address,
      platformUserId: String(from.id),
      content: text,
      platformMsgId,
    });
  }

  private async downloadAndUpload(
    bot: Bot,
    botId: string,
    userId: string,
    media: TelegramInboundMedia,
  ): Promise<WireAttachment | null> {
    let bytes: Buffer | null;
    try {
      bytes = await downloadTelegramFile(bot, media.fileId, media.size);
    } catch (err) {
      this.logger.error(`telegram inbound media download failed: ${describeError(err)}`);
      return null;
    }
    if (!bytes) {
      // Hit the 20 MB getFile ceiling; the platform layer falls back
      // to a text stub. Log so the operator can correlate with the
      // missing-attachment turn the user sees in chat.
      this.logger.warn(
        `telegram inbound media skipped (over getFile limit): kind=${media.kind} mime=${media.mimeType}` +
          (media.size !== undefined ? ` size=${media.size}` : ""),
      );
      return null;
    }
    this.logger.info(
      `telegram inbound media downloaded kind=${media.kind} mime=${media.mimeType} size=${bytes.length}`,
    );

    try {
      const { blobId } = await uploadBlob(bytes, media.mimeType, {
        botId,
        userId,
      });
      this.logger.info(
        `telegram inbound media uploaded blob_id=${blobId} bytes=${bytes.length}`,
      );
      return {
        kind: media.kind,
        blob_id: blobId,
        mime_type: media.mimeType,
        size: bytes.length,
        ...(media.filename ? { filename: media.filename } : {}),
      };
    } catch (err) {
      this.logger.error(`telegram inbound media upload failed: ${describeError(err)}`);
      return null;
    }
  }
}

/**
 * Build a one-line stub describing media that we couldn't actually
 * download (e.g. over Bot API's 20 MB getFile ceiling). The user sees
 * something like `[image: image/jpeg]` so the conversation keeps
 * flowing instead of looking like an empty turn.
 */
function mediaFallbackText(media: TelegramInboundMedia): string {
  const name = media.filename ? ` ${media.filename}` : "";
  return `[${media.kind}: ${media.mimeType}${name}]`;
}

export function threadOpts(
  chat: TelegramChat,
): { message_thread_id?: number } {
  return chat.threadId !== undefined ? { message_thread_id: chat.threadId } : {};
}

type ParseOpts = { parse_mode: typeof TELEGRAM_PARSE_MODE } | Record<string, never>;

/**
 * Send `text` formatted as MarkdownV2; on the Bot API's
 * `can't parse entities` 400 (and only that), retry with the original
 * plain text so the message still lands. Other errors propagate so
 * callers see real failures (chat not found, network, 401 …).
 */
export async function sendWithMarkdownFallback<R>(
  text: string,
  send: (text: string, parseOpts: ParseOpts) => Promise<R>,
  logger: Logger,
): Promise<R> {
  try {
    return await send(markdownToTelegram(text), {
      parse_mode: TELEGRAM_PARSE_MODE,
    });
  } catch (err) {
    if (!isParseEntitiesError(err)) throw err;
    logger.warn(
      `telegram MarkdownV2 parse failed; resending as plain text: ${describeError(err)}`,
    );
    return await send(text, {});
  }
}

function isParseEntitiesError(err: unknown): boolean {
  if (!(err instanceof GrammyError)) return false;
  if (err.error_code !== 400) return false;
  // Match on the `can't parse entities` prefix — the byte-offset
  // suffix varies, so equality wouldn't match.
  return err.description.toLowerCase().includes("can't parse entities");
}

/**
 * `String(err)` on grammy errors hides the real cause under
 * `err.error` (grammy) or `err.cause` (standard); walk both chains so
 * the warn-log surfaces every link.
 */
function describeError(err: unknown): string {
  if (err instanceof GrammyError) {
    return `GrammyError ${err.error_code} on ${err.method}: ${err.description}`;
  }
  const parts: string[] = [];
  let cur: unknown = err;
  for (let depth = 0; cur != null && depth < 4; depth++) {
    if (cur instanceof Error) {
      parts.push(`${cur.name}: ${cur.message}`);
      const next: unknown =
        cur instanceof HttpError ? cur.error : (cur as { cause?: unknown }).cause;
      cur = next === cur ? null : next;
    } else {
      parts.push(String(cur));
      cur = null;
    }
  }
  return parts.length > 0 ? parts.join(" <- ") : String(err);
}
