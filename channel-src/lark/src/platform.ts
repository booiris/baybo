import type {
  AgentNotice,
  Logger,
  StartBotCommand,
  WireAttachment,
} from "@aura/channel-sdk";
import { composeAuraUserId } from "@aura/channel-sdk/bot";
import type {
  BotInboundEvent,
  BotMediaPayload,
  BotPlatform,
  BotStartHooks,
} from "@aura/channel-sdk/bot";
import * as lark from "@larksuiteoapi/node-sdk";

import type { LarkApprovals } from "./approvals.js";
import { parseStartBotCredentials } from "./auth/credentials.js";
import { downloadResourceAsAttachment } from "./media/inbound.js";
import { sendLarkAttachments } from "./media/outbound.js";

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

export class LarkPlatform implements BotPlatform<lark.LarkChannel, LarkChat> {
  constructor(
    private readonly logger: Logger,
    private readonly approvals: LarkApprovals,
  ) {}

  async sendText(
    handle: lark.LarkChannel,
    chat: LarkChat,
    text: string,
  ): Promise<void> {
    await handle.send(chat.chatId, { text });
  }

  async sendNotice(
    handle: lark.LarkChannel,
    chat: LarkChat,
    notice: AgentNotice,
  ): Promise<void> {
    const prefix = notice.level === "error" ? "❌" : "⚠️";
    await this.sendText(handle, chat, `${prefix} ${notice.text}`);
  }

  async stopBot(handle: lark.LarkChannel): Promise<void> {
    try {
      await handle.disconnect();
    } catch (err) {
      this.logger.debug(`lark disconnect raised; ignoring: ${String(err)}`);
    }
  }

  async sendMedia(
    handle: lark.LarkChannel,
    chat: LarkChat,
    payload: BotMediaPayload,
  ): Promise<void> {
    await sendLarkAttachments({
      channel: handle,
      chat,
      payload,
      logger: this.logger,
    });
  }

  async startBot(
    cmd: StartBotCommand,
    hooks: BotStartHooks<lark.LarkChannel, LarkChat>,
  ): Promise<{
    handle: lark.LarkChannel;
    username?: string;
  }> {
    const creds = parseStartBotCredentials(cmd);
    const channel = lark.createLarkChannel({
      appId: creds.appId,
      appSecret: creds.appSecret,
      transport: "websocket",
      domain: creds.domain,
      // The SDK's safety pipeline already covers what openclaw's
      // `inbound/dedup.ts` does (replay filter, stale-message cutoff)
      // and what `policy.ts` does (group/DM/mention-required gates).
      // Phase 2 surfaces these knobs to operator config; for the MVP
      // the SDK defaults plus mention-required-in-groups are fine.
      policy: { requireMention: true, dmMode: "open" },
    });

    channel.on("message", (msg) =>
      this.dispatchInbound(channel, cmd.botId, msg, hooks.emit),
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

    return username ? { handle: channel, username } : { handle: channel };
  }

  private async dispatchInbound(
    channel: lark.LarkChannel,
    botId: string,
    msg: lark.NormalizedMessage,
    emit: (ev: BotInboundEvent<LarkChat>) => void,
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
    const attachments: WireAttachment[] = [];
    for (const resource of accepted) {
      const att = await downloadResourceAsAttachment({
        channel,
        resource,
        botId,
        userId: auraUserId,
        logger: this.logger,
      });
      if (att) attachments.push(att);
    }

    const content = msg.content;
    if (!content && attachments.length === 0) return;

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
