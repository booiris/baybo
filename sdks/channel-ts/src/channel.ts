import type { Logger } from "./logger.js";
import type { ResourceAccess, WireAttachment } from "./wire.js";

export interface AgentMessage {
  sessionId: string;
  userId: string;
  content: string;
  /**
   * Non-text payloads the agent attached to this turn. Bytes are not
   * inline — each entry's `blob_id` references a blob the gateway has
   * already persisted. Sidecars that want to deliver these to the
   * platform should use `fetchBlob` to pull the bytes back over
   * `GET /v1/blobs/<id>`. Empty / undefined when the agent's reply is
   * text-only.
   */
  attachments?: WireAttachment[];
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

/**
 * Telemetry surfaced when the agent dispatches a tool call. Streaming
 * channels render an indicator in the in-flight card; everyone else
 * drops it. `paramsPreview` is a pre-truncated, vault-redacted JSON
 * snippet (≤256 chars) — sidecars never see raw tool arguments.
 * `description` is the tool's optional `call_label` hook (Bash's
 * `description`, etc.) when present.
 */
export interface AgentToolCallStarted {
  sessionId: string;
  userId: string;
  callId: string;
  tool: string;
  paramsPreview: string;
  description?: string;
}

/**
 * Pair to {@link AgentToolCallStarted}. `error` is set when the tool
 * errored or was denied; `resultPreview` is empty / a short fallback
 * in that case. Match against the `callId` you saw on `onToolCallStarted`
 * to pair the indicator with its terminal state.
 */
export interface AgentToolCallCompleted {
  sessionId: string;
  userId: string;
  callId: string;
  tool: string;
  resultPreview: string;
  error?: string;
  durationMs: number;
}

export type DiagnoseStatus = "ok" | "warn" | "error";

/**
 * One row in a diagnose report. `status` distinguishes a hard failure
 * an operator should act on (`error`) from a soft warning (`warn`)
 * from a healthy probe (`ok`). `detail` is free-form context.
 */
export interface DiagnoseCheck {
  name: string;
  status: DiagnoseStatus;
  detail: string;
}

/**
 * Inputs passed to {@link Channel.onDiagnoseRequested}. The sidecar
 * runs whatever self-tests fit its platform (Lark: bot identity
 * fetch, WSS connectivity, send-roundtrip) and returns a list of
 * checks. Whole-pipeline failures (the report couldn't even be
 * assembled) raise an exception from the hook; the SDK runner
 * translates that into `ok: false` on the wire.
 */
export interface DiagnoseRequest {
  botId: string;
}

export interface UserInbound {
  sessionId: string;
  userId: string;
  content: string;
  /**
   * Per-tenant credential that produced this inbound event, for
   * channels that multiplex many bots (Telegram, future Discord).
   * Omit / empty string for channels without a bot concept. Aura's
   * pairing gate uses `(channelType, botId, userId)` as the triple.
   */
  botId?: string;
  /**
   * Platform-native message id (Telegram `update_id`, Weixin `msg_id`,
   * …). When set, the gateway dedups inbound traffic per
   * `(channelType, botId, platformMsgId)` — protects against the
   * sidecar replaying its long-poll buffer after a restart, or any
   * other path that surfaces the same upstream event twice. Channels
   * without a stable platform id can omit it.
   */
  platformMsgId?: string;
  /**
   * Non-text payloads attached to the user's message. Each entry
   * references a blob the sidecar already uploaded via
   * `POST /v1/blobs`; the gateway's wire-side conversion turns them
   * into `ContentBlock::Image/Audio/File` for the agent.
   */
  attachments?: WireAttachment[];
}

export interface ApprovalRequest {
  callId: string;
  sessionId: string;
  /**
   * Aura user id (`<channelType>_<id>` — e.g. `telegram_<...>` — matching the inbound
   * `UserInbound.userId`). Sidecars that route approval prompts by
   * platform user consume this directly; empty string when the tool
   * call isn't user-scoped (e.g. cron-triggered).
   */
  userId: string;
  tool: string;
  /**
   * Resources the tool will touch on this call (filesystem paths,
   * hosts, shell commands). Sidecars should render these to the user
   * as the primary "what is being approved?" surface — they describe
   * the security-relevant action, not the JSON shape of the call.
   * Empty array when the tool declares no controlled resources
   * (auto-approve path, no prompt would have fired).
   */
  accesses: ResourceAccess[];
  paramsPreview: string;
  /**
   * Optional human-readable label produced by the tool's
   * `Tool::call_label` hook (e.g. Bash's `description` parameter).
   * Sidecars may show this alongside `accesses`. `undefined` when the
   * tool did not supply one.
   */
  description?: string;
}

export type ApprovalDecision = "approve" | "approve_always" | "deny";

export type NoticeLevel = "warn" | "error";

/**
 * Control-plane: aura is telling the sidecar to attach a new
 * per-tenant credential. For the Telegram channel `botId` is an
 * operator-chosen label and `token` is the @BotFather token; other
 * sidecars can repurpose the same shape for whatever per-tenant
 * identity they multiplex.
 *
 * `metadata` carries any auxiliary key/value configuration the
 * gateway has stored for this bot beyond the primary token (Lark's
 * `app_secret` / `encrypt_key` / `verification_token` / `base_url`,
 * Discord intents bitmask, etc.). The map is always present but is
 * empty for single-secret channels (Telegram, Weixin); the runner
 * supplies a frozen object so a sidecar can't accidentally mutate
 * the wire payload.
 */
export interface StartBotCommand {
  botId: string;
  token: string;
  metadata: Readonly<Record<string, string>>;
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
   * Optional tool-use telemetry. The gateway emits
   * `Frame::ToolCallStarted` immediately before invoking each tool
   * call on the agent's turn and `Frame::ToolCallCompleted` once the
   * tool returns (or errors / is denied). Streaming sidecars use
   * these to render mid-turn "🔧 running …" indicators in their
   * in-flight card; sidecars without a streaming surface omit the
   * methods and the SDK runner drops the frames silently.
   */
  onToolCallStarted?(ev: AgentToolCallStarted): Promise<void>;

  onToolCallCompleted?(ev: AgentToolCallCompleted): Promise<void>;

  /**
   * Self-test hook. Aura's admin endpoint
   * `GET /v1/admin/channels/<type>/diagnose?bot_id=<id>` round-trips
   * a request through the WS to this hook; the operator sees the
   * returned checks rendered in the dashboard. Implement when the
   * sidecar can run useful probes (auth status, transport
   * connectivity, send-roundtrip); omit otherwise. Sidecars that
   * implement the hook MUST advertise the `"diagnose"` capability so
   * the gateway only routes diagnose requests to peers that can
   * answer them.
   */
  onDiagnoseRequested?(req: DiagnoseRequest): Promise<DiagnoseCheck[]>;

  /**
   * MCP tunnel hook. Per Decision #9 of the Lark report, the
   * sidecar hosts an MCP server (today: `feishu_*` OAPI tools,
   * Phase 3.3 work) and JSON-RPC envelopes ride the existing channel
   * WS as opaque [`Frame::Mcp`] payloads. The runtime delivers each
   * inbound envelope here with the originating `tunnelId` (a UUID
   * per agent-side MCP session); the sidecar processes the envelope
   * and replies via [`McpReplyHandle.send`].
   *
   * Implementing this hook auto-claims the `"mcp_tunnel"` capability
   * on `Register`; the gateway only forwards `Frame::Mcp` to peers
   * that claimed it.
   */
  onMcpEnvelope?(
    tunnelId: string,
    payload: Uint8Array,
    reply: McpReplyHandle,
  ): Promise<void>;
}

/**
 * Lifeline an `onMcpEnvelope` handler uses to ship a JSON-RPC reply
 * (or notification) back through the gateway tunnel. Decoupled from
 * the request so a sidecar's MCP server can produce streamed
 * notifications independent of the request/reply pairing.
 */
export interface McpReplyHandle {
  send(payload: Uint8Array): Promise<void>;
}

// Re-extend the Channel interface — TS interface declarations merge,
// so the approval / slash / start-stop / inbound members below all
// land on the same `Channel` exported above. Keeping them separate
// from the MCP hook lets the doc strings stay near the interface
// they describe without nesting `McpReplyHandle` inside `Channel`.
export interface Channel {
  /**
   * Return value is encoded into a `ResolveApproval` frame by the runner.
   * The runner invokes this handler in a detached task so concurrent
   * approvals do not serialize behind a slow one.
   */
  onApprovalRequested?(req: ApprovalRequest): Promise<ApprovalDecision>;

  onApprovalResolved?(callId: string, decision: ApprovalDecision): Promise<void>;

  /**
   * Control-plane: gateway-authored slash-command manifest. Pushed
   * once after a successful Register handshake (and on every
   * reconnect) so each sidecar's native command surface — Telegram
   * `setMyCommands`, Discord application commands, … — stays in sync
   * with the gateway dispatcher without each sidecar maintaining its
   * own copy. Sidecars without a command UI may ignore this hook;
   * `BotChannel` honours it by re-publishing the new list to every
   * live bot.
   */
  onSlashManifest?(
    commands: ReadonlyArray<import("./generated/SlashCommandSpec.js").SlashCommandSpec>,
  ): Promise<void>;

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
   *
   * Single-consumer contract: the runner drives exactly one iterator
   * at a time per connection, and a channel instance is long-lived
   * across reconnects (each new connection calls `inbound(newSignal)`
   * once). Implementations may share queue state across iterators to
   * buffer events across a reconnect window, but must not be driven
   * by two concurrent consumers — behavior is undefined.
   */
  inbound(signal: AbortSignal): AsyncIterable<UserInbound>;

  onStop?(): Promise<void>;
}

export interface RunOptions {
  /**
   * Explicit WebSocket URL (e.g. `ws://127.0.0.1:42111/v1/channel-ws`
   * for the default loopback deployment, or `wss://` for any custom
   * remote gateway). Default: read from the `AURA_CHANNEL_URL` env
   * var — the gateway's sidecar supervisor sets it when it spawns
   * the child process.
   */
  wsUrl?: string;

  /**
   * Capability token presented on the `Register` frame. Default:
   * read from the `AURA_CHANNEL_TOKEN` env var (same launcher
   * contract as `AURA_CHANNEL_URL`).
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

  /**
   * Optional capability strings advertised on the `Register` frame so
   * the gateway only sends frames the sidecar speaks. Available
   * strings: `"secrets"` (gates {@link secrets} client). Forward-
   * compatible: a sidecar that doesn't claim a capability simply
   * doesn't get the matching frame.
   */
  capabilities?: ReadonlyArray<string>;
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
