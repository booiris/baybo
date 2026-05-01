import type { Logger, WireAttachment } from "@aura/channel-sdk";
import { fetchBlob } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

import type { LarkChat } from "../platform.js";

// Lark's `LarkChannel.send` takes one `SendInput` per call, so a
// multi-attachment agent reply turns into N sequential sends followed
// by an optional caption. We send the caption last so a Lark client
// that scrolls to the newest message lands on the agent's text rather
// than the trailing attachment.
export async function sendLarkAttachments(args: {
  channel: lark.LarkChannel;
  chat: LarkChat;
  payload: { text: string; attachments: WireAttachment[] };
  logger: Logger;
}): Promise<void> {
  const { channel, chat, payload, logger } = args;
  for (const att of payload.attachments) {
    const blob = await loadBlob(att, logger);
    if (!blob) continue;
    const input = buildSendInput(att, blob.bytes);
    if (!input) {
      logger.warn(
        `lark sendMedia: unsupported attachment kind=${att.kind} blob_id=${att.blob_id}`,
      );
      continue;
    }
    try {
      await channel.send(chat.chatId, input);
    } catch (err) {
      logger.error(
        `lark send attachment failed kind=${att.kind} blob_id=${att.blob_id}: ${String(err)}`,
      );
    }
  }
  if (payload.text) {
    try {
      await channel.send(chat.chatId, { text: payload.text });
    } catch (err) {
      logger.error(`lark send caption failed: ${String(err)}`);
    }
  }
}

async function loadBlob(
  att: WireAttachment,
  logger: Logger,
): Promise<{ bytes: Buffer; mimeType: string } | null> {
  try {
    const { bytes, mimeType } = await fetchBlob(att.blob_id);
    // Lark SDK's `image`/`file`/`audio` sources accept Buffer; convert
    // from the SDK's Uint8Array view without copying.
    return { bytes: Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength), mimeType };
  } catch (err) {
    logger.error(
      `lark sendMedia: fetchBlob failed blob_id=${att.blob_id} err=${String(err)}`,
    );
    return null;
  }
}

function buildSendInput(
  att: WireAttachment,
  bytes: Buffer,
): lark.SendInput | null {
  switch (att.kind) {
    case "image":
      return { image: { source: bytes } };
    case "audio":
      return { audio: { source: bytes } };
    case "file":
      // Lark requires a fileName on file uploads. Default to a
      // synthetic name when the agent didn't carry one.
      return {
        file: {
          source: bytes,
          fileName: att.filename ?? `attachment-${att.blob_id.slice(0, 8)}`,
        },
      };
  }
}
