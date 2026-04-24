import type { Logger } from "@aura/channel-sdk";
import type { BotInboundEvent, BotStartHooks } from "@aura/channel-sdk/bot";

import { SESSION_EXPIRED_ERRCODE, pauseSession } from "../api/session-guard.js";
import { getUpdates } from "../api/endpoints.js";
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
        const text = extractPlainText(msg);
        if (!text) continue;
        const ev: BotInboundEvent<WeixinChat> = {
          chat: { toUserId: msg.from_user_id },
          platformUserId: msg.from_user_id,
          content: text,
        };
        hooks.emit(ev);
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
