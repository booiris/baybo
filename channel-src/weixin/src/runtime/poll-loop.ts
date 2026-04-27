import type { Logger, WireAttachment } from "@aura/channel-sdk";
import { uploadBlob } from "@aura/channel-sdk";
import type { BotInboundEvent, BotStartHooks } from "@aura/channel-sdk/bot";
import { composeAuraUserId } from "@aura/channel-sdk/bot";

import { SESSION_EXPIRED_ERRCODE, pauseSession } from "../api/session-guard.js";
import { getUpdates } from "../api/endpoints.js";
import type { WeixinMessage } from "../api/types.js";
import {
  decodeMediaItem,
  findDownloadableMedia,
  type InboundMedia,
} from "../media/media-download.js";
import { extractPlainText } from "../messaging/inbound.js";
import type { WeixinChat, WeixinBotHandle } from "../types.js";

const ERROR_BACKOFF_MS = 30_000;
const ERROR_THRESHOLD = 3;

/**
 * Drives one bot's long-poll inbound loop. Resolves when the abort
 * controller fires or when the server returns errcode -14 (session
 * expired) — both are terminal for this bot and BotChannel's
 * `waitForExit` contract treats a resolved promise as the bot having
 * stopped.
 *
 * `get_updates_buf` is in-memory only; a sidecar restart replays the
 * most recent updates from an empty cursor. Downstream session
 * dedup in aura handles the duplicates.
 *
 * Media is decoded inline: the first downloadable item is decrypted
 * via the iLink CDN, uploaded to the gateway's blob store, and
 * surfaced as a `WireAttachment` on the inbound event. Failure to
 * decode media is non-fatal — the text body still flows through and
 * the agent loses only the binary payload, with an error logged.
 */
export async function runPollLoop(
  handle: WeixinBotHandle,
  hooks: BotStartHooks<WeixinBotHandle, WeixinChat>,
  logger: Logger,
): Promise<void> {
  const { state } = handle;
  let consecutiveErrors = 0;

  logger.info(
    `weixin bot '${state.accountId}' long-poll started (baseUrl=${state.baseUrl})`,
  );

  while (!state.abort.signal.aborted) {
    try {
      const resp = await getUpdates({
        baseUrl: state.baseUrl,
        token: state.botToken,
        get_updates_buf: state.getUpdatesBuf,
        signal: state.abort.signal,
      });

      if (resp.errcode === SESSION_EXPIRED_ERRCODE || resp.ret === SESSION_EXPIRED_ERRCODE) {
        logger.error(
          `weixin bot '${state.accountId}' session expired (errcode=${SESSION_EXPIRED_ERRCODE}); pausing & exiting poll loop`,
        );
        pauseSession(state.accountId);
        return;
      }

      if (resp.get_updates_buf !== undefined) {
        state.getUpdatesBuf = resp.get_updates_buf;
      }

      consecutiveErrors = 0;

      for (const msg of resp.msgs ?? []) {
        if (!msg.from_user_id) continue;
        if (msg.context_token) {
          state.contextTokens.set(msg.from_user_id, msg.context_token);
        }
        await dispatchInboundMessage(handle, hooks, logger, msg);
      }
    } catch (err) {
      if (state.abort.signal.aborted) return;
      consecutiveErrors++;
      logger.warn(
        `weixin bot '${state.accountId}' getUpdates failed (${consecutiveErrors}/${ERROR_THRESHOLD}): ${String(err)}`,
      );
      if (consecutiveErrors >= ERROR_THRESHOLD) {
        logger.warn(
          `weixin bot '${state.accountId}' backing off ${ERROR_BACKOFF_MS}ms after ${consecutiveErrors} errors`,
        );
        try {
          await abortableSleep(ERROR_BACKOFF_MS, state.abort.signal);
        } catch {
          return;
        }
        consecutiveErrors = 0;
      }
    }
  }
}

async function dispatchInboundMessage(
  handle: WeixinBotHandle,
  hooks: BotStartHooks<WeixinBotHandle, WeixinChat>,
  logger: Logger,
  msg: WeixinMessage,
): Promise<void> {
  const { state } = handle;
  const fromUserId = msg.from_user_id ?? "";

  const text = extractPlainText(msg);
  const mediaItem = findDownloadableMedia(msg);
  let attachment: WireAttachment | undefined;
  if (mediaItem) {
    attachment = await downloadAndUpload(handle, logger, msg, mediaItem);
  }

  // A bare image with no caption produces empty text + an attachment.
  // We still emit so the agent sees the media; pure no-ops (no text
  // and no media) drop here.
  if (!text && !attachment) return;

  const ev: BotInboundEvent<WeixinChat> = {
    chat: { toUserId: fromUserId },
    platformUserId: fromUserId,
    content: text,
    ...(attachment ? { attachments: [attachment] } : {}),
  };
  // Suppress lint about unused state — state is used implicitly via
  // the handle reference threaded through `downloadAndUpload`.
  void state;
  hooks.emit(ev);
}

async function downloadAndUpload(
  handle: WeixinBotHandle,
  logger: Logger,
  msg: WeixinMessage,
  mediaItem: ReturnType<typeof findDownloadableMedia> & object,
): Promise<WireAttachment | undefined> {
  const { state } = handle;
  let media: InboundMedia | null;
  try {
    media = await decodeMediaItem(mediaItem, state.cdnBaseUrl, logger);
  } catch (err) {
    logger.error(`weixin inbound media decode threw: ${String(err)}`);
    return undefined;
  }
  if (!media) return undefined;

  try {
    const fromUserId = msg.from_user_id ?? "";
    // Match `BotChannel.ingest`'s composite aura user id so the
    // gateway's pairing gate sees the same identity for the upload
    // as for the text frame the user has already approved. Weixin
    // overrides `chatKey` to `chat.toUserId`, so pass that through.
    const auraUserId = composeAuraUserId(
      "weixin",
      state.accountId,
      { toUserId: fromUserId },
      fromUserId,
      (chat) => chat.toUserId,
    );
    const { blobId } = await uploadBlob(media.bytes, media.mimeType, {
      botId: state.accountId,
      userId: auraUserId,
    });
    return {
      kind: media.kind === "video" ? "file" : media.kind, // wire side only knows image/audio/file
      blob_id: blobId,
      mime_type: media.mimeType,
      size: media.bytes.length,
      ...(media.filename ? { filename: media.filename } : {}),
    };
  } catch (err) {
    logger.error(`weixin inbound media upload failed: ${String(err)}`);
    return undefined;
  }
}

function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(new Error("aborted"));
      return;
    }
    const t = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(t);
      reject(new Error("aborted"));
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}
