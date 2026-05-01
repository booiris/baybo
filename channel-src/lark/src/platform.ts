import type {
  AgentNotice,
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
  // Per-userId streaming session: at most one card streams to a given
  // (channelType, botId, chatKey, platformUserId) tuple at a time.
  // Aura's gateway already serialises a session's outbound, so the
  // map collisions only happen across distinct sessions in the same
  // chat — those collisions are correct (interleaving cards would be
  // worse).
  private readonly streams = new Map<string, LarkStreamingSession>();

  constructor(
    private readonly logger: Logger,
    private readonly approvals: LarkApprovals,
  ) {}

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
