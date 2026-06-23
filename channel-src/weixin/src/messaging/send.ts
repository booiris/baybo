/**
 * Outbound iLink message builders for image / video / file payloads.
 * Each helper takes a previously-uploaded `UploadedFileInfo` (from
 * `cdn/upload.ts`) and constructs the matching MessageItem, then
 * sends it as its own `sendMessage` request — iLink expects exactly
 * one item per message, so a caption + media is always two requests.
 *
 * Pure-text outbound stays in `WeixinPlatform.sendText` (no caption
 * → no media) since it has its own context-token + typing-cancel
 * orchestration.
 */
import crypto from "node:crypto";

import type { Logger } from "@baybo/channel-sdk";

import { sendMessage } from "../api/endpoints.js";
import type { WeixinApiOptions } from "../api/endpoints.js";
import {
  MessageItemType,
  MessageState,
  MessageType,
  type MessageItem,
  type SendMessageReq,
} from "../api/types.js";
import type { UploadedFileInfo } from "../cdn/upload.js";

function generateClientId(): string {
  // iLink uses `client_id` for server-side dedup. A random 8-byte hex
  // suffix keeps it unique across all sidecar instances on a host —
  // the random part is cheap and the timestamp is a debug hint.
  return `baybo-weixin:${Date.now()}-${crypto.randomBytes(4).toString("hex")}`;
}

/**
 * Send a sequence of items to one recipient — typically `[TEXT?, MEDIA]`.
 * Each item rides its own `sendMessage` request because iLink demands
 * exactly one entry in `item_list` per call.
 */
async function sendItems(params: {
  to: string;
  items: MessageItem[];
  opts: WeixinApiOptions & { contextToken?: string };
  label: string;
  logger: Logger;
}): Promise<{ messageId: string }> {
  const { to, items, opts, label, logger } = params;
  let lastClientId = "";
  for (const item of items) {
    lastClientId = generateClientId();
    const req: SendMessageReq = {
      msg: {
        from_user_id: "",
        to_user_id: to,
        client_id: lastClientId,
        message_type: MessageType.BOT,
        message_state: MessageState.FINISH,
        item_list: [item],
        ...(opts.contextToken !== undefined ? { context_token: opts.contextToken } : {}),
      },
    };
    try {
      await sendMessage({
        baseUrl: opts.baseUrl,
        ...(opts.token !== undefined ? { token: opts.token } : {}),
        ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
        body: req,
      });
    } catch (err) {
      logger.error(`${label}: failed to=${to} clientId=${lastClientId} err=${String(err)}`);
      throw err;
    }
  }
  logger.info(`${label}: success to=${to} clientId=${lastClientId}`);
  return { messageId: lastClientId };
}

function imageItem(uploaded: UploadedFileInfo): MessageItem {
  return {
    type: MessageItemType.IMAGE,
    image_item: {
      media: {
        encrypt_query_param: uploaded.downloadEncryptedQueryParam,
        // `aeskey` from the upload pipeline is hex; iLink expects
        // `CDNMedia.aes_key` to be base64 of either raw 16 bytes or
        // a 32-char hex ASCII string. The latter is what older
        // clients ship, so we keep parity with origin and base64
        // the hex string itself.
        aes_key: Buffer.from(uploaded.aeskey).toString("base64"),
        encrypt_type: 1,
      },
      mid_size: uploaded.fileSizeCiphertext,
    },
  };
}

function videoItem(uploaded: UploadedFileInfo): MessageItem {
  return {
    type: MessageItemType.VIDEO,
    video_item: {
      media: {
        encrypt_query_param: uploaded.downloadEncryptedQueryParam,
        aes_key: Buffer.from(uploaded.aeskey).toString("base64"),
        encrypt_type: 1,
      },
      video_size: uploaded.fileSizeCiphertext,
    },
  };
}

function fileItem(uploaded: UploadedFileInfo, fileName: string): MessageItem {
  return {
    type: MessageItemType.FILE,
    file_item: {
      media: {
        encrypt_query_param: uploaded.downloadEncryptedQueryParam,
        aes_key: Buffer.from(uploaded.aeskey).toString("base64"),
        encrypt_type: 1,
      },
      file_name: fileName,
      len: String(uploaded.fileSize),
    },
  };
}

/** Build the full item list (caption + media). Empty caption skips the TEXT item. */
function captionedItems(text: string, media: MessageItem): MessageItem[] {
  const items: MessageItem[] = [];
  if (text) items.push({ type: MessageItemType.TEXT, text_item: { text } });
  items.push(media);
  return items;
}

export async function sendImageMessage(params: {
  to: string;
  text: string;
  uploaded: UploadedFileInfo;
  opts: WeixinApiOptions & { contextToken?: string };
  logger: Logger;
}): Promise<{ messageId: string }> {
  return sendItems({
    to: params.to,
    items: captionedItems(params.text, imageItem(params.uploaded)),
    opts: params.opts,
    label: "sendImageMessage",
    logger: params.logger,
  });
}

export async function sendVideoMessage(params: {
  to: string;
  text: string;
  uploaded: UploadedFileInfo;
  opts: WeixinApiOptions & { contextToken?: string };
  logger: Logger;
}): Promise<{ messageId: string }> {
  return sendItems({
    to: params.to,
    items: captionedItems(params.text, videoItem(params.uploaded)),
    opts: params.opts,
    label: "sendVideoMessage",
    logger: params.logger,
  });
}

export async function sendFileMessage(params: {
  to: string;
  text: string;
  fileName: string;
  uploaded: UploadedFileInfo;
  opts: WeixinApiOptions & { contextToken?: string };
  logger: Logger;
}): Promise<{ messageId: string }> {
  return sendItems({
    to: params.to,
    items: captionedItems(params.text, fileItem(params.uploaded, params.fileName)),
    opts: params.opts,
    label: "sendFileMessage",
    logger: params.logger,
  });
}
