import type { Bot } from "grammy";
import type {
  Audio,
  Document,
  Message,
  PhotoSize,
  Video,
  Voice,
} from "grammy/types";

/**
 * Inbound media descriptor extracted from a Telegram `Message`. The
 * caller fetches the bytes via {@link downloadTelegramFile} and then
 * uploads them to the gateway as a `WireAttachment`.
 */
export interface TelegramInboundMedia {
  kind: "image" | "audio" | "file";
  fileId: string;
  mimeType: string;
  filename?: string;
  /** Reported size in bytes. Used to short-circuit getFile (Bot API
   * caps download at 20 MB) before we round-trip to Telegram. */
  size?: number;
}

/**
 * Bot API's `getFile` endpoint refuses payloads larger than 20 MB. A
 * larger document still surfaces in updates — but we can only emit a
 * text-stub for it. We treat anything strictly above this as
 * non-downloadable.
 */
export const BOT_API_DOWNLOAD_LIMIT = 20 * 1024 * 1024;

/**
 * Pick the most interesting media payload off a Telegram inbound
 * message. Telegram delivers at most one media field per message in
 * practice, but a document-style animation still populates both
 * `animation` and `document`, so the priority below disambiguates.
 *
 * Priority (most specific representation first):
 *   photo → video → voice → audio → document
 *
 * Returns `null` when the message has no media we know how to ship,
 * including stickers, contacts, locations, polls, and service messages.
 */
export function pickInboundMedia(
  message: Message,
): TelegramInboundMedia | null {
  if (message.photo && message.photo.length > 0) {
    const largest = pickLargestPhoto(message.photo);
    if (largest) {
      return {
        kind: "image",
        fileId: largest.file_id,
        mimeType: "image/jpeg",
        ...(largest.file_size !== undefined ? { size: largest.file_size } : {}),
      };
    }
  }
  if (message.video) {
    return videoToMedia(message.video);
  }
  if (message.voice) {
    return voiceToMedia(message.voice);
  }
  if (message.audio) {
    return audioToMedia(message.audio);
  }
  if (message.document) {
    return documentToMedia(message.document);
  }
  return null;
}

/**
 * Telegram's `photo` array is the same image at several thumbnail
 * sizes. The caption-bearing original is the largest entry, which
 * conventionally sits last but isn't guaranteed to — pick by reported
 * area, falling back to file_size, then to array order.
 */
function pickLargestPhoto(photos: PhotoSize[]): PhotoSize | null {
  let best: PhotoSize | null = null;
  let bestScore = -1;
  for (const p of photos) {
    const score = p.width * p.height;
    if (score > bestScore) {
      best = p;
      bestScore = score;
    }
  }
  return best;
}

function videoToMedia(v: Video): TelegramInboundMedia {
  return {
    kind: "file",
    fileId: v.file_id,
    mimeType: v.mime_type ?? "video/mp4",
    ...(v.file_name ? { filename: v.file_name } : {}),
    ...(v.file_size !== undefined ? { size: v.file_size } : {}),
  };
}

function voiceToMedia(v: Voice): TelegramInboundMedia {
  return {
    kind: "audio",
    fileId: v.file_id,
    mimeType: v.mime_type ?? "audio/ogg",
    ...(v.file_size !== undefined ? { size: v.file_size } : {}),
  };
}

function audioToMedia(a: Audio): TelegramInboundMedia {
  return {
    kind: "audio",
    fileId: a.file_id,
    mimeType: a.mime_type ?? "audio/mpeg",
    ...(a.file_name ? { filename: a.file_name } : {}),
    ...(a.file_size !== undefined ? { size: a.file_size } : {}),
  };
}

function documentToMedia(d: Document): TelegramInboundMedia {
  return {
    kind: "file",
    fileId: d.file_id,
    mimeType: d.mime_type ?? "application/octet-stream",
    ...(d.file_name ? { filename: d.file_name } : {}),
    ...(d.file_size !== undefined ? { size: d.file_size } : {}),
  };
}

/**
 * Pull the bytes for a `file_id` from the Bot API's file storage.
 * Returns `null` when the payload is over the Bot API's 20 MB
 * `getFile` ceiling — the caller should fall back to a text stub.
 */
export async function downloadTelegramFile(
  bot: Bot,
  fileId: string,
  size: number | undefined,
  signal?: AbortSignal,
): Promise<Buffer | null> {
  if (size !== undefined && size > BOT_API_DOWNLOAD_LIMIT) {
    return null;
  }
  // grammy's `getFile` types its signal against an `abort-controller`
  // polyfill instead of Node's standard `AbortSignal`. The two are
  // structurally identical at runtime; cast through `unknown` to
  // satisfy the older shim's stricter event-target generics.
  const file = await bot.api.getFile(
    fileId,
    signal as unknown as Parameters<typeof bot.api.getFile>[1],
  );
  if (!file.file_path) {
    throw new Error(`telegram getFile returned no file_path for ${fileId}`);
  }
  const url = `https://api.telegram.org/file/bot${bot.token}/${file.file_path}`;
  const init: { signal?: AbortSignal } = {};
  if (signal) init.signal = signal;
  const res = await fetch(url, init);
  if (!res.ok) {
    throw new Error(
      `telegram file download failed: ${res.status} ${res.statusText} (${url.replace(bot.token, "<token>")})`,
    );
  }
  return Buffer.from(await res.arrayBuffer());
}
