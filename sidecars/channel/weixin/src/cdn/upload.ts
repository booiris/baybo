/**
 * Outbound media → iLink CDN pipeline. Takes a plaintext buffer in
 * memory (the gateway-side `fetchBlob` already pulled the bytes), runs
 * the iLink upload preflight, then AES-128-ECB encrypts and POSTs to
 * the returned CDN URL.
 *
 * Returns the `UploadedFileInfo` shape that the message-build helpers
 * in `messaging/send.ts` expect — `encrypt_query_param`, `aeskey`,
 * `fileSize`, `fileSizeCiphertext`. The agent-side `BlobRef` is no
 * longer involved at this layer; everything flows through Buffer.
 */
import crypto from "node:crypto";

import type { Logger } from "@baybo/channel-sdk";

import { aesEcbPaddedSize } from "./aes-ecb.js";
import { uploadBufferToCdn } from "./cdn-upload.js";
import { getUploadUrl } from "../api/endpoints.js";
import type { WeixinApiOptions } from "../api/endpoints.js";
import { UploadMediaType } from "../api/types.js";

export interface UploadedFileInfo {
  filekey: string;
  /** Goes into `CDNMedia.encrypt_query_param` on the outbound MessageItem. */
  downloadEncryptedQueryParam: string;
  /** Hex-encoded AES-128-ECB key. Convert to base64 for `CDNMedia.aes_key`. */
  aeskey: string;
  /** Plaintext size in bytes. */
  fileSize: number;
  /** Ciphertext size after PKCS7 padding. Used as `mid_size` / `video_size`. */
  fileSizeCiphertext: number;
}

/** Shared upload pipeline shared by image / video / file flavours. */
async function uploadMediaToCdn(params: {
  bytes: Buffer;
  toUserId: string;
  opts: WeixinApiOptions;
  cdnBaseUrl: string;
  mediaType: (typeof UploadMediaType)[keyof typeof UploadMediaType];
  label: string;
  logger: Logger;
}): Promise<UploadedFileInfo> {
  const { bytes, toUserId, opts, cdnBaseUrl, mediaType, label, logger } = params;

  const rawsize = bytes.length;
  const rawfilemd5 = crypto.createHash("md5").update(bytes).digest("hex");
  const filesize = aesEcbPaddedSize(rawsize);
  const filekey = crypto.randomBytes(16).toString("hex");
  const aeskey = crypto.randomBytes(16);

  logger.debug(
    `${label}: rawsize=${rawsize} filesize=${filesize} md5=${rawfilemd5} filekey=${filekey}`,
  );

  const uploadUrlResp = await getUploadUrl({
    ...opts,
    filekey,
    media_type: mediaType,
    to_user_id: toUserId,
    rawsize,
    rawfilemd5,
    filesize,
    no_need_thumb: true,
    aeskey: aeskey.toString("hex"),
  });

  const uploadFullUrl = uploadUrlResp.upload_full_url?.trim();
  const uploadParam = uploadUrlResp.upload_param;
  if (!uploadFullUrl && !uploadParam) {
    logger.error(
      `${label}: getUploadUrl returned no upload URL, resp=${JSON.stringify(uploadUrlResp)}`,
    );
    throw new Error(`${label}: getUploadUrl returned no upload URL`);
  }

  const { downloadParam: downloadEncryptedQueryParam } = await uploadBufferToCdn({
    buf: bytes,
    ...(uploadFullUrl ? { uploadFullUrl } : {}),
    ...(uploadParam !== undefined ? { uploadParam } : {}),
    filekey,
    cdnBaseUrl,
    aeskey,
    label: `${label}[orig filekey=${filekey}]`,
    logger,
  });

  return {
    filekey,
    downloadEncryptedQueryParam,
    aeskey: aeskey.toString("hex"),
    fileSize: rawsize,
    fileSizeCiphertext: filesize,
  };
}

/** Upload an image buffer (JPEG/PNG/...) for a single iLink recipient. */
export async function uploadImageToWeixin(params: {
  bytes: Buffer;
  toUserId: string;
  opts: WeixinApiOptions;
  cdnBaseUrl: string;
  logger: Logger;
}): Promise<UploadedFileInfo> {
  return uploadMediaToCdn({
    ...params,
    mediaType: UploadMediaType.IMAGE,
    label: "uploadImageToWeixin",
  });
}

/** Upload a video buffer (MP4/...) for a single iLink recipient. */
export async function uploadVideoToWeixin(params: {
  bytes: Buffer;
  toUserId: string;
  opts: WeixinApiOptions;
  cdnBaseUrl: string;
  logger: Logger;
}): Promise<UploadedFileInfo> {
  return uploadMediaToCdn({
    ...params,
    mediaType: UploadMediaType.VIDEO,
    label: "uploadVideoToWeixin",
  });
}

/** Upload a generic file attachment (PDF/zip/...) for a single iLink recipient. */
export async function uploadFileAttachmentToWeixin(params: {
  bytes: Buffer;
  toUserId: string;
  opts: WeixinApiOptions;
  cdnBaseUrl: string;
  logger: Logger;
}): Promise<UploadedFileInfo> {
  return uploadMediaToCdn({
    ...params,
    mediaType: UploadMediaType.FILE,
    label: "uploadFileAttachmentToWeixin",
  });
}
