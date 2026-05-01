import type { Readable } from "node:stream";

import type { Logger, WireAttachment } from "@aura/channel-sdk";
import { BlobPairingRequiredError, uploadBlob } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

const MIME_BY_KIND: Record<lark.ResourceDescriptor["type"], string> = {
  image: "image/jpeg",
  audio: "audio/ogg",
  video: "video/mp4",
  file: "application/octet-stream",
  // Lark stickers are GIF-style images; treat as image so the agent
  // sees them as an image attachment rather than a generic blob.
  sticker: "image/gif",
};

// Cap each inbound resource at 50 MiB. Lark's tenant-side limits are
// roughly 10M image / 30M file / 30M audio / 50M video on the cloud
// product, and the gateway's blob ingest caps at 100 MiB; 50 MiB
// stays under both while still passing typical media. A private-cloud
// tenant with raised limits would otherwise let a malicious user
// force the sidecar to allocate the full payload before the gateway
// could reject it.
const MAX_RESOURCE_BYTES = 50 * 1024 * 1024;

// Stickers ride the agent's `image` content block; videos fall back to
// `file` because Aura's wire only exposes image / audio / file. See
// `sdks/channel-ts/src/generated/AttachmentKind.ts`.
function mapKind(
  type: lark.ResourceDescriptor["type"],
): WireAttachment["kind"] {
  switch (type) {
    case "image":
    case "sticker":
      return "image";
    case "audio":
      return "audio";
    case "file":
    case "video":
      return "file";
  }
}

function resourceFetchType(
  type: lark.ResourceDescriptor["type"],
): lark.ResourceType {
  return type === "image" || type === "sticker" ? "image" : "file";
}

export async function downloadResourceAsAttachment(args: {
  channel: lark.LarkChannel;
  resource: lark.ResourceDescriptor;
  botId: string;
  userId: string;
  logger: Logger;
}): Promise<WireAttachment | null> {
  const { channel, resource, botId, userId, logger } = args;
  let bytes: Buffer;
  try {
    bytes = await downloadBounded(channel, resource, logger);
  } catch (err) {
    logger.error(
      `lark inbound media download failed kind=${resource.type} key=${resource.fileKey}: ${String(err)}`,
    );
    return null;
  }
  if (bytes.length === 0) return null;
  const mimeType = MIME_BY_KIND[resource.type];
  try {
    const { blobId } = await uploadBlob(bytes, mimeType, { botId, userId });
    return {
      kind: mapKind(resource.type),
      blob_id: blobId,
      mime_type: mimeType,
      size: bytes.length,
      ...(resource.fileName ? { filename: resource.fileName } : {}),
    };
  } catch (err) {
    // Pairing prompts must surface to the user; re-throw so the
    // dispatcher can still emit a Message frame for the gateway's
    // pairing flow to run.
    if (err instanceof BlobPairingRequiredError) throw err;
    logger.error(
      `lark inbound media upload failed kind=${resource.type} key=${resource.fileKey}: ${String(err)}`,
    );
    return null;
  }
}

/**
 * Stream the resource through `rawClient.im.v1.{image,file}.get` with
 * a hard byte cap. `LarkChannel.downloadResource` calls these same
 * endpoints but `bufferFromStream`s the whole body without bound — a
 * malicious user can exploit that to force the shared sidecar to
 * allocate hundreds of MiB before any check runs.
 *
 * Two checks land before the upload path:
 *  1. Pre-flight `content-length` (when present): reject upfront so
 *     no bytes are read.
 *  2. Running counter while reading the stream: destroy the stream
 *     and reject as soon as the cap is exceeded.
 *
 * Returns an empty Buffer on cap exceeded so the caller can drop the
 * resource silently — the user keeps their other (in-bounds)
 * attachments and we don't poison the inbound with an attachment we
 * never persisted. A warning log surfaces the drop.
 */
async function downloadBounded(
  channel: lark.LarkChannel,
  resource: lark.ResourceDescriptor,
  logger: Logger,
): Promise<Buffer> {
  const fetchType = resourceFetchType(resource.type);
  const r =
    fetchType === "image"
      ? await channel.rawClient.im.v1.image.get({
          path: { image_key: resource.fileKey },
        })
      : await channel.rawClient.im.v1.file.get({
          path: { file_key: resource.fileKey },
        });

  const advertisedSize = parseContentLength(r.headers);
  if (advertisedSize !== null && advertisedSize > MAX_RESOURCE_BYTES) {
    logger.warn(
      `lark inbound resource exceeds ${MAX_RESOURCE_BYTES}-byte cap (advertised=${advertisedSize}) kind=${resource.type} key=${resource.fileKey}; dropping without download`,
    );
    return Buffer.alloc(0);
  }

  return readStreamCapped(r.getReadableStream(), resource, logger);
}

function parseContentLength(headers: unknown): number | null {
  if (typeof headers !== "object" || headers === null) return null;
  const raw = (headers as Record<string, unknown>)["content-length"];
  const str = Array.isArray(raw) ? raw[0] : raw;
  if (typeof str !== "string" && typeof str !== "number") return null;
  const n = typeof str === "number" ? str : Number.parseInt(str, 10);
  return Number.isFinite(n) && n >= 0 ? n : null;
}

function readStreamCapped(
  stream: Readable,
  resource: lark.ResourceDescriptor,
  logger: Logger,
): Promise<Buffer> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let total = 0;
    let aborted = false;

    const fail = (err: Error): void => {
      if (aborted) return;
      aborted = true;
      stream.destroy();
      reject(err);
    };

    stream.on("data", (chunk: Buffer | string) => {
      if (aborted) return;
      const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
      total += buf.length;
      if (total > MAX_RESOURCE_BYTES) {
        logger.warn(
          `lark inbound resource exceeded ${MAX_RESOURCE_BYTES}-byte cap mid-stream kind=${resource.type} key=${resource.fileKey}; dropping`,
        );
        aborted = true;
        stream.destroy();
        // Empty buffer signals "drop this attachment"; the caller
        // bails on `.length === 0` rather than uploading 0 bytes.
        resolve(Buffer.alloc(0));
        return;
      }
      chunks.push(buf);
    });
    stream.on("end", () => {
      if (aborted) return;
      resolve(Buffer.concat(chunks));
    });
    stream.on("error", (err) =>
      fail(err instanceof Error ? err : new Error(String(err))),
    );
  });
}
