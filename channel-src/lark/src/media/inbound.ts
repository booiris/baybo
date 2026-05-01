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
  const fetchType = resourceFetchType(resource.type);
  let bytes: Buffer;
  try {
    bytes = await channel.downloadResource(resource.fileKey, fetchType);
  } catch (err) {
    logger.error(
      `lark inbound media download failed kind=${resource.type} key=${resource.fileKey}: ${String(err)}`,
    );
    return null;
  }
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
