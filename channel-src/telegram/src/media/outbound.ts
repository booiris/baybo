import type { Bot } from "grammy";
import { InputFile } from "grammy";

import type { WireAttachment } from "@aura/channel-sdk";

import type { TelegramChat } from "../platform.js";

/** Telegram caption ceiling per the Bot API: 1024 chars after entity
 * parsing. Cut at the codepoint boundary to avoid mid-grapheme breaks
 * — clients sometimes drop messages with malformed text. */
export const TELEGRAM_CAPTION_MAX = 1024;

/** Anything grammy's `InputFile` constructor accepts as the file source.
 * Streaming is the production path (`ReadableStream` from `fetchBlobStream`);
 * `Buffer`/`Uint8Array` is kept for tests and small inline payloads. */
export type AttachmentSource =
  | Uint8Array
  | ReadableStream<Uint8Array>
  | AsyncIterable<Uint8Array>;

/**
 * Dispatch a single attachment to the right `bot.api.sendXxx` method
 * based on the wire-level `kind` and the underlying MIME type. `caption`
 * is applied to this attachment only — the platform layer rotates it
 * onto the first attachment in a multi-attachment payload. Pass a
 * `ReadableStream` from `fetchBlobStream` to keep peak memory at one
 * chunk regardless of file size; grammy pipes the async iterable
 * straight into the multipart upload.
 */
export async function sendTelegramAttachment(
  bot: Bot,
  chat: TelegramChat,
  att: WireAttachment,
  source: AttachmentSource,
  caption: string,
): Promise<void> {
  const threadOpt =
    chat.threadId !== undefined ? { message_thread_id: chat.threadId } : {};
  const captionOpt = caption ? { caption: truncateCaption(caption) } : {};
  const inputName = att.filename ?? defaultFilename(att);
  const file = new InputFile(source, inputName);

  switch (att.kind) {
    case "image":
      await bot.api.sendPhoto(chat.chatId, file, {
        ...captionOpt,
        ...threadOpt,
      });
      return;
    case "audio":
      // Voice notes (`audio/ogg` with OPUS) render with the bubble
      // waveform; everything else is treated as music.
      if (isVoiceMime(att.mime_type)) {
        await bot.api.sendVoice(chat.chatId, file, {
          ...captionOpt,
          ...threadOpt,
        });
      } else {
        await bot.api.sendAudio(chat.chatId, file, {
          ...captionOpt,
          ...threadOpt,
        });
      }
      return;
    case "file":
      // Telegram differentiates video from "general file" at send-time:
      // sendVideo gets a streamable player; sendDocument is a download
      // tile. Pick by mime so an inbound video round-trips correctly.
      if (att.mime_type.startsWith("video/")) {
        await bot.api.sendVideo(chat.chatId, file, {
          ...captionOpt,
          ...threadOpt,
        });
      } else {
        await bot.api.sendDocument(chat.chatId, file, {
          ...captionOpt,
          ...threadOpt,
        });
      }
      return;
  }
}

function isVoiceMime(mime: string): boolean {
  // Telegram's voice-note format is OGG/Opus. Anything else (mp3,
  // m4a, wav) sounds wrong as a voice note and is safer as audio.
  return mime === "audio/ogg" || mime === "audio/opus";
}

function defaultFilename(att: WireAttachment): string {
  switch (att.kind) {
    case "image":
      return "image" + extensionFor(att.mime_type, ".jpg");
    case "audio":
      return "audio" + extensionFor(att.mime_type, ".ogg");
    case "file":
      return "file" + extensionFor(att.mime_type, "");
  }
}

function extensionFor(mime: string, fallback: string): string {
  const slash = mime.indexOf("/");
  if (slash < 0) return fallback;
  const sub = mime.slice(slash + 1);
  if (!sub) return fallback;
  return `.${sub.split(";")[0]?.trim() || fallback.replace(/^\./, "")}`;
}

function truncateCaption(s: string): string {
  if (s.length <= TELEGRAM_CAPTION_MAX) return s;
  // Slice by codepoint to avoid splitting a surrogate pair.
  return [...s].slice(0, TELEGRAM_CAPTION_MAX).join("");
}
