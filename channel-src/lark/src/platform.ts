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

import { secrets } from "@aura/channel-sdk";

import type { LarkApprovals } from "./approvals.js";
import { AuthFlowController } from "./auth/auth-flow.js";
import { UATAccessor, UATMutex } from "./auth/auto-auth.js";
import {
  parseBotRuntimeConfig,
  parseStartBotCredentials,
  type BotRuntimeConfig,
} from "./auth/credentials.js";
import { UATRefreshScheduler } from "./auth/refresh-scheduler.js";
import { UATStore } from "./auth/uat-store.js";
import {
  buildResolvedWriteApprovalCard,
  buildWriteApprovalCard,
  parseWriteApprovalCardValue,
  type WriteApprovalCardOpts,
  type WriteApprovalDecision,
} from "./messaging/write-approval.js";
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
  /** UAT pipeline. Created at startBot — see {@link buildUatPipeline}.
   * `null` when the SDK didn't negotiate the `secrets` capability,
   * which makes UAT-requiring tools fail closed with a clean
   * configuration error rather than throwing at the call site. */
  uat: UatPipeline | null;
}

interface UatPipeline {
  accessor: UATAccessor;
  scheduler: UATRefreshScheduler;
  authFlow: AuthFlowController;
  /** Direct handle to the UAT store. Held alongside the scheduler so
   * the diagnose hook (and any future operator-facing surface) can
   * inspect the stored grants without reaching into private fields
   * on the scheduler / accessor. */
  store: UATStore;
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
  // Per-call write-approval waiters keyed by `call_id` (UUID minted
  // per `approveWrite`). Card actions tagged `aura: "mcp_write"` route
  // here from `handleCardAction`. Only the originating user's clicks
  // count — cross-user taps in a group chat are silently ignored, so
  // a coworker can't approve someone else's destructive write.
  private readonly pendingApprovals = new Map<
    string,
    {
      botId: string;
      triggererOpenId: string;
      cardMessageId: string;
      handle: lark.LarkChannel;
      request: WriteApprovalCardOpts;
      resolve: (decision: WriteApprovalDecision) => void;
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
      // Resolve the bot AND the chat the agent is replying in. Per
      // Codex review #2, chat-bound MCP tools must NOT accept an
      // arbitrary chat id from LLM-supplied args — that lets a
      // paired user probe metadata for any chat the bot can see. We
      // bind to the active conversation by reading
      // `contextByAuraUser` (populated from each inbound dispatch)
      // and surfacing chatId + platformUserId in the `ok` variant.
      //
      // Resolution order:
      //   1. With `auraUserId` cached → use that conversation's
      //      bot+chat+user. If `auraBotId` was also supplied, sanity
      //      check it matches; mismatch returns `none` (caller
      //      crossed wires).
      //   2. Without conversation context → can't safely run a
      //      chat-bound tool. Return `none` (no active Lark chat
      //      for this session).
      //   3. The slice 2A `ambiguous` fallback only kicks in when an
      //      explicit `auraBotId` resolves to a multi-bot ambiguity
      //      AND there's no chat context. Today this branch is
      //      unreachable from any registered tool, but it's kept
      //      so `LarkChannelResolution` stays a stable surface for
      //      future bot-only tools.
      channelResolver: ({ auraBotId, auraUserId }) => {
        if (auraUserId) {
          const ctx = this.contextByAuraUser.get(auraUserId);
          if (!ctx) return { kind: "none" };
          if (auraBotId && auraBotId !== ctx.botId) return { kind: "none" };
          const state = this.bots.get(ctx.botId);
          if (!state) return { kind: "none" };
          return {
            kind: "ok",
            channel: state.handle,
            chatId: ctx.chatId,
            platformUserId: ctx.platformUserId,
            ...(state.uat !== null && { uatAccessor: state.uat.accessor }),
          };
        }
        // No conversation context. Without a chat id we can't run a
        // chat-bound tool. The bot-count classification is kept for
        // any future tool that operates without a chat.
        const handles = [...this.bots.values()].map((s) => s.handle);
        if (handles.length === 0) return { kind: "none" };
        if (handles.length === 1) return { kind: "none" };
        return { kind: "ambiguous", bot_count: handles.length };
      },
      askUser: (input, prompt, timeoutMs) =>
        this.askUser(input, prompt, timeoutMs),
      approveWrite: (input, request, timeoutMs) =>
        this.approveWrite(input, request, timeoutMs),
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

  /** Send an interactive [Approve] [Deny] card into the active
   * conversation and wait up to `timeoutMs` for the originating
   * user's click. Per-call (not per-session) so the user can refuse
   * specific destructive ops even after granting OAuth.
   *
   * Only the user who started the conversation can resolve the card
   * — taps from other users in a group chat are silently ignored
   * (mirrors `LarkApprovals` operator-filter for bash approvals,
   * stops a coworker from rubber-stamping someone else's writes). */
  async approveWrite(
    input: { auraUserId?: string; auraBotId?: string },
    request: {
      toolName: string;
      summary: string;
      detail?: string;
    },
    timeoutMs: number,
  ): Promise<
    | { kind: "approved" }
    | { kind: "denied" }
    | { kind: "timeout" }
    | { kind: "no_context" }
  > {
    if (!input.auraUserId) return { kind: "no_context" };
    const ctx = this.contextByAuraUser.get(input.auraUserId);
    if (!ctx) return { kind: "no_context" };
    if (input.auraBotId && input.auraBotId !== ctx.botId) {
      return { kind: "no_context" };
    }
    const state = this.bots.get(ctx.botId);
    if (!state) return { kind: "no_context" };

    const callId = `mcpw_${Date.now()}_${Math.random().toString(36).slice(2, 10)}`;
    const cardOpts: WriteApprovalCardOpts = {
      callId,
      toolName: request.toolName,
      summary: request.summary,
      ...(request.detail !== undefined && { detail: request.detail }),
    };
    let cardMessageId: string;
    try {
      const result = await state.handle.send(ctx.chatId, {
        card: buildWriteApprovalCard(cardOpts),
      });
      cardMessageId = result.messageId;
    } catch (err) {
      this.logger.warn(
        `lark approveWrite: card send failed bot=${ctx.botId} chat=${ctx.chatId} tool=${request.toolName}: ${String(err)}`,
      );
      return { kind: "timeout" };
    }

    return await new Promise((resolve) => {
      const timer = setTimeout(() => {
        const entry = this.pendingApprovals.get(callId);
        if (!entry) return;
        this.pendingApprovals.delete(callId);
        // Best-effort terminal edit so the card doesn't dangle as a
        // pending prompt forever — mark it as expired.
        void state.handle
          .updateCard(
            entry.cardMessageId,
            buildResolvedWriteApprovalCard(entry.request, "deny"),
          )
          .catch(() => undefined);
        resolve({ kind: "timeout" });
      }, timeoutMs);

      this.pendingApprovals.set(callId, {
        botId: ctx.botId,
        triggererOpenId: ctx.platformUserId,
        cardMessageId,
        handle: state.handle,
        request: cardOpts,
        resolve: (decision) =>
          resolve(decision === "approve" ? { kind: "approved" } : { kind: "denied" }),
        timer,
      });
    });
  }

  /** Try to resolve `ev` as an MCP write-approval click. Returns
   * true if the event was an MCP-write tap (so the caller skips the
   * bash-approval pipeline) — false otherwise.
   *
   * Only the originating user's clicks count; cross-user taps in a
   * group chat are ignored so a coworker can't approve someone
   * else's destructive write.
   *
   * Tagged `aura: "mcp_write"` ≠ bash's `aura: "approval"`, so the
   * bash-approval handler can stay narrow without being aware of
   * MCP writes. */
  private dispatchWriteApproval(ev: lark.CardActionEvent): boolean {
    const value = parseWriteApprovalCardValue(ev.action.value);
    if (!value) return false;
    const entry = this.pendingApprovals.get(value.callId);
    if (!entry) {
      // Card already resolved or expired. Surface as handled so the
      // bash-approval handler doesn't try to route it.
      return true;
    }
    if (ev.operator.openId !== entry.triggererOpenId) {
      this.logger.debug(
        `lark approveWrite: tap from ${ev.operator.openId} ignored — only ${entry.triggererOpenId} may resolve call=${value.callId}`,
      );
      return true;
    }
    clearTimeout(entry.timer);
    this.pendingApprovals.delete(value.callId);
    entry.resolve(value.decision);
    void entry.handle
      .updateCard(
        entry.cardMessageId,
        buildResolvedWriteApprovalCard(entry.request, value.decision),
      )
      .catch((err) => {
        this.logger.debug(
          `lark approveWrite: terminal card update failed call=${value.callId}: ${String(err)}`,
        );
      });
    return true;
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

    // UAT pipeline status: how many users have authorized, plus a
    // probe that the secrets capability is actually wired (see
    // helper). Cheap — just inspects in-memory state + counts
    // entries in the secret store.
    checks.push(await this.summariseUatPipeline(state));

    // MCP tool surface: count of registered tools so an operator
    // can confirm the sidecar started clean (a tool registration
    // throw would surface as a smaller-than-expected count).
    checks.push({
      name: "mcp_tools",
      status: "ok",
      detail: `${this.mcpServer.toolNames().length} MCP tools registered`,
    });

    // OAuth endpoint reachability — separate from the WS host,
    // catches DNS / TCP issues that `transport` (which is WS to
    // open.X) wouldn't surface for the device-flow path
    // (accounts.X / open.X HTTP).
    checks.push(await this.probeOAuthEndpoint(state));

    return checks;
  }

  private async probeOAuthEndpoint(state: BotState): Promise<DiagnoseCheck> {
    const PROBE_TIMEOUT_MS = 5_000;
    if (!state.uat) {
      // No UAT pipeline = no OAuth host configured for this bot.
      // Use `warn` since skipping is informational, not a hard
      // error — the SDK's DiagnoseStatus enum doesn't have a
      // dedicated `skipped` variant.
      return {
        name: "oauth_endpoint",
        status: "warn",
        detail: "OAuth pipeline disabled — skipping endpoint reachability probe",
      };
    }
    const baseUrl = state.uat.accessor.baseUrl;
    const url = `${baseUrl.replace(/\/+$/, "")}/open-apis/authen/v1/user_info`;
    try {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), PROBE_TIMEOUT_MS);
      try {
        // Hitting the OAuth endpoint without a UAT returns 401 / a
        // structured error — that's still a successful round-trip
        // (the host is reachable + responding); we just want to know
        // we COULD talk to it. A connection error / DNS failure
        // would throw.
        const resp = await fetch(url, {
          headers: { Authorization: "Bearer probe" },
          signal: ctrl.signal,
        });
        return {
          name: "oauth_endpoint",
          status: "ok",
          detail: `${baseUrl} reachable (HTTP ${resp.status})`,
        };
      } finally {
        clearTimeout(timer);
      }
    } catch (err) {
      return {
        name: "oauth_endpoint",
        status: "error",
        detail: `failed to reach ${baseUrl}: ${String(err)}`,
      };
    }
  }

  private async summariseUatPipeline(state: BotState): Promise<DiagnoseCheck> {
    if (!state.uat) {
      return {
        name: "uat_pipeline",
        status: "warn",
        detail: "OAuth pipeline disabled (gateway didn't negotiate `secrets` capability)",
      };
    }
    try {
      // store.listUsers requires the `secrets` capability; if the
      // gateway negotiated it but the bot row doesn't exist server-
      // side, this throws SecretBotUnknownError — surface as error.
      const users = await state.uat.store.listUsers();
      return {
        name: "uat_pipeline",
        status: "ok",
        detail: `${users.length} authorized user${users.length === 1 ? "" : "s"} stored, refresh scheduler running`,
      };
    } catch (err) {
      return {
        name: "uat_pipeline",
        status: "error",
        detail: `UAT store query failed: ${String(err)}`,
      };
    }
  }

  async stopBot(handle: lark.LarkChannel): Promise<void> {
    const botId = this.botIdFromHandle(handle);
    if (botId !== null) {
      const state = this.bots.get(botId);
      if (state?.uat) {
        // Stop the refresh sweep timer + abort any in-flight device
        // flow so the sidecar can exit cleanly.
        await state.uat.scheduler.stop();
        state.uat.authFlow.cancelAll();
      }
      // Resolve any pending write-approval cards for this bot as
      // `denied` so the in-flight tool calls unwind cleanly instead
      // of hanging on a timer that survives bot teardown.
      for (const [callId, entry] of this.pendingApprovals) {
        if (entry.botId !== botId) continue;
        clearTimeout(entry.timer);
        this.pendingApprovals.delete(callId);
        entry.resolve("deny");
      }
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
    channel.on("cardAction", (ev) => {
      // MCP write-approval cards carry `aura: "mcp_write"`; route those
      // here. Bash approval cards carry `aura: "approval"` and continue
      // to the existing approvals pipeline. Other action shapes fall
      // through (silent — Lark dispatches every cardAction event to
      // every handler).
      if (this.dispatchWriteApproval(ev)) return;
      this.approvals.handleCardAction(ev);
    });
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
    const uat = this.buildUatPipeline(channel, creds.appId, creds.appSecret, creds.baseUrl);
    if (uat !== null) uat.scheduler.start();
    this.bots.set(cmd.botId, { handle: channel, config, uat });

    return username ? { handle: channel, username } : { handle: channel };
  }

  /** Build the UAT pipeline for one bot (store + scheduler + auth flow
   * + auto-auth interceptor + per-user mutex). The SDK's `secrets()`
   * factory doesn't probe the capability at construction — the error
   * surfaces only on the first WS-mediated op — so this can't usefully
   * fail-fast here. If the gateway didn't negotiate `secrets`, the
   * accessor's first invoke will throw `CapabilityMissingError`, which
   * the MCP tool wrapper translates to a clean tool error. */
  private buildUatPipeline(
    channel: lark.LarkChannel,
    appId: string,
    appSecret: string,
    baseUrl: string,
  ): UatPipeline | null {
    const scope = secrets().scope(appId);
    const store = new UATStore(scope);
    const mutex = new UATMutex();
    const authFlow = new AuthFlowController({
      channel,
      store,
      appId,
      appSecret,
      baseUrl,
      logger: this.logger,
    });
    const accessor = new UATAccessor({
      store,
      authFlow,
      mutex,
      appId,
      appSecret,
      baseUrl,
      logger: this.logger,
    });
    const scheduler = new UATRefreshScheduler({
      store,
      mutex,
      appId,
      appSecret,
      baseUrl,
      logger: this.logger,
    });
    return { accessor, authFlow, scheduler, store };
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

