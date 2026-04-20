export { Client, SdkError } from "./client.js";
export type { ConnectOptions } from "./client.js";
export {
  PROTOCOL_VERSION,
  encodeFrame,
  decodeFrame,
} from "./wire.js";
export type { ChannelTypeV2, Frame, Message } from "./wire.js";
