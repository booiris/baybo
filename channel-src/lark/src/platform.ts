import type {
  AgentNotice,
  DiagnoseCheck,
  DiagnoseRequest,
  Logger,
  StartBotCommand,
  WireAttachment,
} from "@aura/channel-sdk";
import { BlobPairingRequiredError } from "@aura/channel-sdk";
import { composeAuraUserId } from "@aura/channel-sdk/bot";
import type {
  BotInboundEvent,
  BotMediaPayload,
  BotPlatform,
  BotStartHooks,
} from "@aura/channel-sdk/bot";
import * as lark from "@larksuiteoapi/node-sdk";

import type { LarkApprovals } from "./approvals.js";
import {
  parseBotRuntimeConfig,
  parseStartBotCredentials,
  type BotRuntimeConfig,
} from "./auth/credentials.js";
import { Semaphore } from "./concurrency.js";
import { LarkMcpServer } from "./mcp/server.js";
import { downloadResourceAsAttachment } from "./media/inbound.js";
import { sendLarkAttachments } from "./media/outbound.js";
import { cleanInboundContent } from "./messaging/inbound.js";
import { LarkStreamingSession } from "./streaming.js";

export const CHANNEL_TYPE = "lark";

/**
 * Conversation address. `chatId` is the Feishu chat id (`oc_…` for
 * groups, `p2p_…` form for DMs). Phase 1 ignores Feishu threads — a
 * thread-aware route would need `replyTo` plumbing on every outbound
 * send (the SDK's `replyInThread` flag is silently dropped when
 * `replyTo` isn't set) plus a per-route most-recent-message anchor.
 * That arrives with the streaming-card work in Phase 2; for now,
 * thread-mention replies post into the parent group like every other
 * inbound, which is correct (visible to the user) even if it isn't
 * thread-pretty.
 */
export interface LarkChat {
  chatId: string;
}

interface BotState {
  handle: lark.LarkChannel;
  config: BotRuntimeConfig;
}

// Two simultaneous download phases per bot. One slot makes everything
// strictly serial (latency penalty for unrelated peers); raising it
// past two doesn't help the common case (a single user rarely sends
// two attachment-heavy messages in flight) and weakens the cap on
// peak in-flight memory (`permits × MAX_RESOURCE_BYTES`).
const MEDIA_CONCURRENCY = 2;

export class LarkPlatform implements BotPlatform<lark.LarkChannel, LarkChat> {
  // Per-bot runtime state: streaming/reaction toggles. Keyed on
  // `botId` (StartBot is idempotent at the SDK layer).
  private readonly bots = new Map<string, BotState>();
  // MCP server stub. Single instance per platform — JSON-RPC envelopes
  // arrive with their own `id`s so concurrent agent sessions sharing
  // a single sidecar don't collide. Phase 3.3 slice 2 swaps this for
  // a real `@modelcontextprotocol/sdk` server hosting the OAPI tools.
  private readonly mcpServer: LarkMcpServer;
  // Per-userId streaming session: at most one card streams to a given
  // (channelType, botId, chatKey, platformUserId) tuple at a time.
  // Aura's gateway already serialises a session's outbound, so the
  // map collisions only happen across distinct sessions in the same
  // chat — those collisions are correct (interleaving cards would be
  // worse).
  private readonly streams = new Map<string, LarkStreamingSession>();
  // `auraUserId → { botId, chatId, platformUserId }` cache populated
  // from each inbound dispatch. The MCP `feishu_ask_user` tool reads
  // it via `_meta.auraUserId` to figure out which Lark conversation
  // the agent is replying in — the auraUserId encodes chat + user but
  // chat ids contain underscores, so a string-decode would be
  // fragile. The map is the source of truth.
  private readonly contextByAuraUser = new Map<
    string,
    { botId: string; chatId: string; platformUserId: string }
  >();
  // One-shot `feishu_ask_user` waiters keyed by
  // `${botId}|${chatId}|${platformUserId}` — the next inbound from
  // that thread fulfils the waiter and is NOT forwarded to the
  // agent (it's the answer to the agent's tool call, not a new
  // user turn).
  private readonly pendingQuestions = new Map<
    string,
    {
      resolve: (text: string) => void;
      reject: (err: Error) => void;
      timer: ReturnType<typeof setTimeout>;
    }
  >();

  constructor(
    private readonly logger: Logger,
    private readonly approvals: LarkApprovals,
  ) {
    this.mcpServer = new LarkMcpServer({
      logger,
      // Slice 2A: single-bot deployments resolve to the only bot.
      // Multi-bot disambiguation:
      //   1. If the call carries `_meta.auraBotId` and that bot is
      //      live, route to it directly. This is the slice 2F path;
      //      the gateway threads `Session::user::bot_id` through.
      //   2. Otherwise, fall back to slice 2A's three-state behaviour:
      //      single-bot → `ok`, none → `none`, multi-bot → `ambiguous`
      //      (fail closed; silently picking a bot would leak cross-
      //      tenant data).
      channelResolver: ({ auraBotId }) => {
        if (auraBotId !== undefined) {
          const state = this.bots.get(auraBotId);
          if (state) return { kind: "ok", channel: state.handle };
          // The supplied bot id isn't connected. Don't silently fall
          // through to single-bot — the caller specifically asked for
          // a bot we don't have. Surface as `none` so the tool error
          // matches "the requested tenant isn't reachable".
          return { kind: "none" };
        }
        const handles = [...this.bots.values()].map((s) => s.handle);
        if (handles.length === 0) return { kind: "none" };
        if (handles.length === 1) return { kind: "ok", channel: handles[0]! };
        return { kind: "ambiguous", bot_count: handles.length };
      },
      askUser: (input, prompt, timeoutMs) =>
        this.askUser(input, prompt, timeoutMs),
    });
  }

  /**
   * Send `prompt` into the conversation we last saw `auraUserId` reply
   * on, then await the user's next message in that thread.
   *
   * Returns the reply text on success, `null` on timeout. The next
   * inbound matching `(botId, chatId, platformUserId)` is intercepted
   * in `dispatchInbound` before it would otherwise reach the agent
   * loop — answering an agent's tool call must NOT also fire a fresh
   * user turn (the agent would then see the answer twice and could
   * loop on the same question).
   */
  async askUser(
    input: { auraUserId?: string; auraBotId?: string },
    prompt: string,
    timeoutMs: number,
  ): Promise<{ kind: "ok"; text: string } | { kind: "no_context" } | { kind: "timeout" }> {
    if (!input.auraUserId) return { kind: "no_context" };
    const ctx = this.contextByAuraUser.get(input.auraUserId);
    if (!ctx) return { kind: "no_context" };
    // Belt-and-braces: if the call carries an explicit auraBotId,
    // make sure it matches the cached context. Mismatch likely means
    // the caller (LLM-supplied tool args) crossed wires; fail safe.
    if (input.auraBotId && input.auraBotId !== ctx.botId) {
      return { kind: "no_context" };
    }
    const state = this.bots.get(ctx.botId);
    if (!state) return { kind: "no_context" };

    const key = `${ctx.botId}|${ctx.chatId}|${ctx.platformUserId}`;
    // A second concurrent ask_user against the same thread would
    // race on the next inbound; the older one is replaced. The
    // displaced waiter's reject lets the older tool call surface
    // a clean "superseded" error rather than hanging forever.
    const existing = this.pendingQuestions.get(key);
    if (existing) {
      clearTimeout(existing.timer);
      existing.reject(
        new Error("feishu_ask_user superseded by a concurrent call to the same thread"),
      );
      this.pendingQuestions.delete(key);
    }

    try {
      await state.handle.send(ctx.chatId, { text: prompt });
    } catch (err) {
      this.logger.warn(
        `feishu_ask_user prompt send failed bot=${ctx.botId} chat=${ctx.chatId}: ${String(err)}`,
      );
      return { kind: "timeout" };
    }

    return await new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pendingQuestions.delete(key);
        resolve({ kind: "timeout" });
      }, timeoutMs);
      this.pendingQuestions.set(key, {
        resolve: (text) => {
          clearTimeout(timer);
          resolve({ kind: "ok", text });
        },
        reject: (err) => {
          clearTimeout(timer);
          reject(err);
        },
        timer,
      });
    });
  }

  async onAgentMcpEnvelope(
    tunnelId: string,
    payload: Uint8Array,
    reply: import("@aura/channel-sdk").McpReplyHandle,
  ): Promise<void> {
    await this.mcpServer.accept(tunnelId, payload, reply);
  }

  async sendText(
    handle: lark.LarkChannel,
    chat: LarkChat,
    text: string,
    userId: string,
  ): Promise<void> {
    const stream = this.streams.get(userId);
    if (stream) {
      // Finalise the live streaming card with the canonical body.
      // BotChannel cancelled typing already; nothing else to do.
      this.streams.delete(userId);
      try {
        await stream.finish(text);
        return;
      } catch (err) {
        this.logger.warn(
          `lark streaming finalise failed; falling back to plain send: ${String(err)}`,
        );
        // Fall through to a plain `send` so the user still sees the
        // reply even when the SDK's stream pipeline broke.
      }
    }
    await handle.send(chat.chatId, { text });
  }

  async sendNotice(
    handle: lark.LarkChannel,
    chat: LarkChat,
    notice: AgentNotice,
  ): Promise<void> {
    // Notices are out-of-band: even mid-stream, they should land as a
    // separate message with the warn/error prefix the user expects.
    // Don't try to fold them into the streaming card.
    const prefix = notice.level === "error" ? "❌" : "⚠️";
    await handle.send(chat.chatId, { text: `${prefix} ${notice.text}` });
  }

  async onAgentDiagnoseRequested(req: DiagnoseRequest): Promise<DiagnoseCheck[]> {
    const state = this.bots.get(req.botId);
    if (!state) {
      return [
        {
          name: "bot_state",
          status: "error",
          detail: `bot '${req.botId}' is not currently running on this sidecar`,
        },
      ];
    }
    const checks: DiagnoseCheck[] = [];

    const identity = state.handle.botIdentity;
    if (identity) {
      checks.push({
        name: "bot_identity",
        status: "ok",
        detail: `name=${identity.name} open_id=${identity.openId}`,
      });
    } else {
      checks.push({
        name: "bot_identity",
        status: "warn",
        detail: "botIdentity not yet populated; channel may still be connecting",
      });
    }

    // The Lark SDK's connect() resolves only after a successful WS
    // handshake. If we have a state entry with the live handle here,
    // the WS was healthy at startup. We don't trigger an extra probe
    // just to test it — the SDK's auto-reconnect would mask a flap
    // anyway, and a "send the bot a self-ping" check would require a
    // real chatId to target.
    checks.push({
      name: "transport",
      status: "ok",
      detail: "websocket transport attached",
    });

    checks.push({
      name: "config",
      status: "ok",
      detail: `streaming=${state.config.streaming} reaction_echo=${state.config.reactionEcho}`,
    });

    return checks;
  }

  async stopBot(handle: lark.LarkChannel): Promise<void> {
    const botId = this.botIdFromHandle(handle);
    if (botId !== null) {
      this.purgeBotStreams(botId);
      this.bots.delete(botId);
    }
    try {
      await handle.disconnect();
    } catch (err) {
      this.logger.debug(`lark disconnect raised; ignoring: ${String(err)}`);
    }
  }

  private botIdFromHandle(handle: lark.LarkChannel): string | null {
    for (const [botId, state] of this.bots) {
      if (state.handle === handle) return botId;
    }
    return null;
  }

  private purgeBotStreams(botId: string): void {
    const prefix = `${CHANNEL_TYPE}_${botId}_`;
    for (const [userId, stream] of this.streams) {
      if (!userId.startsWith(prefix)) continue;
      this.streams.delete(userId);
      // Best-effort flush so the SDK's stream() promise resolves and
      // the producer's buffer is freed; if the channel is already
      // disconnected this rejects and we swallow.
      void stream.finish().catch(() => undefined);
    }
  }

  async sendMedia(
    handle: lark.LarkChannel,
    chat: LarkChat,
    payload: BotMediaPayload,
    userId: string,
  ): Promise<void> {
    // Media + streaming card don't compose: media wants its own
    // attachments and a caption, while the card holds markdown.
    // Finalise the stream with whatever caption text exists, then
    // ship the attachments separately. The user sees the card with
    // the caption text and a follow-up media message.
    const stream = this.streams.get(userId);
    if (stream) {
      this.streams.delete(userId);
      try {
        await stream.finish(payload.text);
      } catch (err) {
        this.logger.debug(
          `lark streaming finalise failed before sendMedia: ${String(err)}`,
        );
      }
    }
    await sendLarkAttachments({
      channel: handle,
      chat,
      payload,
      logger: this.logger,
    });
  }

  async onAgentDelta(
    handle: lark.LarkChannel,
    chat: LarkChat,
    userId: string,
    text: string,
  ): Promise<void> {
    if (text.length === 0) return;
    const stream = this.ensureStream(handle, chat, userId);
    if (!stream) return;
    stream.append(text);
  }

  async onAgentToolCallStarted(
    handle: lark.LarkChannel,
    chat: LarkChat,
    userId: string,
    ev: import("@aura/channel-sdk").AgentToolCallStarted,
  ): Promise<void> {
    const stream = this.ensureStream(handle, chat, userId);
    if (!stream) return;
    stream.setToolCallRunning(ev.callId, ev.tool, ev.paramsPreview);
  }

  async onAgentToolCallCompleted(
    handle: lark.LarkChannel,
    chat: LarkChat,
    userId: string,
    ev: import("@aura/channel-sdk").AgentToolCallCompleted,
  ): Promise<void> {
    const stream = this.streams.get(userId);
    // No session means the tool fired before any delta opened a card;
    // dropping the indicator is fine — onMessage will land the agent's
    // final reply via `sendText` regardless.
    if (!stream) return;
    stream.setToolCallCompleted(ev.callId, ev.error !== undefined);
  }

  private ensureStream(
    handle: lark.LarkChannel,
    chat: LarkChat,
    userId: string,
  ): LarkStreamingSession | null {
    const botId = this.botIdFromHandle(handle);
    const config = botId ? this.bots.get(botId)?.config : undefined;
    if (config && !config.streaming) return null;
    let stream = this.streams.get(userId);
    if (!stream) {
      stream = new LarkStreamingSession(handle, chat.chatId, this.logger);
      this.streams.set(userId, stream);
    }
    return stream;
  }

  async startBot(
    cmd: StartBotCommand,
    hooks: BotStartHooks<lark.LarkChannel, LarkChat>,
  ): Promise<{
    handle: lark.LarkChannel;
    username?: string;
  }> {
    const creds = parseStartBotCredentials(cmd);
    const config = parseBotRuntimeConfig(cmd);

    const channel = lark.createLarkChannel({
      appId: creds.appId,
      appSecret: creds.appSecret,
      transport: "websocket",
      domain: creds.domain,
      // The SDK's safety pipeline already covers what openclaw's
      // `inbound/dedup.ts` does (replay filter, stale-message cutoff)
      // and what `policy.ts` does (group/DM/mention-required gates).
      // Phase 2 keeps these knobs at the SDK defaults; per-bot policy
      // overrides (allowlists, dmMode toggles) wait for Phase 3 when
      // operator UX for them is built out.
      policy: { requireMention: true, dmMode: "open" },
    });

    const downloadSlots = new Semaphore(MEDIA_CONCURRENCY);
    channel.on("message", (msg) =>
      this.dispatchInbound(
        channel,
        cmd.botId,
        msg,
        hooks.emit,
        config,
        downloadSlots,
      ),
    );
    channel.on("cardAction", (ev) => this.approvals.handleCardAction(ev));
    // The SDK fires `error` for outbound failures (rate_limited,
    // format_error, ssrf_blocked, …) and reconnect exhaustion. None of
    // them mean "the bot is dead" — the WSClient handles transient WS
    // drops internally. Just log so operators see them; bot lifecycle
    // is owned by `stopBot`/`disconnect()`, not by an event.
    channel.on("error", (err) =>
      this.logger.error(
        `lark channel error code=${err.code} message=${err.message}`,
      ),
    );

    await hooks.attach(channel);
    await channel.connect();
    const username = channel.botIdentity?.name;

    // Stash state only after `connect()` succeeds; on failure we never
    // entered live state, so leaving the map untouched keeps the
    // soft-error path clean. The handle is the same instance that
    // BotChannel will pass back to `stopBot` later.
    this.bots.set(cmd.botId, { handle: channel, config });

    return username ? { handle: channel, username } : { handle: channel };
  }

  private async dispatchInbound(
    channel: lark.LarkChannel,
    botId: string,
    msg: lark.NormalizedMessage,
    emit: (ev: BotInboundEvent<LarkChat>) => void,
    config: BotRuntimeConfig,
    downloadSlots: Semaphore,
  ): Promise<void> {
    const address: LarkChat = { chatId: msg.chatId };
    const platformUserId = msg.senderId;
    const platformMsgId = msg.messageId;

    const auraUserId = composeAuraUserId(
      CHANNEL_TYPE,
      botId,
      address,
      platformUserId,
    );

    // Record the conversation context BEFORE the pending-question
    // intercept. A reply that fulfils a `feishu_ask_user` waiter
    // should still update the cache so a follow-up `feishu_ask_user`
    // against the same auraUserId resolves to the same chat.
    this.contextByAuraUser.set(auraUserId, {
      botId,
      chatId: msg.chatId,
      platformUserId,
    });

    // `feishu_ask_user` intercept: if the agent fired an
    // ask-user tool call against this thread and is still
    // awaiting a reply, this inbound IS the reply — fulfil the
    // waiter and skip the normal forward to the gateway. Without
    // the early return the agent would see the same answer twice
    // (once as the tool result, once as a new user turn).
    const askKey = `${botId}|${msg.chatId}|${platformUserId}`;
    const waiter = this.pendingQuestions.get(askKey);
    if (waiter) {
      this.pendingQuestions.delete(askKey);
      const text = cleanInboundContent(msg.content, msg.mentions) ?? "";
      waiter.resolve(text);
      return;
    }

    // Sequentially download up to MAX_ATTACHMENTS_PER_MESSAGE
    // resources. Concurrent downloads were attractive but each helper
    // buffers the full payload into memory; a malicious / runaway
    // user could send many large files and force the shared sidecar
    // process to allocate them all at once. Sequential keeps peak
    // RSS at one resource. The count cap matches Lark's UI multi-
    // upload limit (9) so a typical batch still flows through.
    const accepted = msg.resources.slice(0, MAX_ATTACHMENTS_PER_MESSAGE);
    const dropped = msg.resources.length - accepted.length;
    if (dropped > 0) {
      this.logger.warn(
        `lark inbound message=${platformMsgId} carried ${msg.resources.length} resources; dropping ${dropped} over the per-message cap of ${MAX_ATTACHMENTS_PER_MESSAGE}`,
      );
    }
    let pairingRequired = false;
    // Hold the slot for the whole download phase so concurrent
    // inbounds can't compound their per-resource caps into an
    // unbounded peak. Skip the gate entirely when there's nothing to
    // download — text-only messages must not queue behind a
    // media-heavy peer's downloads.
    const attachments: WireAttachment[] =
      accepted.length === 0
        ? []
        : await downloadSlots.withPermit(async () => {
            const out: WireAttachment[] = [];
            for (const resource of accepted) {
              try {
                const att = await downloadResourceAsAttachment({
                  channel,
                  resource,
                  botId,
                  userId: auraUserId,
                  logger: this.logger,
                });
                if (att) out.push(att);
              } catch (err) {
                if (!(err instanceof BlobPairingRequiredError)) throw err;
                pairingRequired = true;
                break;
              }
            }
            return out;
          });

    const content = cleanInboundContent(msg.content, msg.mentions);
    if (!content && attachments.length === 0 && !pairingRequired) return;

    if (config.reactionEcho) {
      // Best-effort acknowledgement; failures are debug-level. We do
      // NOT await — reaction round-trips can take 1–2s and we don't
      // want to delay the agent's first delta on an Asia-region link.
      void channel.addReaction(platformMsgId, "OK").catch((err: unknown) => {
        this.logger.debug(
          `lark inbound reaction-echo failed message=${platformMsgId}: ${String(err)}`,
        );
      });
    }

    emit({
      chat: address,
      platformUserId,
      content,
      platformMsgId,
      ...(attachments.length > 0 ? { attachments } : {}),
    });
  }
}

// Lark's chat UI lets users attach up to 9 images per send; pick the
// same number so the common case isn't truncated, and reject anything
// past it as a defense against memory-exhaustion attacks (each
// resource is fully buffered before upload).
const MAX_ATTACHMENTS_PER_MESSAGE = 9;
