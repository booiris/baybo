import { encode as msgpackEncode, decode as msgpackDecode } from "@msgpack/msgpack";

import type { Frame } from "./generated/Frame.js";

export type { Frame } from "./generated/Frame.js";
export type { Message } from "./generated/Message.js";
export type { AttachmentKind } from "./generated/AttachmentKind.js";
export type { WireAttachment } from "./generated/WireAttachment.js";
export type { SecretOp } from "./generated/SecretOp.js";
export type { ResourceAccess } from "./generated/ResourceAccess.js";

export function encodeFrame(frame: Frame): Uint8Array {
  return msgpackEncode(frame);
}

export function decodeFrame(bytes: Uint8Array): Frame {
  const decoded = msgpackDecode(bytes);
  if (decoded === null || typeof decoded !== "object" || !("kind" in decoded)) {
    throw new Error("decoded payload is not a Frame (missing `kind`)");
  }
  return decoded as Frame;
}
