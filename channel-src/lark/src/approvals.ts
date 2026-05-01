import type {
  ApprovalDecision,
  ApprovalRequest,
  Logger,
} from "@aura/channel-sdk";
import type { BotRoute, PlatformApprovals } from "@aura/channel-sdk/bot";
import { composeAuraUserId } from "@aura/channel-sdk/bot";
import type * as lark from "@larksuiteoapi/node-sdk";

import {
  buildApprovalCard,
  buildResolvedApprovalCard,
  parseApprovalCardValue,
} from "./messaging/cards.js";
import { CHANNEL_TYPE, type LarkChat } from "./platform.js";

interface PendingEntry {
  botId: string;
  handle: lark.LarkChannel;
  request: ApprovalRequest;
  cardMessageId: string;
  triggererOpenId: string;
  resolve: (decision: ApprovalDecision) => void;
}

/**
 * Lark approvals broker.
 *
 * For each `Frame::ApprovalRequested` the broker posts a CardKit v1
 * card with three buttons. Users tap a button → Lark fires
 * `card.action.trigger` → the SDK's `cardAction` event lands on
 * {@link handleCardAction}, which:
 *
 * - Looks up the pending entry by `call_id`,
 * - Verifies the operator is the user who triggered the approval —
 *   rejects from anyone else with a toast (security: a group-chat
 *   approval card is clickable by every group member, so without this
 *   filter user B could approve user A's bash call),
 * - Resolves the SDK promise with the chosen decision,
 * - Edits the card to its terminal state on `onResolved`.
 *
 * The Lark SDK doesn't ship a per-bot middleware tree like grammy, so
 * `attach(handle)` is a no-op — handler subscription happens in
 * `LarkPlatform.startBot` via `channel.on("cardAction", …)`. The
 * handler routes through this class so the pending state stays
 * private to the approvals broker.
 */
export class LarkApprovals
  implements PlatformApprovals<lark.LarkChannel, LarkChat>
{
  private readonly pending = new Map<string, PendingEntry>();

  constructor(private readonly logger: Logger) {}

  async onRequested(
    req: ApprovalRequest,
    route: BotRoute<lark.LarkChannel, LarkChat> | null,
  ): Promise<ApprovalDecision> {
    if (!route) {
      this.logger.warn(
        `lark approval ${req.callId}: no routable chat for ${req.userId}; auto-deny`,
      );
      return "deny";
    }

    const triggererOpenId = extractPlatformUserId(req.userId, route);
    if (!triggererOpenId) {
      this.logger.warn(
        `lark approval ${req.callId}: cannot decompose userId='${req.userId}'; auto-deny`,
      );
      return "deny";
    }

    const card = buildApprovalCard(req);
    let cardMessageId: string;
    try {
      const result = await route.handle.send(route.chat.chatId, { card });
      cardMessageId = result.messageId;
    } catch (err) {
      this.logger.error(
        `lark approval ${req.callId}: card send failed; auto-deny: ${String(err)}`,
      );
      return "deny";
    }

    return new Promise<ApprovalDecision>((resolve) => {
      this.pending.set(req.callId, {
        botId: route.botId,
        handle: route.handle,
        request: req,
        cardMessageId,
        triggererOpenId,
        resolve,
      });
    });
  }

  onBotStopped(botId: string): void {
    for (const [callId, entry] of this.pending) {
      if (entry.botId !== botId) continue;
      this.pending.delete(callId);
      entry.resolve("deny");
    }
  }

  async onResolved(
    callId: string,
    decision: ApprovalDecision,
  ): Promise<void> {
    const entry = this.pending.get(callId);
    if (!entry) return;
    this.pending.delete(callId);
    entry.resolve(decision);
    try {
      await entry.handle.updateCard(
        entry.cardMessageId,
        buildResolvedApprovalCard(entry.request, decision),
      );
    } catch (err) {
      this.logger.debug(
        `lark approval ${callId}: card edit on resolve failed: ${String(err)}`,
      );
    }
  }

  onStop(): void {
    for (const [, entry] of this.pending) {
      entry.resolve("deny");
    }
    this.pending.clear();
  }

  /**
   * Routed from `LarkPlatform`'s `channel.on("cardAction", …)`
   * subscription. The Lark SDK's `LarkChannel.cardAction` handler
   * surface discards the return value (the dispatcher's lower-level
   * toast hook isn't reachable without bypassing `LarkChannel`), so
   * unauthorized taps are silently dropped — the card stays open until
   * the triggerer resolves it. Phase 2 should expose the dispatcher to
   * surface a "you can't approve this" toast.
   */
  handleCardAction(ev: lark.CardActionEvent): void {
    const value = parseApprovalCardValue(ev.action.value);
    if (!value) return;
    const entry = this.pending.get(value.callId);
    if (!entry) return;

    if (ev.operator.openId !== entry.triggererOpenId) {
      this.logger.debug(
        `lark approval ${value.callId}: tap from ${ev.operator.openId} ignored — only ${entry.triggererOpenId} may resolve`,
      );
      return;
    }

    this.pending.delete(value.callId);
    entry.resolve(value.decision);
  }
}

/**
 * Recover the Lark `open_id` suffix from the composite aura user id
 * `<channelType>_<botId>_<chatKey>_<openId>`. Naive `lastIndexOf("_")`
 * is unsafe here because Feishu open_id / chat_id values themselves
 * contain underscores (`oc_…`, `om_…`, `ou_…`) and the chatKey is a
 * `key=value&…` blob that can include them too. Recompose the prefix
 * deterministically from the route we already have and strip it.
 */
function extractPlatformUserId(
  auraUserId: string,
  route: BotRoute<lark.LarkChannel, LarkChat>,
): string | null {
  const prefix = composeAuraUserId(CHANNEL_TYPE, route.botId, route.chat, "");
  if (!auraUserId.startsWith(prefix)) return null;
  const tail = auraUserId.slice(prefix.length);
  return tail.length > 0 ? tail : null;
}

