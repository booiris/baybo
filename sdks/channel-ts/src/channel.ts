import type { Logger } from "./logger.js";

export interface AgentMessage {
  sessionId: string;
  userId: string;
  content: string;
}

/**
 * One incremental chunk of an in-flight assistant response. Emitted
 * by `onDelta`; sidecars that don't render partial output (Telegram
 * default) ignore it. `userId` mirrors the frame the gateway puts on
 * the wire so sidecars can scope deltas to the right platform user.
 */
export interface AgentDelta {
  sessionId: string;
  userId: string;
  text: string;
}

/**
 * Out-of-band notice emitted by aura (skill warnings, degraded-mode
 * banners). `userId` identifies the platform user the notice is
 * addressed to — sidecars that fan notices to a single chat per user
 * consume it the same way `onMessage` consumes its `userId`.
 */
export interface AgentNotice {
  sessionId: string;
  userId: string;
  level: NoticeLevel;
  text: string;
}

export interface UserInbound {
  sessionId: string;
  userId: string;
  content: string;
}

export interface ApprovalRequest {
  callId: string;
  sessionId: string;
  /**
   * Aura user id (`tg_<id>` for Telegram, matching the inbound
   * `UserInbound.userId`). Sidecars that route approval prompts by
   * platform user consume this directly; empty string when the tool
   * call isn't user-scoped (e.g. cron-triggered).
   */
  userId: string;
  tool: string;
  paramsPreview: string;
}

export type ApprovalDecision = "approve" | "approve_always" | "deny";

export type NoticeLevel = "warn" | "error";

/**
 * Control-plane: aura is telling the sidecar to attach a new
 * per-tenant credential. For the Telegram channel `botId` is an
 * operator-chosen label and `token` is the @BotFather token; other
 * sidecars can repurpose the same shape for whatever per-tenant
 * identity they multiplex.
 */
export interface StartBotCommand {
  botId: string;
  token: string;
}

/** Control-plane: aura is telling the sidecar to detach a tenant. */
export interface StopBotCommand {
  botId: string;
}

/**
 * Ack for a `Start`/`StopBot` command. `ok: false + message` surfaces
 * startup failures (invalid token, rate-limited, etc.) to aura so the
 * admin dashboard can render the reason instead of a silent failure.
 */
export interface BotStatusReport {
  botId: string;
  ok: boolean;
  message?: string;
}

/**
 * Sidecar contract. Implement this object and pass it to {@link runChannel}.
 *
 * The only required members are `channelType`, `onMessage`, and `inbound`;
 * every other hook is optional and its frame is dropped on the floor if
 * unimplemented.
 */
export interface Channel {
  readonly channelType: string;

  onMessage(msg: AgentMessage): Promise<void>;

  onDelta?(delta: AgentDelta): Promise<void>;

  onNotice?(notice: AgentNotice): Promise<void>;

  /**
   * Return value is encoded into a `ResolveApproval` frame by the runner.
   * The runner invokes this handler in a detached task so concurrent
   * approvals do not serialize behind a slow one.
   */
  onApprovalRequested?(req: ApprovalRequest): Promise<ApprovalDecision>;

  onApprovalResolved?(callId: string, decision: ApprovalDecision): Promise<void>;

  /**
   * Control-plane: aura is attaching a new per-tenant credential (bot
   * token for Telegram / Discord / Slack, API key for an HTTP
   * channel, …). Return `ok: true` once the credential is live, or
   * `ok: false + message` on failure so aura can surface the reason.
   * A sidecar that doesn't implement this hook can't host runtime-
   * provisioned tenants; aura logs `unsupported` and moves on.
   */
  onStartBot?(cmd: StartBotCommand): Promise<BotStatusReport>;

  /**
   * Control-plane: aura is detaching a previously-attached tenant.
   * Implementations should tear down polling / connections for
   * `cmd.botId` and flush any in-flight state. `ok: false + message`
   * for operator visibility; aura treats the bot as removed either way.
   */
  onStopBot?(cmd: StopBotCommand): Promise<BotStatusReport>;

  /**
   * Pull-based producer of user-authored messages flowing into the agent.
   * Yield one `UserInbound` per native platform event. Terminate the
   * iterable when `signal` aborts — the runner uses the same signal to
   * tear down the transport.
   */
  inbound(signal: AbortSignal): AsyncIterable<UserInbound>;

  onStop?(): Promise<void>;
}

export interface RunOptions {
  /**
   * Explicit WebSocket URL. Accepts standard `ws://` / `wss://` for TCP
   * deployments and `ws+unix://<socket-path>:/v1/channel-ws` for UDS.
   * Default: derived from the `AURA_CHANNEL_SOCKET` env var — the
   * convention a future sidecar supervisor will set; until that lands,
   * the sidecar's launcher must export it (or pass `wsUrl` directly).
   */
  wsUrl?: string;

  /**
   * Capability token presented on the `Register` frame. Default: read
   * from the `AURA_CHANNEL_TOKEN` env var (same launcher contract as
   * `AURA_CHANNEL_SOCKET`).
   */
  token?: string;

  /**
   * Abort signal to tear the runner down. Aborting closes the transport
   * and cancels the `inbound()` generator.
   */
  abortSignal?: AbortSignal;

  logger?: Logger;

  /**
   * Auto-reconnect with exponential backoff + jitter when the transport
   * drops. Default: `true` (enabled with sensible defaults). Pass
   * `false` to disable entirely — `runChannel` then returns after the
   * first disconnect. Pass an object to tune the policy.
   *
   * Reconnect fires on transient failures (`connect`, `peer_closed`,
   * `transport`). Fatal failures (`config`, `register_rejected`,
   * `protocol_violation`, `decode`) always propagate.
   */
  reconnect?: boolean | ReconnectPolicy;
}

export interface ReconnectPolicy {
  /** Initial backoff delay in ms. Default: 500. */
  initialDelayMs?: number;
  /** Maximum backoff delay in ms. Default: 30_000. */
  maxDelayMs?: number;
  /** Backoff multiplier between successive failed attempts. Default: 2. */
  multiplier?: number;
  /** Jitter factor (0..1). Applied symmetrically: `delay * (1 ± jitter)`. Default: 0.25. */
  jitter?: number;
  /** Give up after this many consecutive failed attempts. Default: unlimited. */
  maxAttempts?: number;
}

export class RunnerError extends Error {
  constructor(
    message: string,
    public readonly kind:
      | "connect"
      | "register_rejected"
      | "protocol_violation"
      | "peer_closed"
      | "decode"
      | "transport"
      | "config",
  ) {
    super(message);
    this.name = "RunnerError";
  }
}
