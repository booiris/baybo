export { runChannel } from "./runner.js";
export { runSidecar } from "./sidecar.js";
export type { SidecarOptions } from "./sidecar.js";
export { runRegistration } from "./register.js";
export type { RegistrationContext, RegistrationResult } from "./register.js";
export { RunnerError } from "./channel.js";
export type {
  AgentDelta,
  AgentMessage,
  AgentNotice,
  AgentToolCallCompleted,
  AgentToolCallStarted,
  ApprovalDecision,
  ApprovalRequest,
  BotStatusReport,
  Channel,
  NoticeLevel,
  ReconnectPolicy,
  RunOptions,
  StartBotCommand,
  StopBotCommand,
  UserInbound,
} from "./channel.js";
export { defaultLogger, hasWireSink } from "./logger.js";
export type {
  Logger,
  WireCapableLogger,
  WireLogLevel,
  WireLogSink,
} from "./logger.js";
export type { AttachmentKind, WireAttachment } from "./wire.js";
export {
  BlobPairingRequiredError,
  fetchBlob,
  fetchBlobStream,
  uploadBlob,
} from "./blobs.js";
export type {
  BlobClientOptions,
  FetchBlobResult,
  FetchBlobStreamResult,
  UploadBlobResult,
} from "./blobs.js";
export {
  CapabilityMissingError,
  SecretBotUnknownError,
  SecretQuotaExceededError,
  secrets,
} from "./secrets.js";
export type { Secrets, SecretsScope } from "./secrets.js";
