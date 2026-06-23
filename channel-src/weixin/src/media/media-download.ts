/**
 * Inbound media decode for Weixin. Picks the first downloadable media
 * item out of a `WeixinMessage`, downloads + AES-128-ECB decrypts it,
 * and returns the plaintext buffer ready for `uploadBlob` against the
 * Baybo gateway.
 *
 * Quoted media (`ref_msg`) is handled separately — the caller should
 * fall back to that only when the main item list has no media of its
 * own. Priority is IMAGE > VIDEO > FILE > VOICE, mirroring origin's
 * monitor loop so a multi-item message with both an image and a
 * voice-note returns the image first.
 */
import type { Logger } from "@baybo/channel-sdk";

import { downloadAndDecryptBuffer, downloadPlainCdnBuffer } from "../cdn/pic-decrypt.js";
import { MessageItemType, type MessageItem, type WeixinMessage } from "../api/types.js";

import { getMimeFromFilename } from "./mime.js";
import { silkToWav } from "./silk-to-wav.js";

/**
 * Decoded media buffer + the metadata the gateway upload needs. The
 * caller turns `bytes`/`mimeType` into a WireAttachment by uploading
 * to `POST /v1/blobs` and stashing the returned `blob_id`.
 */
export interface InboundMedia {
  /** What kind of attachment to mint on the wire. */
  kind: "image" | "audio" | "video" | "file";
  /** Plaintext bytes. Already decrypted from the CDN. */
  bytes: Buffer;
  /** MIME type. Image/Video/File mimes are inferred from the iLink
   * media kind; voice currently lands as `audio/silk` because we
   * haven't shipped silk → wav transcoding yet (P4.3). */
  mimeType: string;
  /** Optional original filename (`file_name`). Only set for FILE. */
  filename?: string;
}

/** Returns `true` if `item` is a media-bearing kind we know how to download. */
export function isMediaItem(item: MessageItem): boolean {
  const kind = item.type;
  return (
    kind === MessageItemType.IMAGE
    || kind === MessageItemType.VIDEO
    || kind === MessageItemType.FILE
    || kind === MessageItemType.VOICE
  );
}

/**
 * Short Chinese label for a media item kind. Used as a stand-in body
 * when media decode/upload fails (network, unpaired user, oversized
 * file, …) so a media-only message still lands at the gateway. Without
 * this, an unpaired user's first message being a bare image silently
 * vanishes and they never get the pairing code.
 */
export function mediaFallbackText(item: MessageItem): string {
  switch (item.type) {
    case MessageItemType.IMAGE:
      return "[图片]";
    case MessageItemType.VIDEO:
      return "[视频]";
    case MessageItemType.VOICE:
      return "[语音]";
    case MessageItemType.FILE: {
      const name = item.file_item?.file_name;
      return name ? `[文件: ${name}]` : "[文件]";
    }
    default:
      return "[媒体]";
  }
}

/**
 * Walk `msg.item_list` (and ref_msg as a last resort) and return the
 * first item that carries a downloadable media payload, in priority
 * order IMAGE > VIDEO > FILE > VOICE. `null` when nothing is
 * downloadable — the caller falls back to text-only handling.
 *
 * A VOICE item that already has `voice_item.text` (server-side STT)
 * is intentionally skipped from this picker — the text is the better
 * representation and the audio adds nothing for the agent.
 */
export function findDownloadableMedia(msg: WeixinMessage): MessageItem | null {
  const items = msg.item_list ?? [];
  const hasMedia = (m?: { encrypt_query_param?: string; full_url?: string }): boolean =>
    Boolean(m?.encrypt_query_param || m?.full_url);

  for (const t of [
    MessageItemType.IMAGE,
    MessageItemType.VIDEO,
    MessageItemType.FILE,
    MessageItemType.VOICE,
  ]) {
    const found = items.find((i) => {
      if (i.type !== t) return false;
      if (t === MessageItemType.IMAGE) return hasMedia(i.image_item?.media);
      if (t === MessageItemType.VIDEO) return hasMedia(i.video_item?.media);
      if (t === MessageItemType.FILE) return hasMedia(i.file_item?.media);
      // VOICE: skip when the server already sent us text.
      return hasMedia(i.voice_item?.media) && !i.voice_item?.text;
    });
    if (found) return found;
  }

  // No main media — try a quoted message's media.
  const ref = items.find((i) => i.ref_msg?.message_item)?.ref_msg?.message_item;
  if (ref && isMediaItem(ref)) return ref;
  return null;
}

/**
 * Download + decrypt a single media item.
 *
 * Returns `null` (rather than throwing) when the item can't be
 * downloaded — the inbound path is best-effort: a media-only message
 * that fails to decrypt should still surface to the agent as a text
 * description, not vanish entirely. Callers log + continue.
 */
export async function decodeMediaItem(
  item: MessageItem,
  cdnBaseUrl: string,
  logger: Logger,
): Promise<InboundMedia | null> {
  switch (item.type) {
    case MessageItemType.IMAGE:
      return decodeImage(item, cdnBaseUrl, logger);
    case MessageItemType.VIDEO:
      return decodeVideo(item, cdnBaseUrl, logger);
    case MessageItemType.FILE:
      return decodeFile(item, cdnBaseUrl, logger);
    case MessageItemType.VOICE:
      return decodeVoice(item, cdnBaseUrl, logger);
    default:
      return null;
  }
}

async function decodeImage(
  item: MessageItem,
  cdnBaseUrl: string,
  logger: Logger,
): Promise<InboundMedia | null> {
  const img = item.image_item;
  if (!img?.media || (!img.media.encrypt_query_param && !img.media.full_url)) return null;
  // Newer protocol: image_item.aeskey is a 32-char hex string. Convert
  // to the base64 encoding `parseAesKey` expects (it strips the
  // wrapper inside). Older protocol falls back to media.aes_key.
  const aesKeyBase64 = img.aeskey
    ? Buffer.from(img.aeskey, "hex").toString("base64")
    : img.media.aes_key;
  try {
    const bytes = aesKeyBase64
      ? await downloadAndDecryptBuffer({
          encryptedQueryParam: img.media.encrypt_query_param ?? "",
          aesKeyBase64,
          cdnBaseUrl,
          label: "inbound image",
          ...(img.media.full_url ? { fullUrl: img.media.full_url } : {}),
          logger,
        })
      : await downloadPlainCdnBuffer({
          encryptedQueryParam: img.media.encrypt_query_param ?? "",
          cdnBaseUrl,
          label: "inbound image-plain",
          ...(img.media.full_url ? { fullUrl: img.media.full_url } : {}),
          logger,
        });
    return { kind: "image", bytes, mimeType: "image/jpeg" };
  } catch (err) {
    logger.error(`inbound image decode failed: ${String(err)}`);
    return null;
  }
}

async function decodeVideo(
  item: MessageItem,
  cdnBaseUrl: string,
  logger: Logger,
): Promise<InboundMedia | null> {
  const v = item.video_item;
  if (!v?.media || (!v.media.encrypt_query_param && !v.media.full_url) || !v.media.aes_key) {
    return null;
  }
  try {
    const bytes = await downloadAndDecryptBuffer({
      encryptedQueryParam: v.media.encrypt_query_param ?? "",
      aesKeyBase64: v.media.aes_key,
      cdnBaseUrl,
      label: "inbound video",
      ...(v.media.full_url ? { fullUrl: v.media.full_url } : {}),
      logger,
    });
    return { kind: "video", bytes, mimeType: "video/mp4" };
  } catch (err) {
    logger.error(`inbound video decode failed: ${String(err)}`);
    return null;
  }
}

async function decodeFile(
  item: MessageItem,
  cdnBaseUrl: string,
  logger: Logger,
): Promise<InboundMedia | null> {
  const f = item.file_item;
  if (!f?.media || (!f.media.encrypt_query_param && !f.media.full_url) || !f.media.aes_key) {
    return null;
  }
  try {
    const bytes = await downloadAndDecryptBuffer({
      encryptedQueryParam: f.media.encrypt_query_param ?? "",
      aesKeyBase64: f.media.aes_key,
      cdnBaseUrl,
      label: "inbound file",
      ...(f.media.full_url ? { fullUrl: f.media.full_url } : {}),
      logger,
    });
    const filename = f.file_name ?? "file.bin";
    return {
      kind: "file",
      bytes,
      mimeType: getMimeFromFilename(filename),
      filename,
    };
  } catch (err) {
    logger.error(`inbound file decode failed: ${String(err)}`);
    return null;
  }
}

async function decodeVoice(
  item: MessageItem,
  cdnBaseUrl: string,
  logger: Logger,
): Promise<InboundMedia | null> {
  const v = item.voice_item;
  // VOICE with server-side STT: caller already used the text. We
  // shouldn't be here; defensive null.
  if (v?.text) return null;
  if (!v?.media || (!v.media.encrypt_query_param && !v.media.full_url) || !v.media.aes_key) {
    return null;
  }
  try {
    const silkBytes = await downloadAndDecryptBuffer({
      encryptedQueryParam: v.media.encrypt_query_param ?? "",
      aesKeyBase64: v.media.aes_key,
      cdnBaseUrl,
      label: "inbound voice",
      ...(v.media.full_url ? { fullUrl: v.media.full_url } : {}),
      logger,
    });
    // Transcode SILK → 24 kHz mono PCM WAV so downstream LLM/audio
    // pipelines (Whisper, gpt-4o-audio, …) accept the payload
    // directly. On transcode failure we fall back to the raw bytes
    // as `audio/silk` — the attachment still surfaces, just without
    // the universal-format guarantee.
    const wavBytes = await silkToWav(silkBytes, logger);
    return wavBytes !== null
      ? { kind: "audio", bytes: wavBytes, mimeType: "audio/wav" }
      : { kind: "audio", bytes: silkBytes, mimeType: "audio/silk" };
  } catch (err) {
    logger.error(`inbound voice decode failed: ${String(err)}`);
    return null;
  }
}
