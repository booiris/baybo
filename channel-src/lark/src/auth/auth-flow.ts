import type { Logger } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

import {
  buildAuthFailedCard,
  buildAuthPendingCard,
  buildAuthorizedCard,
  type AuthFailureKind,
} from "./auth-cards.js";
import {
  pollDeviceToken,
  requestDeviceAuthorization,
} from "./device-flow.js";
import type { StoredUAToken, UATStore } from "./uat-store.js";

export type AuthFlowOutcome =
  | { kind: "ok"; uat: StoredUAToken }
  | { kind: "denied" }
  | { kind: "expired" }
  | { kind: "cancelled" }
  | { kind: "error"; message: string };

export interface AuthFlowOptions {
  channel: AuthFlowChannel;
  store: UATStore;
  appId: string;
  appSecret: string;
  baseUrl: string;
  logger: Logger;
  /** Override fetch for tests. */
  fetchImpl?: typeof fetch;
}

/** The slice of `lark.LarkChannel` the controller actually uses.
 *
 * Narrowed so tests can inject an in-memory channel without standing
 * up the entire Lark SDK. */
export interface AuthFlowChannel {
  send: lark.LarkChannel["send"];
  updateCard: lark.LarkChannel["updateCard"];
}

export interface AuthRequest {
  userOpenId: string;
  chatId: string;
  /** Human-friendly description of what the agent was trying to do.
   * Becomes the "Aura needs your authorization to ${reason}" line in
   * the pending card. Keep it short — "read your Feishu calendar",
   * "search your Bitable apps", etc. */
  reason: string;
  /** OAuth scope override. Defaults to none ⇒ the device-flow client
   * appends just `offline_access`, which is enough for the basic
   * `/authen/v1/user_info` lookup but not for any per-API scope. The
   * caller (typically the auto-auth interceptor) is expected to pass
   * the scope the failing tool needs. */
  scope?: string;
}

interface InflightFlow {
  promise: Promise<AuthFlowOutcome>;
  controller: AbortController;
}

/** Per-bot orchestrator for the OAuth UAT device flow.
 *
 * Responsibilities:
 *   1. Run the RFC 8628 client (device_authorization → poll token).
 *   2. Render the user-facing card and edit it through pending →
 *      authorized / denied / expired / error.
 *   3. Persist the resulting UAT into the store.
 *   4. Dedup concurrent requests for the same `userOpenId` — if two
 *      tool calls trigger auth at once, both await the same flow
 *      instead of sending duplicate cards (Codex-style hardening:
 *      stops a paired user from spamming auth cards by repeated tool
 *      taps). */
export class AuthFlowController {
  private readonly inflight = new Map<string, InflightFlow>();

  constructor(private readonly opts: AuthFlowOptions) {}

  /** Start (or join) an auth flow for `req.userOpenId`. */
  async requestUAT(req: AuthRequest): Promise<AuthFlowOutcome> {
    const existing = this.inflight.get(req.userOpenId);
    if (existing) return existing.promise;

    const controller = new AbortController();
    const promise = this.runFlow(req, controller.signal).finally(() => {
      const cur = this.inflight.get(req.userOpenId);
      if (cur && cur.promise === promise) this.inflight.delete(req.userOpenId);
    });
    this.inflight.set(req.userOpenId, { promise, controller });
    return promise;
  }

  /** Cancel every in-flight flow. Called on bot stop / sidecar
   * shutdown so we don't leak polling timers across the process
   * lifetime. */
  cancelAll(): void {
    for (const flow of this.inflight.values()) flow.controller.abort();
  }

  /** Whether a flow for `userOpenId` is currently in flight. */
  isInflight(userOpenId: string): boolean {
    return this.inflight.has(userOpenId);
  }

  private async runFlow(
    req: AuthRequest,
    signal: AbortSignal,
  ): Promise<AuthFlowOutcome> {
    if (signal.aborted) return { kind: "cancelled" };

    const auth = await this.requestDeviceCode(req).catch((err) => err);
    if (auth instanceof Error) {
      return { kind: "error", message: auth.message };
    }

    const cardMessageId = await this.sendPendingCard(req, auth).catch(
      (err) => err,
    );
    if (cardMessageId instanceof Error) {
      return {
        kind: "error",
        message: `failed to send auth card: ${cardMessageId.message}`,
      };
    }

    const poll = await pollDeviceToken({
      appId: this.opts.appId,
      appSecret: this.opts.appSecret,
      baseUrl: this.opts.baseUrl,
      logger: this.opts.logger,
      ...(this.opts.fetchImpl !== undefined && {
        fetchImpl: this.opts.fetchImpl,
      }),
      deviceCode: auth.deviceCode,
      interval: auth.interval,
      expiresIn: auth.expiresIn,
      signal,
    });

    if (poll.ok) {
      const stored = await this.opts.store.storeGrant(
        req.userOpenId,
        poll.token,
      );
      void this.terminalCard(cardMessageId, buildAuthorizedCard({}));
      return { kind: "ok", uat: stored };
    }

    // Aborted polling (signal fired) returns expired_token from the
    // device-flow client; differentiate so the caller can distinguish
    // "user didn't respond in 4 minutes" from "we cancelled because
    // the bot is stopping".
    if (signal.aborted) {
      void this.terminalCard(
        cardMessageId,
        buildAuthFailedCard({ kind: "error", detail: "cancelled" }),
      );
      return { kind: "cancelled" };
    }

    let cardKind: AuthFailureKind;
    let outcome: AuthFlowOutcome;
    if (poll.error === "access_denied") {
      cardKind = "denied";
      outcome = { kind: "denied" };
    } else if (poll.error === "expired_token") {
      cardKind = "expired";
      outcome = { kind: "expired" };
    } else {
      cardKind = "error";
      outcome = { kind: "error", message: poll.message };
    }
    void this.terminalCard(
      cardMessageId,
      buildAuthFailedCard({
        kind: cardKind,
        ...(cardKind === "error" && { detail: poll.message }),
      }),
    );
    return outcome;
  }

  private async requestDeviceCode(
    req: AuthRequest,
  ): ReturnType<typeof requestDeviceAuthorization> {
    return requestDeviceAuthorization({
      appId: this.opts.appId,
      appSecret: this.opts.appSecret,
      baseUrl: this.opts.baseUrl,
      logger: this.opts.logger,
      ...(this.opts.fetchImpl !== undefined && {
        fetchImpl: this.opts.fetchImpl,
      }),
      ...(req.scope !== undefined && { scope: req.scope }),
    });
  }

  private async sendPendingCard(
    req: AuthRequest,
    auth: Awaited<ReturnType<typeof requestDeviceAuthorization>>,
  ): Promise<string> {
    const card = buildAuthPendingCard({
      reason: req.reason,
      verificationUriComplete: auth.verificationUriComplete,
      verificationUri: auth.verificationUri,
      userCode: auth.userCode,
      expiresAtMs: Date.now() + auth.expiresIn * 1000,
    });
    const result = await this.opts.channel.send(req.chatId, { card });
    return result.messageId;
  }

  private async terminalCard(
    cardMessageId: string,
    card: object,
  ): Promise<void> {
    try {
      await this.opts.channel.updateCard(cardMessageId, card);
    } catch (err) {
      // Terminal-card failures are best-effort — the user still has
      // the original pending card; not blocking on this keeps the
      // outcome promise honest.
      this.opts.logger.debug(
        `lark auth card terminal update failed: ${String(err)}`,
      );
    }
  }
}
