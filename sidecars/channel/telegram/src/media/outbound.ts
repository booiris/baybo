import type { Bot } from "grammy";
import { InputFile } from "grammy";

import type { Logger, WireAttachment } from "@baybo/channel-sdk";

import type { TELEGRAM_PARSE_MODE } from "../markdown.js";
import {
  sendWithMarkdownFallback,
  threadOpts,
  type TelegramChat,
} from "../platform.js";

/** Telegram caption ceiling per the Bot API: 1024 chars after entity
 * parsing. Cut at the codepoint boundary to avoid mid-grapheme breaks
 * — clients sometimes drop messages with malformed text. */
export const TELEGRAM_CAPTION_MAX = 1024;

/** Raster formats `sendPhoto` actually accepts. The wire's `image` kind is a
 * *rendering* hint — it means "a surface that draws images should draw this" —
 * so it also covers `image/svg+xml`, which `sendPhoto` rejects outright. Ship
 * anything outside this set as a document rather than lose the file. */
const TELEGRAM_PHOTO_MIMES = new Set([
  "image/jpeg",
  "image/png",
  "image/gif",
  "image/webp",
  "image/bmp",
]);

/** Anything grammy's `InputFile` constructor accepts as the file source.
 * We hand it a `Uint8Array` in production: grammy's bundled multipart
 * code runs on bun via `--target=bun`, and `yield* webReadableStream`
 * inside grammy throws `TypeError: undefined is not a function` there.
 * Buffering through `fetchBlob` (Bot API caps at 50 MB anyway) is the
 * stable path. */
export type AttachmentSource = Uint8Array;

/**
 * Dispatch a single attachment to the right `bot.api.sendXxx` method
 * based on the wire-level `kind` and the underlying MIME type. `caption`
 * is applied to this attachment only — the platform layer rotates it
 * onto the first attachment in a multi-attachment payload.
 */
export async function sendTelegramAttachment(
  bot: Bot,
  chat: TelegramChat,
  att: WireAttachment,
  source: AttachmentSource,
  caption: string,
  logger: Logger,
): Promise<void> {
  const thread = threadOpts(chat);
  const inputName = att.filename ?? defaultFilename(att);

  // A fresh `InputFile` per attempt: grammy consumes the source when it
  // builds the multipart body, so the MarkdownV2 retry below cannot reuse it.
  const dispatch = (captionOpt: CaptionOpts): Promise<unknown> => {
    const file = new InputFile(source, inputName);
    const opts = { ...captionOpt, ...thread };
    switch (att.kind) {
      case "image":
        return TELEGRAM_PHOTO_MIMES.has(att.mime_type)
          ? bot.api.sendPhoto(chat.chatId, file, opts)
          : bot.api.sendDocument(chat.chatId, file, opts);
      case "audio":
        // Voice notes (`audio/ogg` with OPUS) render with the bubble
        // waveform; everything else is treated as music.
        return isVoiceMime(att.mime_type)
          ? bot.api.sendVoice(chat.chatId, file, opts)
          : bot.api.sendAudio(chat.chatId, file, opts);
      case "file":
        // Telegram differentiates video from "general file" at send-time:
        // sendVideo gets a streamable player; sendDocument is a download
        // tile. Pick by mime so an inbound video round-trips correctly.
        return att.mime_type.startsWith("video/")
          ? bot.api.sendVideo(chat.chatId, file, opts)
          : bot.api.sendDocument(chat.chatId, file, opts);
    }
  };

  if (!caption) {
    await dispatch({});
    return;
  }
  // Truncate before converting: the 1024 cap is on entity-parsed
  // chars, not the raw `\.`-escaped form, so capping the source keeps
  // us safely under regardless of how many escapes the conversion adds.
  //
  // The caption is the agent's own reply prose, so it hits the same
  // `telegramify-markdown` output Telegram sometimes rejects with
  // `can't parse entities`. Without this fallback that 400 would kill the
  // attachment — the user would read the reply and never receive the file.
  await sendWithMarkdownFallback(
    truncateCaption(caption),
    (text, parseOpts) => dispatch({ caption: text, ...parseOpts }),
    logger,
  );
}

type CaptionOpts = { caption?: string; parse_mode?: typeof TELEGRAM_PARSE_MODE };

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
