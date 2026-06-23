import type { Logger, WireAttachment } from "@baybo/channel-sdk";
import { uploadBlob } from "@baybo/channel-sdk";
import type { BotInboundEvent, BotStartHooks } from "@baybo/channel-sdk/bot";
import { composeBayboUserId } from "@baybo/channel-sdk/bot";

import {
  SESSION_EXPIRED_ERRCODE,
  getRemainingPauseMs,
  isSessionPaused,
  pauseSession,
} from "../api/session-guard.js";
import { getUpdates } from "../api/endpoints.js";
import type { WeixinMessage } from "../api/types.js";
import {
  decodeMediaItem,
  findDownloadableMedia,
  mediaFallbackText,
  type InboundMedia,
} from "../media/media-download.js";
import { extractPlainText } from "../messaging/inbound.js";
import type { WeixinChat, WeixinBotHandle } from "../types.js";

const ERROR_BACKOFF_MS = 30_000;
const ERROR_THRESHOLD = 3;
// Wake interval while waiting out a SESSION_EXPIRED pause. The actual
// pause duration is set by `pauseSession` (1 hour today), but we wake
// every 60 s so an early resume (e.g. operator manually clears the
// pause via a future API) takes effect promptly. Cheap — one timer per
// bot, only while paused.
const PAUSE_RECHECK_MS = 60_000;

/**
 * Drives one bot's long-poll inbound loop. Resolves only when the abort
 * controller fires (operator-initiated stop or sidecar shutdown).
 *
 * SESSION_EXPIRED handling: errcode -14 marks the iLink session as
 * paused via `pauseSession` (1 h cooldown by default). Instead of
 * exiting and forcing the gateway to restart the bot — which would
 * just hit the same -14 again on the next reconcile tick and loop —
 * we sleep inside the loop until the cooldown elapses, then retry.
 * This way the bot stays alive in BotChannel routing and self-recovers
 * once the platform-side session is usable again.
 *
 * `get_updates_buf` is in-memory only; a sidecar restart replays the
 * most recent updates from an empty cursor. The gateway's
 * `(channel_type, bot_id, platform_msg_id)` dedup catches the replay.
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
    if (isSessionPaused(state.accountId)) {
      const remainingMs = getRemainingPauseMs(state.accountId);
      const sleepMs = Math.min(PAUSE_RECHECK_MS, Math.max(remainingMs, 1_000));
      logger.warn(
        `weixin bot '${state.accountId}' session paused (~${Math.ceil(remainingMs / 60_000)} min remaining); sleeping ${Math.ceil(sleepMs / 1000)}s before retry`,
      );
      try {
        await abortableSleep(sleepMs, state.abort.signal);
      } catch {
        return;
      }
      continue;
    }

    try {
      const resp = await getUpdates({
        baseUrl: state.baseUrl,
        token: state.botToken,
        get_updates_buf: state.getUpdatesBuf,
        signal: state.abort.signal,
      });

      if (resp.errcode === SESSION_EXPIRED_ERRCODE || resp.ret === SESSION_EXPIRED_ERRCODE) {
        logger.error(
          `weixin bot '${state.accountId}' session expired (errcode=${SESSION_EXPIRED_ERRCODE}); pausing — will resume after cooldown`,
        );
        pauseSession(state.accountId);
        // Loop top picks up the pause guard on the next iteration.
        continue;
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

  // Bare media + decode/upload failure (network, unpaired user, …)
  // would otherwise produce empty content + no attachment and we'd drop
  // the inbound silently. The pairing gate lives upstream of this
  // sidecar, so a swallowed media-only message means a brand-new user
  // never receives their pairing code. Surface a short stub instead so
  // the gateway sees a real frame.
  let content = text;
  if (mediaItem && !attachment && !content) {
    content = mediaFallbackText(mediaItem);
  }

  if (!content && !attachment) return;

  // iLink's `message_id` is unique per upstream message. Threading it
  // through enables the gateway's
  // `(channel_type, bot_id, platform_msg_id)` dedup so a sidecar
  // restart that replays `get_updates_buf` doesn't re-fire the agent
  // on every message in the buffer.
  const platformMsgId =
    msg.message_id !== undefined ? String(msg.message_id) : undefined;

  const ev: BotInboundEvent<WeixinChat> = {
    chat: { toUserId: fromUserId },
    platformUserId: fromUserId,
    content,
    ...(platformMsgId ? { platformMsgId } : {}),
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
    // Match `BotChannel.ingest`'s composite baybo user id so the
    // gateway's pairing gate sees the same identity for the upload
    // as for the text frame the user has already approved. Weixin
    // overrides `chatKey` to `chat.toUserId`, so pass that through.
    const bayboUserId = composeBayboUserId(
      "weixin",
      state.accountId,
      { toUserId: fromUserId },
      fromUserId,
      (chat) => chat.toUserId,
    );
    const { blobId } = await uploadBlob(media.bytes, media.mimeType, {
      botId: state.accountId,
      userId: bayboUserId,
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
