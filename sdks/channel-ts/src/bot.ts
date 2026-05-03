import type {
  AgentDelta,
  AgentMessage,
  AgentNotice,
  ApprovalDecision,
  ApprovalRequest,
  BotStatusReport,
  Channel,
  StartBotCommand,
  StopBotCommand,
  UserInbound,
} from "./channel.js";
import type { SlashCommandSpec } from "./generated/SlashCommandSpec.js";
import type { Logger } from "./logger.js";
import type { WireAttachment } from "./wire.js";

export type { SlashCommandSpec };

/**
 * One native-platform inbound event flowing from the transport into
 * the channel. `chat` and `platformUserId` are the platform's own
 * identifiers (Telegram chat id, Discord channel id, …) kept opaque
 * so the channel doesn't have to know each platform's shape.
 *
 * `attachments` is populated by sidecars whose platform delivers
 * non-text media (images, voice, files). Each entry must reference a
 * blob the sidecar has already uploaded via `POST /v1/blobs` (use the
 * `uploadBlob` helper). Sidecars that don't speak media omit the field.
 */
export interface BotInboundEvent<ChatId> {
  chat: ChatId;
  platformUserId: string;
  content: string;
  /**
   * Platform-native message id (Telegram `update_id`, Weixin `msg_id`,
   * …). Forwarded to the gateway as `Message.platform_msg_id` so the
   * gateway can dedup if the sidecar replays the same upstream event
   * (long-poll buffer replay after restart, transient retries).
   * Sidecars without a stable id can omit it; dedup is then disabled
   * for that frame.
   */
  platformMsgId?: string;
  attachments?: WireAttachment[];
}

/**
 * Outbound media payload handed to {@link BotPlatform.sendMedia}. The
 * sidecar receives an already-resolved list of `WireAttachment`s
 * referencing blobs the gateway holds; use `fetchBlob` to pull the
 * actual bytes when the platform's send API needs them inline.
 */
export interface BotMediaPayload {
  text: string;
  attachments: WireAttachment[];
}

/**
 * Routing tuple reconstructed from an aura user id. The SDK hands
 * these to the approvals hook so approval UI code never has to touch
 * the routing tables itself.
 */
export interface BotRoute<BotHandle, ChatId> {
  botId: string;
  handle: BotHandle;
  chat: ChatId;
}

/**
 * Callbacks the SDK passes into {@link BotPlatform.startBot}. The
 * transport invokes each at the right point in the bot's lifecycle.
 *
 * Exit signalling is NOT a hook — the transport returns a
 * `waitForExit` promise from `startBot` instead, so the SDK can bind
 * lifetime to the exact generation it just spawned without the
 * transport having to pass identity through a callback.
 */
export interface BotStartHooks<BotHandle, ChatId> {
  /** Push one inbound user message into the channel. */
  emit(ev: BotInboundEvent<ChatId>): void;

  /**
   * Pre-start hook: give any configured approvals implementation a
   * chance to wire bot-specific handlers (e.g. grammy callbackQuery)
   * while the middleware tree is still mutable. Transports must call
   * this once after constructing `handle` and before starting polling,
   * even when no approvals are configured (the SDK passes a no-op in
   * that case).
   */
  attach(handle: BotHandle): void | Promise<void>;
}

/**
 * Platform adapter a sidecar hands to {@link BotChannel}. Every
 * method here is platform-specific — how to spin up a bot, how to
 * send a message, how to compose the composite user id. Everything
 * generic (routing, lifecycle, queue, StopBot purge) lives in the
 * channel.
 *
 * `ChatId` is the full conversation address the channel caches in
 * its routing table. Contract (not expressible in the type system):
 * it must be a stable **plain data value** — primitives (string,
 * number) or plain objects with primitive fields
 * (e.g. `{ chatId, threadId? }` for Telegram supergroup topics). It
 * must NOT be a live platform handle (e.g. a `discord.js`
 * `TextChannel` object) — routing entries outlive any one bot's
 * polling session, and caching a live handle leaks resources and
 * surfaces dead references after a restart. If the conversation
 * address is composite (Telegram `chat_id + message_thread_id`,
 * Slack `channel + thread_ts`), use an object and document its
 * shape in your platform type.
 */
export interface BotPlatform<BotHandle, ChatId> {
  /**
   * Optional override for how the channel stringifies a chat address
   * into the composite aura user id. The SDK composes the final
   * `userId` as `<channelType>_<botId>_<chatKey>_<platformUserId>`
   * (surfaced as `UserInbound.userId` / `ApprovalRequest.userId`).
   *
   * Default (when omitted): primitives pass through `String(chat)`;
   * plain objects serialize to `key=value&…` with keys sorted
   * alphabetically for stability. Override only when you want a
   * prettier / shorter form — correctness-wise the default handles
   * any `ChatId` shaped as primitives-or-plain-object-of-primitives.
   */
  chatKey?(chat: ChatId): string;

  /**
   * Construct, initialize, and start a bot. Register the platform's
   * native inbound handler and call `hooks.emit` from it. Call
   * `hooks.attach(handle)` once after the handle exists and before
   * starting polling so any approvals hook can wire its own handlers.
   *
   * If polling is asynchronous and can terminate (grammy-style
   * `bot.start()` returns exactly such a promise), return it as
   * `waitForExit`. The SDK binds that promise to the handle it just
   * received, so a late exit from a prior generation can't purge a
   * freshly-spawned replacement for the same `botId`.
   *
   * Throw to surface `ok: false + message` on aura's admin dashboard.
   */
  startBot(
    cmd: StartBotCommand,
    hooks: BotStartHooks<BotHandle, ChatId>,
  ): Promise<{
    handle: BotHandle;
    username?: string;
    waitForExit?: Promise<void>;
  }>;

  /**
   * Tear down the bot. Called from `onStopBot` and `onStop`. Must be
   * idempotent — a transport that races its own exit path with an
   * external `bot.stop()` should swallow the second invocation
   * cleanly.
   */
  stopBot(handle: BotHandle): Promise<void>;

  /**
   * Deliver an outbound text message on a specific bot+chat pair.
   * `userId` is the composite aura user id the channel routed by, so a
   * streaming-aware platform can finalise an in-flight stream for that
   * user instead of posting a fresh message. Implementations that
   * don't stream can ignore the parameter.
   */
  sendText(
    handle: BotHandle,
    chat: ChatId,
    text: string,
    userId: string,
  ): Promise<void>;

  /**
   * Deliver an outbound message that carries non-text media. Called by
   * {@link BotChannel.onMessage} only when the agent attached
   * `WireAttachment`s. Sidecars that implement this should fetch the
   * referenced blobs via `fetchBlob`, upload them to the platform, and
   * post the combined message. Omit the method to fall through to
   * `sendText` with the text body only — attachments will be dropped
   * with a warning, so platforms that need media support **must**
   * implement this. `userId` mirrors the `sendText` arg so streaming
   * platforms can close a live card before sending media.
   */
  sendMedia?(
    handle: BotHandle,
    chat: ChatId,
    payload: BotMediaPayload,
    userId: string,
  ): Promise<void>;

  /**
   * Optional platform-specific renderer for out-of-band notices.
   * Defaults to `sendText` with a level-keyed emoji prefix (`⚠️` warn,
   * `❌` error). Override when your platform supports richer
   * formatting (embeds, block-kit attachments).
   */
  sendNotice?(
    handle: BotHandle,
    chat: ChatId,
    notice: AgentNotice,
  ): Promise<void>;

  /**
   * Optional "working on it" ping fired by the channel once per
   * accepted inbound, before the event is queued for aura. Best-effort:
   * the channel fire-and-forgets the returned promise, so a rate-limit
   * or network blip is a debug log, not a pumped error. Implement when
   * your platform has a typing / presence primitive (Telegram
   * `sendChatAction`, Slack typing indicator, Discord `triggerTyping`,
   * …); omit otherwise.
   */
  notifyTyping?(handle: BotHandle, chat: ChatId): Promise<void>;

  /**
   * Optional streaming sink for in-flight agent deltas. When
   * implemented, the channel routes `Frame::Delta` here after
   * resolving the route; the platform decides how to throttle and
   * render (Lark CardKit `cards.stream`, etc.). The channel cancels
   * any active typing session for the user before invoking, since the
   * streaming UI replaces the typing indicator. Sidecars without a
   * streaming primitive omit this method and deltas are dropped on
   * the floor (today's behaviour).
   *
   * `userId` is the composite aura user id; the platform's
   * `sendText` / `sendMedia` is still called for the terminal frame,
   * and platforms that opened a stream are responsible for finalising
   * it there (e.g. by inspecting the user's session state in
   * `sendText` and closing the stream instead of posting a fresh
   * message).
   */
  onAgentDelta?(
    handle: BotHandle,
    chat: ChatId,
    userId: string,
    text: string,
  ): Promise<void>;

  /**
   * Optional tool-use telemetry sink. The channel routes
   * `Frame::ToolCallStarted` / `Frame::ToolCallCompleted` here after
   * resolving the route, so platforms can render a mid-turn
   * indicator (Lark CardKit footer line, Slack thread block, …).
   * Omit if the platform has no streaming surface — frames drop
   * silently.
   */
  onAgentToolCallStarted?(
    handle: BotHandle,
    chat: ChatId,
    userId: string,
    ev: import("./channel.js").AgentToolCallStarted,
  ): Promise<void>;

  onAgentToolCallCompleted?(
    handle: BotHandle,
    chat: ChatId,
    userId: string,
    ev: import("./channel.js").AgentToolCallCompleted,
  ): Promise<void>;

  /**
   * Optional sidecar-hosted MCP server. The channel forwards each
   * inbound `Frame::Mcp` envelope here unchanged; the platform owns
   * the per-tunnel session lifecycle (one McpServer per `tunnelId`).
   * Implementing this method is what makes BotChannel advertise the
   * `"mcp_tunnel"` capability — without it the runner never claims
   * support and the gateway never forwards Mcp frames to this peer.
   */
  onAgentMcpEnvelope?(
    tunnelId: string,
    payload: Uint8Array,
    reply: import("./channel.js").McpReplyHandle,
  ): Promise<void>;

  /**
   * Optional self-test hook. The channel forwards a
   * `Frame::DiagnoseRequest` here and ships the returned checks back
   * over the wire. Implementing this method is what makes
   * BotChannel advertise the `"diagnose"` capability — without it
   * the runner never claims support and admin diagnose calls
   * short-circuit with `not implemented`.
   */
  onAgentDiagnoseRequested?(
    req: import("./channel.js").DiagnoseRequest,
  ): Promise<import("./channel.js").DiagnoseCheck[]>;

  /**
   * Optional slash-command registrar. Implement when the platform has
   * a server-side command list the client UI surfaces (Telegram
   * `setMyCommands`, Discord application commands, Slack
   * `commands.list`, …); omit otherwise. Called once per successful
   * {@link startBot} with the gateway-authored manifest published via
   * `Frame::SlashManifest` (and re-published on every reconnect).
   * Best-effort: a thrown error is logged but does not fail bot
   * startup, since the bot still functions without the client-side
   * autocomplete affordance.
   */
  registerSlashCommands?(
    handle: BotHandle,
    commands: ReadonlyArray<SlashCommandSpec>,
  ): Promise<void>;
}

/**
 * Optional approvals adapter. A sidecar that wants interactive
 * approval UI (inline keyboards, block-kit, slash components)
 * implements this and hands it to {@link BotChannel}. The SDK
 * resolves the route from `req.userId` and hands it in directly, so
 * the approval code never has to touch routing tables.
 */
export interface PlatformApprovals<
  BotHandle,
  ChatId,
> {
  /**
   * Aura is asking for an approval. `route` is `null` if the user is
   * unknown or the bot has been detached — fail closed with `"deny"`
   * in that case.
   */
  onRequested(
    req: ApprovalRequest,
    route: BotRoute<BotHandle, ChatId> | null,
  ): Promise<ApprovalDecision>;

  /**
   * Aura has resolved an approval it originally requested. Use this
   * to update the platform-side UI (e.g. edit the prompt message to
   * show the final decision).
   */
  onResolved?(callId: string, decision: ApprovalDecision): Promise<void>;

  /**
   * Register bot-specific handlers on a freshly-created bot before
   * polling starts. Invoked by the channel via
   * {@link BotStartHooks.attach}. Omit if your UI doesn't need per-bot
   * wiring.
   */
  attach?(handle: BotHandle, botId: string): void | Promise<void>;

  /**
   * A specific bot went away — explicit StopBot, or a polling crash
   * without an auto-restart yet. Auto-deny or drop any pending
   * approval state that was routed through this bot, so a later
   * `onResolved` or `onStop` doesn't try to talk to a dead handle.
   */
  onBotStopped?(botId: string): void | Promise<void>;

  /** Full channel shutdown — flush any remaining approval state. */
  onStop?(): void | Promise<void>;
}

export interface BotChannelOptions<
  BotHandle,
  ChatId,
> {
  channelType: string;
  logger: Logger;
  platform: BotPlatform<BotHandle, ChatId>;
  approvals?: PlatformApprovals<BotHandle, ChatId>;
  /**
   * Per-instance inbound queue capacity. When the queue fills the
   * channel warns once and drops the oldest entry — losing a message
   * is preferable to unbounded memory growth on a stuck consumer.
   * Default: 1024.
   */
  inboundQueueCapacity?: number;
  /**
   * How often to re-issue the typing indicator while the agent is
   * working. Telegram's `sendChatAction` expires server-side in ~5s,
   * so refresh just under that. Default: 4000 ms.
   */
  typingRefreshMs?: number;
  /**
   * Maximum quiet time between agent outputs before the typing
   * session self-cancels. Inbounds from the user do NOT extend this
   * timer — only the agent producing an `onMessage` / `onNotice` does.
   * Guards against a wedged agent leaving the indicator on forever
   * while the user keeps typing. Default: 60_000 ms.
   */
  typingSafetyMs?: number;
  /**
   * Slash commands to publish on every successful StartBot via
   * {@link BotPlatform.registerSlashCommands}. Empty / omitted means
   * "do not register". Platforms that don't implement
   * `registerSlashCommands` log a warning the first time the channel
   * tries to register and otherwise no-op.
   *
   * The list is the channel's static manifest, not a per-bot one:
   * registering the same set on every bot is the intended behaviour
   * since aura's gateway-side dispatcher is bot-agnostic.
   */
  slashCommands?: ReadonlyArray<SlashCommandSpec>;
}

const DEFAULT_QUEUE_CAPACITY = 1024;
const DEFAULT_TYPING_REFRESH_MS = 4000;
const DEFAULT_TYPING_SAFETY_MS = 60_000;

interface TypingSession {
  timer: ReturnType<typeof setInterval>;
  safety: ReturnType<typeof setTimeout>;
  // Pending turn counter — incremented on inbound, decremented on
  // outbound (onMessage/onNotice). The session only really ends when
  // this reaches zero, so a quick double-send doesn't lose the
  // indicator when the first turn replies while the second is still
  // queued behind aura's per-session serialization.
  pending: number;
  ping: () => void;
}

/**
 * Stable string form for any plain-data chat address. Primitives pass
 * through; objects serialize to `key=value&…` with keys sorted so the
 * output doesn't depend on construction order. Keys that are absent
 * from the object (Telegram DM, no `threadId`) drop out naturally,
 * which is exactly the discrimination we want: `{chatId: 42}` and
 * `{chatId: 42, threadId: 7}` produce different keys → different
 * sessions.
 *
 * Exported so platform code can compute the same chat key the
 * `BotChannel` does — e.g. when constructing the aura user id for an
 * out-of-band action (blob upload) before `hooks.emit` has fired.
 */
export function defaultChatKey(chat: unknown): string {
  if (chat === null || typeof chat !== "object") {
    return String(chat);
  }
  const obj = chat as Record<string, unknown>;
  const keys = Object.keys(obj).sort();
  return keys.map((k) => `${k}=${chatValueToString(obj[k])}`).join("&");
}

// `String(v)` on a nested object yields `[object Object]`, which would
// silently collide every chat into one routing bucket. The contract
// (see BotPlatform docs) is "primitives or plain objects of
// primitives" — but no runtime check enforces it, so a future
// platform passing a nested object would otherwise produce a
// platform-wide session collision before anyone noticed. Fall back
// to JSON.stringify so each distinct nested value at least gets a
// distinct key. Note: JSON.stringify isn't key-order-stable for
// nested objects, so off-contract shapes still risk discriminating
// `{a:{x:1,y:2}}` vs `{a:{y:2,x:1}}` — that's strictly better than
// silent total collision and the contract still says "don't do that."
function chatValueToString(v: unknown): string {
  if (v === null || typeof v !== "object") return String(v);
  return JSON.stringify(v);
}

/** Render an attachment list as `count=N size=B mime=X` (singular when
 * `count==1`) / `mimes=X,Y` (plural with dedup) — the SDK's per-event
 * inbound/outbound logs use this so the operator sees the high-level
 * shape without each attachment getting its own line. */
function formatAttachments(atts: ReadonlyArray<WireAttachment>): string {
  const totalSize = atts.reduce((s, a) => s + a.size, 0);
  const mimes = [...new Set(atts.map((a) => a.mime_type))];
  const mimeStr =
    mimes.length === 1 ? `mime=${mimes[0]}` : `mimes=${mimes.join(",")}`;
  return `count=${atts.length} size=${totalSize} ${mimeStr}`;
}

/**
 * Compose the canonical aura user id `<channelType>_<botId>_<chatKey>_<platformUserId>`.
 *
 * This is the identity the gateway pairs against — both the
 * `Frame::Message.user_id` it stores and the `x-aura-user-id` header
 * the blob `POST /v1/blobs` endpoint expects. `BotChannel.ingest`
 * uses this when emitting inbound events; platform code that needs
 * to act on the user before the event reaches the channel (chiefly
 * uploading inbound media to the gateway, which has its own pairing
 * gate) MUST compose the same id, otherwise the upload pairs against
 * a different identity and lands in `pending` even when the user has
 * already approved their text-message pairing.
 *
 * Pass `chatKey` only if the platform overrides
 * {@link BotPlatform.chatKey}; the same default we apply in
 * `ingest` is used otherwise.
 */
export function composeAuraUserId<ChatId>(
  channelType: string,
  botId: string,
  chat: ChatId,
  platformUserId: string,
  chatKey?: (chat: ChatId) => string,
): string {
  const k = chatKey ? chatKey(chat) : defaultChatKey(chat);
  return `${channelType}_${botId}_${k}_${platformUserId}`;
}

/**
 * `Channel` implementation for sidecars that multiplex many bots
 * (Telegram / Discord / Slack / …).
 *
 * Owns every piece of state a bot-multiplexing sidecar would otherwise
 * hand-maintain: StartBot/StopBot idempotency, the three routing
 * maps (`bots`, `chatByUser`, `botByUser`), the inbound back-
 * pressured queue, StopBot's user-entry purge, teardown fan-out, and
 * the approval-route plumbing that hands pre-resolved {@link BotRoute}
 * tuples to the approval hook.
 *
 * Lifetime: **one instance is long-lived across the SDK runner's
 * auto-reconnect cycles.** `inbound(signal)` is called once per
 * connection; a signal abort terminates that iterator locally without
 * invalidating the channel. Only `onStop()` represents permanent
 * shutdown. Pending inbound events received between a disconnect and
 * the next iterator are buffered (up to `inboundQueueCapacity`).
 *
 * Single-consumer: at most one `inbound(signal)` iterator is active
 * at a time — matches the SDK runner's outbound pump. Driving two
 * concurrent consumers is undefined behavior (the shared waiter slot
 * is clobbered; one consumer hangs). This is inherited from the
 * `Channel` contract, not a `BotChannel` quirk.
 *
 * Type params:
 * - `BotHandle` — opaque platform runtime handle (grammy `Bot`,
 *   discord.js `Client`, …). The channel never compares handles for
 *   identity; generation tracking is internal, so returning the same
 *   value across multiple `startBot` calls is fine.
 * - `ChatId` — the full conversation address cached in routing
 *   tables across bot restarts. Must be plain data (primitive or
 *   plain object of primitives) so it stays stable and serializable;
 *   must NOT be a live platform handle. Composite addresses like
 *   Telegram's `{ chatId, threadId? }` or Slack's
 *   `{ channel, thread_ts? }` are the expected shape when a
 *   platform's conversation boundary isn't a single scalar. See
 *   {@link BotPlatform} for the contract details.
 *
 * Scope: text-first. `sendText` is the primary outbound; `sendNotice`
 * defaults to a prefix-emoji text message. Rich content (embeds,
 * block-kit, file uploads) is out of scope and would need a dedicated
 * channel type, not an extension here.
 *
 * A sidecar's entry point becomes:
 *
 * ```ts
 * void runSidecar({
 *   channelType: "telegram",
 *   build: (logger) => new BotChannel<Bot, number>({
 *     channelType: "telegram",
 *     logger,
 *     platform: new TelegramPlatform(logger),
 *     approvals: new TelegramApprovals(logger),
 *   }),
 * });
 * ```
 */
export class BotChannel<BotHandle, ChatId>
  implements Channel {
  readonly channelType: string;
  private readonly logger: Logger;
  private readonly platform: BotPlatform<BotHandle, ChatId>;
  private readonly approvals:
    | PlatformApprovals<BotHandle, ChatId>
    | undefined;
  private readonly capacity: number;
  private readonly typingRefreshMs: number;
  private readonly typingSafetyMs: number;
  private readonly typingSessions = new Map<string, TypingSession>();
  // Mutable: starts as the constructor-supplied seed (defaults to empty)
  // and is replaced wholesale every time the gateway pushes a fresh
  // `Frame::SlashManifest`. The gateway is the single source of truth
  // once connected; the seed only matters for offline / pre-connect
  // tests where there's no SlashManifest at all.
  private slashCommands: ReadonlyArray<SlashCommandSpec>;
  // Latched the first time we notice the platform configured
  // `slashCommands` but does not implement `registerSlashCommands` —
  // keeps the warning to one line per process even if many bots start.
  private slashCapabilityWarned = false;

  // Slot per botId holds the live handle and a SDK-internal generation
  // counter. `BotHandle` itself is an opaque platform value (grammy
  // `Bot`, discord.js `Client`, …) — we don't rely on it being unique
  // per startBot call. Generation identity is owned by the channel,
  // not the platform, so a transport that returns a stable string or
  // a singleton still gets correct stale-exit rejection.
  private readonly bots = new Map<
    string,
    { handle: BotHandle; gen: number }
  >();
  private nextGen = 0;
  private readonly chatByUser = new Map<string, ChatId>();
  private readonly botByUser = new Map<string, string>();

  private readonly pending: UserInbound[] = [];
  private waiter: ((v: IteratorResult<UserInbound>) => void) | null = null;
  // Set only by `onStop()` — permanent channel shutdown. Per-connection
  // signal aborts terminate the current iterator locally; they must not
  // flip this, or a reconnect's new iterator would see `done:true` up
  // front and `pushInbound` would silently drop everything.
  private stopped = false;

  constructor(opts: BotChannelOptions<BotHandle, ChatId>) {
    this.channelType = opts.channelType;
    this.logger = opts.logger;
    this.platform = opts.platform;
    this.approvals = opts.approvals;
    this.capacity = opts.inboundQueueCapacity ?? DEFAULT_QUEUE_CAPACITY;
    this.typingRefreshMs = opts.typingRefreshMs ?? DEFAULT_TYPING_REFRESH_MS;
    this.typingSafetyMs = opts.typingSafetyMs ?? DEFAULT_TYPING_SAFETY_MS;
    this.slashCommands = opts.slashCommands ?? [];

    // Forward MCP / diagnose round-trip hooks only when the platform
    // implements them. The SDK runner inspects the channel object
    // for these methods to decide whether to advertise the matching
    // capability on `Register`; defining them unconditionally would
    // claim support a no-op handler can't honour, and the gateway-
    // side caller would time out waiting for a reply.
    const mcpHook = opts.platform.onAgentMcpEnvelope;
    if (mcpHook) {
      this.onMcpEnvelope = (tunnelId, payload, reply) =>
        mcpHook.call(opts.platform, tunnelId, payload, reply);
    }
    const diagHook = opts.platform.onAgentDiagnoseRequested;
    if (diagHook) {
      this.onDiagnoseRequested = (req) => diagHook.call(opts.platform, req);
    }
  }

  // Installed conditionally in the constructor; declared here so the
  // type system sees the optional Channel hooks even when the
  // platform doesn't implement the matching `onAgent*` method.
  onMcpEnvelope?: (
    tunnelId: string,
    payload: Uint8Array,
    reply: import("./channel.js").McpReplyHandle,
  ) => Promise<void>;
  onDiagnoseRequested?: (
    req: import("./channel.js").DiagnoseRequest,
  ) => Promise<import("./channel.js").DiagnoseCheck[]>;

  inbound(signal: AbortSignal): AsyncIterable<UserInbound> {
    const self = this;
    return {
      [Symbol.asyncIterator](): AsyncIterator<UserInbound> {
        let finished = signal.aborted || self.stopped;
        const release = (): void => {
          if (finished) return;
          finished = true;
          if (self.waiter) {
            const waiter = self.waiter;
            self.waiter = null;
            waiter({ value: undefined, done: true });
          }
        };
        let onAbort: (() => void) | null = null;
        if (!signal.aborted) {
          onAbort = () => release();
          signal.addEventListener("abort", onAbort, { once: true });
        }
        return {
          next(): Promise<IteratorResult<UserInbound>> {
            if (finished) {
              return Promise.resolve({ value: undefined, done: true });
            }
            return self.nextInbound();
          },
          async return(): Promise<IteratorResult<UserInbound>> {
            if (onAbort) signal.removeEventListener("abort", onAbort);
            release();
            return { value: undefined, done: true };
          },
        };
      },
    };
  }

  async onMessage(msg: AgentMessage): Promise<void> {
    this.completeTypingTurn(msg.userId);
    const route = this.route(msg.userId);
    if (!route) {
      this.logger.warn(
        "agent message for unknown user or detached bot; dropping",
        msg.userId,
      );
      return;
    }
    const sendMediaPath =
      msg.attachments !== undefined
      && msg.attachments.length > 0
      && this.platform.sendMedia !== undefined;
    this.logOutbound(route.botId, route.chat, msg, sendMediaPath);
    try {
      if (sendMediaPath) {
        await this.platform.sendMedia!(
          route.handle,
          route.chat,
          {
            text: msg.content,
            attachments: msg.attachments!,
          },
          msg.userId,
        );
      } else {
        if (msg.attachments !== undefined && msg.attachments.length > 0) {
          // Platform doesn't implement sendMedia — surface this loudly
          // because the user's reply just lost its non-text payload.
          // The text part still ships so the conversation isn't broken.
          this.logger.warn(
            `dropping ${msg.attachments.length} attachment(s) — platform does not implement sendMedia`,
          );
        }
        await this.platform.sendText(
          route.handle,
          route.chat,
          msg.content,
          msg.userId,
        );
      }
    } catch (err) {
      this.logger.error(
        sendMediaPath ? "sendMedia failed" : "sendText failed",
        err,
      );
    }
  }

  async onDelta(delta: AgentDelta): Promise<void> {
    const handler = this.platform.onAgentDelta;
    if (!handler) {
      // No streaming surface — keep the typing indicator alive so the
      // user sees ongoing activity while the agent produces deltas.
      this.bumpTypingSafety(delta.userId);
      return;
    }
    const route = this.route(delta.userId);
    if (!route) {
      this.logger.debug("delta for unknown user; dropping", delta.userId);
      return;
    }
    // Streaming UI replaces typing — cancel the indicator so we don't
    // double-up and so a wedged typing session can't outlive the turn.
    this.cancelTyping(delta.userId);
    try {
      await handler.call(
        this.platform,
        route.handle,
        route.chat,
        delta.userId,
        delta.text,
      );
    } catch (err) {
      this.logger.debug("onAgentDelta failed", err);
    }
  }

  async onToolCallStarted(
    ev: import("./channel.js").AgentToolCallStarted,
  ): Promise<void> {
    const handler = this.platform.onAgentToolCallStarted;
    if (!handler) return;
    const route = this.route(ev.userId);
    if (!route) {
      this.logger.debug("tool-call started for unknown user; dropping", ev.userId);
      return;
    }
    try {
      await handler.call(this.platform, route.handle, route.chat, ev.userId, ev);
    } catch (err) {
      this.logger.debug("onAgentToolCallStarted failed", err);
    }
  }

  async onToolCallCompleted(
    ev: import("./channel.js").AgentToolCallCompleted,
  ): Promise<void> {
    const handler = this.platform.onAgentToolCallCompleted;
    if (!handler) return;
    const route = this.route(ev.userId);
    if (!route) {
      this.logger.debug("tool-call completed for unknown user; dropping", ev.userId);
      return;
    }
    try {
      await handler.call(this.platform, route.handle, route.chat, ev.userId, ev);
    } catch (err) {
      this.logger.debug("onAgentToolCallCompleted failed", err);
    }
  }

  // MCP and diagnose are different from delta/tool-call: the gateway
  // expects a reply (round-trip semantics) and times out if it
  // doesn't get one. We therefore can't use the "method always
  // present, silent no-op when platform doesn't implement" pattern
  // the streaming hooks use — claiming the capability without an
  // actual handler would have the agent's MCP client time out.
  // Instead, the constructor below conditionally installs these
  // methods only when the platform implements the matching `onAgent*`
  // hook; the SDK runner's capability auto-advertise then reads them
  // correctly.

  async onNotice(notice: AgentNotice): Promise<void> {
    this.completeTypingTurn(notice.userId);
    const route = this.route(notice.userId);
    if (!route) {
      this.logger.debug("notice for unknown user; dropping", notice.userId);
      return;
    }
    try {
      if (this.platform.sendNotice) {
        await this.platform.sendNotice(route.handle, route.chat, notice);
      } else {
        const prefix = notice.level === "error" ? "❌" : "⚠️";
        await this.platform.sendText(
          route.handle,
          route.chat,
          `${prefix} ${notice.text}`,
          notice.userId,
        );
      }
    } catch (err) {
      this.logger.error("notice send failed", err);
    }
  }

  async onApprovalRequested(req: ApprovalRequest): Promise<ApprovalDecision> {
    // Approval UI is about to render to the user; the bot is waiting
    // on them, not processing. Force-cancel regardless of how many
    // turns are pending — they're all effectively paused until the
    // user interacts.
    this.cancelTyping(req.userId);
    if (!this.approvals) {
      this.logger.warn(
        "approval request received but no approvals hook is configured; auto-denying",
        req.callId,
      );
      return "deny";
    }
    const route = this.route(req.userId);
    return this.approvals.onRequested(req, route);
  }

  async onApprovalResolved(
    callId: string,
    decision: ApprovalDecision,
  ): Promise<void> {
    const fn = this.approvals?.onResolved;
    if (fn) await fn.call(this.approvals, callId, decision);
  }

  async onStartBot(cmd: StartBotCommand): Promise<BotStatusReport> {
    if (this.bots.has(cmd.botId)) {
      return {
        botId: cmd.botId,
        ok: true,
        message: "already running",
      };
    }
    let out: {
      handle: BotHandle;
      username?: string;
      waitForExit?: Promise<void>;
    };
    try {
      out = await this.platform.startBot(cmd, {
        emit: (ev) => this.ingest(cmd.botId, ev),
        attach: (h) => this.attachApprovals(h, cmd.botId),
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return {
        botId: cmd.botId,
        ok: false,
        message: `start failed: ${message}`,
      };
    }
    const gen = this.nextGen++;
    this.bots.set(cmd.botId, { handle: out.handle, gen });
    // Bind the exit promise to this specific generation. A stale
    // resolution from a prior generation's `waitForExit` carries the
    // prior `gen`, which `onPollingExit` compares against the current
    // slot — so it can't delete a freshly-spawned replacement under
    // the same botId regardless of what value `BotHandle` is.
    if (out.waitForExit) {
      out.waitForExit.then(
        () => this.onPollingExit(cmd.botId, gen, undefined),
        (err: unknown) => this.onPollingExit(cmd.botId, gen, err),
      );
    }
    await this.publishSlashCommands(cmd.botId, out.handle);
    this.logger.info(
      `bot started channel=${this.channelType} bot=${cmd.botId}` +
        (out.username ? ` username=${out.username}` : ""),
    );
    return {
      botId: cmd.botId,
      ok: true,
      message: out.username ? `running as @${out.username}` : "running",
    };
  }

  async onSlashManifest(
    commands: ReadonlyArray<SlashCommandSpec>,
  ): Promise<void> {
    // Snapshot the field synchronously before the first await so any
    // concurrent `onStartBot` reads the new manifest, not the old one.
    this.slashCommands = [...commands];
    if (this.slashCommands.length === 0) return;
    const live = [...this.bots.entries()];
    for (const [botId, entry] of live) {
      await this.publishSlashCommands(botId, entry.handle);
    }
  }

  private async publishSlashCommands(
    botId: string,
    handle: BotHandle,
  ): Promise<void> {
    if (this.slashCommands.length === 0) return;
    const register = this.platform.registerSlashCommands;
    if (!register) {
      if (!this.slashCapabilityWarned) {
        this.slashCapabilityWarned = true;
        this.logger.warn(
          `slashCommands configured but platform does not implement registerSlashCommands; skipping (bot=${botId})`,
        );
      }
      return;
    }
    try {
      await register.call(this.platform, handle, this.slashCommands);
    } catch (err) {
      this.logger.warn(
        `registerSlashCommands failed for bot=${botId}; bot remains live without command UI`,
        err,
      );
    }
  }

  async onStopBot(cmd: StopBotCommand): Promise<BotStatusReport> {
    const entry = this.bots.get(cmd.botId);
    // Always flush approvals + purge routes, even when the handle is
    // already gone (polling crashed earlier). The operator's intent
    // is "detach this tenant"; stale routes must not survive to
    // resurrect under a later StartBot.
    await this.notifyBotStopped(cmd.botId);
    this.purgeBot(cmd.botId);
    if (entry === undefined) {
      return { botId: cmd.botId, ok: true, message: "not running" };
    }
    try {
      await this.platform.stopBot(entry.handle);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      return {
        botId: cmd.botId,
        ok: false,
        message: `stop failed: ${message}`,
      };
    }
    this.logger.info(
      `bot stopped channel=${this.channelType} bot=${cmd.botId}`,
    );
    return { botId: cmd.botId, ok: true };
  }

  async onStop(): Promise<void> {
    const onStop = this.approvals?.onStop;
    if (onStop) {
      try {
        await onStop.call(this.approvals);
      } catch (err) {
        this.logger.debug("approvals.onStop threw", err);
      }
    }
    this.stopped = true;
    this.cancelAllTyping();
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = null;
      waiter({ value: undefined, done: true });
    }
    const handles = [...this.bots.values()].map((e) => e.handle);
    this.bots.clear();
    this.botByUser.clear();
    this.chatByUser.clear();
    for (const handle of handles) {
      try {
        await this.platform.stopBot(handle);
      } catch (err) {
        this.logger.debug("stopBot threw on onStop", err);
      }
    }
  }

  private async attachApprovals(
    handle: BotHandle,
    botId: string,
  ): Promise<void> {
    const attach = this.approvals?.attach;
    if (!attach) return;
    await attach.call(this.approvals, handle, botId);
  }

  private route(userId: string): BotRoute<BotHandle, ChatId> | null {
    const botId = this.botByUser.get(userId);
    if (botId === undefined) return null;
    const chat = this.chatByUser.get(userId);
    if (chat === undefined) return null;
    const entry = this.bots.get(botId);
    if (entry === undefined) return null;
    return { botId, handle: entry.handle, chat };
  }

  private ingest(botId: string, ev: BotInboundEvent<ChatId>): void {
    const userId = composeAuraUserId(
      this.channelType,
      botId,
      ev.chat,
      ev.platformUserId,
      this.platform.chatKey?.bind(this.platform),
    );
    this.chatByUser.set(userId, ev.chat);
    this.botByUser.set(userId, botId);
    this.logInbound(botId, ev);
    this.startOrRefreshTyping(userId, botId, ev.chat);
    this.pushInbound({
      sessionId: "",
      userId,
      content: ev.content,
      botId,
      ...(ev.platformMsgId ? { platformMsgId: ev.platformMsgId } : {}),
      ...(ev.attachments && ev.attachments.length > 0
        ? { attachments: ev.attachments }
        : {}),
    });
  }

  /** Render a chat using the platform's `chatKey` if defined,
   * otherwise fall through to {@link defaultChatKey}. Same identity
   * `composeAuraUserId` keys on, so a log line's `chat=` token matches
   * the chat-portion of the `user=` (Aura user id) it appears with. */
  private renderChat(chat: ChatId): string {
    return this.platform.chatKey
      ? this.platform.chatKey.call(this.platform, chat)
      : defaultChatKey(chat);
  }

  private logInbound(botId: string, ev: BotInboundEvent<ChatId>): void {
    const base =
      `channel=${this.channelType} bot=${botId}` +
      ` chat=${this.renderChat(ev.chat)} user=${ev.platformUserId}`;
    if (ev.attachments && ev.attachments.length > 0) {
      const textPart = ev.content ? ` text_len=${ev.content.length}` : "";
      this.logger.info(
        `inbound media ${base} ${formatAttachments(ev.attachments)}${textPart}`,
      );
    } else {
      this.logger.info(`inbound text ${base} len=${ev.content.length}`);
    }
  }

  private logOutbound(
    botId: string,
    chat: ChatId,
    msg: AgentMessage,
    sendMediaPath: boolean,
  ): void {
    const base =
      `channel=${this.channelType} bot=${botId}` +
      ` chat=${this.renderChat(chat)}`;
    if (sendMediaPath && msg.attachments) {
      const textPart = msg.content ? ` text_len=${msg.content.length}` : "";
      this.logger.info(
        `outbound media ${base} ${formatAttachments(msg.attachments)}${textPart}`,
      );
    } else {
      this.logger.info(`outbound text ${base} len=${msg.content.length}`);
    }
  }

  private startOrRefreshTyping(
    userId: string,
    botId: string,
    chat: ChatId,
  ): void {
    const notify = this.platform.notifyTyping;
    if (!notify) return;
    // Polling may deliver the first message before `onStartBot` has
    // finished populating `bots` (grammy starts polling inside
    // `bot.start()` which we don't await). Skip the indicator in that
    // narrow window rather than crash — losing typing on the first
    // post-startup message is not user-visible.
    const entry = this.bots.get(botId);
    if (entry === undefined) return;
    const handle = entry.handle;
    const existing = this.typingSessions.get(userId);
    if (existing !== undefined) {
      existing.pending += 1;
      // Do NOT reset safety: it's scoped to agent liveness (quiet
      // time between outbounds), not to the user's typing cadence.
      existing.ping();
      return;
    }
    const ping = (): void => {
      notify.call(this.platform, handle, chat).catch((err: unknown) => {
        this.logger.debug("notifyTyping failed", err);
      });
    };
    ping();
    // .unref() so an orphan session can't block process exit; the
    // sidecar runner's normal shutdown path still calls onStop() which
    // clears tickers explicitly.
    const timer = setInterval(ping, this.typingRefreshMs);
    timer.unref?.();
    const safety = setTimeout(
      () => this.cancelTyping(userId),
      this.typingSafetyMs,
    );
    safety.unref?.();
    this.typingSessions.set(userId, { timer, safety, pending: 1, ping });
  }

  private completeTypingTurn(userId: string): void {
    const session = this.typingSessions.get(userId);
    if (session === undefined) return;
    session.pending -= 1;
    if (session.pending <= 0) {
      this.cancelTyping(userId);
      return;
    }
    // Agent is alive — proven by this outbound. Reset the quiet-time
    // cap so the ticker can keep going through the rest of the queue.
    clearTimeout(session.safety);
    session.safety = setTimeout(
      () => this.cancelTyping(userId),
      this.typingSafetyMs,
    );
    session.safety.unref?.();
  }

  private bumpTypingSafety(userId: string): void {
    const session = this.typingSessions.get(userId);
    if (session === undefined) return;
    clearTimeout(session.safety);
    session.safety = setTimeout(
      () => this.cancelTyping(userId),
      this.typingSafetyMs,
    );
    session.safety.unref?.();
  }

  private cancelTyping(userId: string): void {
    const session = this.typingSessions.get(userId);
    if (session === undefined) return;
    clearInterval(session.timer);
    clearTimeout(session.safety);
    this.typingSessions.delete(userId);
  }

  private cancelAllTypingForBot(botId: string): void {
    for (const [userId, bid] of this.botByUser.entries()) {
      if (bid === botId) this.cancelTyping(userId);
    }
  }

  private cancelAllTyping(): void {
    for (const userId of [...this.typingSessions.keys()]) {
      this.cancelTyping(userId);
    }
  }

  private onPollingExit(
    botId: string,
    gen: number,
    err: unknown,
  ): void {
    // Identity guard: a stale `waitForExit` from a previous generation
    // must not delete a handle that a fresh StartBot has since
    // installed under the same botId.
    const entry = this.bots.get(botId);
    if (entry === undefined || entry.gen !== gen) return;
    if (err !== undefined) {
      this.logger.error(`bot '${botId}' polling exited`, err);
    }
    // Drop the live handle but keep `(userId → botId, chat)` intact
    // so a follow-up `StartBot` (reconciler-driven reconnect) can
    // resume existing sessions without waiting for every user to
    // re-message. Pending approvals for this bot are still released
    // — the promises can't finish against a dead handle.
    this.bots.delete(botId);
    this.cancelAllTypingForBot(botId);
    void this.notifyBotStopped(botId).catch((hookErr) => {
      this.logger.debug("approvals.onBotStopped threw on polling exit", hookErr);
    });
  }

  private async notifyBotStopped(botId: string): Promise<void> {
    const hook = this.approvals?.onBotStopped;
    if (!hook) return;
    try {
      await hook.call(this.approvals, botId);
    } catch (err) {
      this.logger.debug("approvals.onBotStopped threw", err);
    }
  }

  private purgeBot(botId: string): void {
    this.bots.delete(botId);
    for (const [userId, bid] of this.botByUser.entries()) {
      if (bid === botId) {
        this.cancelTyping(userId);
        this.botByUser.delete(userId);
        this.chatByUser.delete(userId);
      }
    }
  }

  private pushInbound(msg: UserInbound): void {
    if (this.stopped) return;
    if (this.waiter) {
      const waiter = this.waiter;
      this.waiter = null;
      waiter({ value: msg, done: false });
      return;
    }
    if (this.pending.length >= this.capacity) {
      this.logger.warn(
        `inbound queue at capacity (${this.capacity}); dropping oldest`,
      );
      this.pending.shift();
    }
    this.pending.push(msg);
  }

  private nextInbound(): Promise<IteratorResult<UserInbound>> {
    const ready = this.pending.shift();
    if (ready !== undefined) {
      return Promise.resolve({ value: ready, done: false });
    }
    if (this.stopped) {
      return Promise.resolve({ value: undefined, done: true });
    }
    return new Promise<IteratorResult<UserInbound>>((resolve) => {
      this.waiter = resolve;
    });
  }
}
