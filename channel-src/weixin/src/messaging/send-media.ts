/**
 * MIME-routed dispatch from `BotPlatform.sendMedia` to the right
 * iLink upload + send pair. The agent's `WireAttachment` carries a
 * `mime_type`; we use it to pick:
 *   - `video/*` → `uploadVideoToWeixin` + `sendVideoMessage`
 *   - `image/*` → `uploadImageToWeixin` + `sendImageMessage`
 *   - else     → `uploadFileAttachmentToWeixin` + `sendFileMessage`
 */
import type { Logger } from "@aura/channel-sdk";

import type { WeixinApiOptions } from "../api/endpoints.js";
import {
  uploadFileAttachmentToWeixin,
  uploadImageToWeixin,
  uploadVideoToWeixin,
} from "../cdn/upload.js";

import { sendFileMessage, sendImageMessage, sendVideoMessage } from "./send.js";

export async function sendWeixinMediaBytes(params: {
  bytes: Buffer;
  mimeType: string;
  filename?: string;
  to: string;
  text: string;
  opts: WeixinApiOptions & { contextToken?: string };
  cdnBaseUrl: string;
  logger: Logger;
}): Promise<{ messageId: string }> {
  const { bytes, mimeType, filename, to, text, opts, cdnBaseUrl, logger } = params;
  const uploadOpts: WeixinApiOptions = {
    baseUrl: opts.baseUrl,
    ...(opts.token !== undefined ? { token: opts.token } : {}),
    ...(opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : {}),
  };

  if (mimeType.startsWith("video/")) {
    logger.info(`sendWeixinMediaBytes: uploading video size=${bytes.length} to=${to}`);
    const uploaded = await uploadVideoToWeixin({
      bytes,
      toUserId: to,
      opts: uploadOpts,
      cdnBaseUrl,
      logger,
    });
    return sendVideoMessage({ to, text, uploaded, opts, logger });
  }

  if (mimeType.startsWith("image/")) {
    logger.info(`sendWeixinMediaBytes: uploading image size=${bytes.length} to=${to}`);
    const uploaded = await uploadImageToWeixin({
      bytes,
      toUserId: to,
      opts: uploadOpts,
      cdnBaseUrl,
      logger,
    });
    return sendImageMessage({ to, text, uploaded, opts, logger });
  }

  // File attachment: PDF, doc, zip, etc. Use the supplied filename or
  // fall back to a generic one — iLink stores `file_name` purely for
  // display so a sensible fallback keeps the receiver's UX intact.
  const fileName = filename ?? "attachment.bin";
  logger.info(
    `sendWeixinMediaBytes: uploading file size=${bytes.length} name=${fileName} to=${to}`,
  );
  const uploaded = await uploadFileAttachmentToWeixin({
    bytes,
    toUserId: to,
    opts: uploadOpts,
    cdnBaseUrl,
    logger,
  });
  return sendFileMessage({ to, text, fileName, uploaded, opts, logger });
}
