import type { Logger } from "@aura/channel-sdk";
import type * as lark from "@larksuiteoapi/node-sdk";

import {
  buildAuthFailedCard,
  buildAuthPendingCard,
  buildAuthorizedCard,
  type AuthFailureKind,
} from "./auth-cards.js";
import {
  fetchUATSubject,
  pollDeviceToken,
  requestDeviceAuthorization,
} from "./device-flow.js";
import { joinScopes } from "./scopes.js";
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
  /** OAuth scopes the underlying tool needs — passed verbatim into
   * the device-flow scope parameter. The accessor's `UATInvokeRequest.scopes`
   * forwards here, so the OAuth grant always asks for what the tool
   * actually needs (Codex review #2). Empty array means "just
   * `offline_access`", which works only for tools like
   * `feishu_who_am_i` that hit `/authen/v1/user_info`. */
  scopes: readonly string[];
}

/** Mask an open_id for log/error surfaces — Lark `ou_` ids look
 * uniform enough that a leaked one in an LLM-readable error can
 * make follow-up spear-phishing easier. Keep just enough to let an
 * operator cross-check against the audit trail. */
function maskOpenId(id: string): string {
  if (id.length <= 8) return id;
  return `${id.slice(0, 4)}…${id.slice(-3)}`;
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
      // Subject check (Codex review #1): Lark issues the UAT to
      // whoever clicked the auth link, NOT necessarily the user we
      // asked. In a group chat any member who can see the card can
      // beat the original requester to the click — without this
      // check we'd silently store the wrong person's UAT under
      // `req.userOpenId` and every subsequent tool call would
      // operate on the impersonator's Feishu data. Verify the token
      // actually belongs to who we expected before persisting.
      const subjectResult = await fetchUATSubject({
        accessToken: poll.token.accessToken,
        baseUrl: this.opts.baseUrl,
        logger: this.opts.logger,
        ...(this.opts.fetchImpl !== undefined && {
          fetchImpl: this.opts.fetchImpl,
        }),
      });
      if (!subjectResult.ok) {
        const detail = `subject verification failed: ${subjectResult.reason}`;
        void this.terminalCard(
          cardMessageId,
          buildAuthFailedCard({ kind: "error", detail }),
        );
        return { kind: "error", message: detail };
      }
      if (subjectResult.subject.openId !== req.userOpenId) {
        const detail = `the authorized user (${maskOpenId(subjectResult.subject.openId)}) does not match the requesting user (${maskOpenId(req.userOpenId)}); the auth link was clicked by someone else in the chat — restart the flow yourself`;
        this.opts.logger.warn(
          `lark auth subject mismatch: expected=${req.userOpenId} got=${subjectResult.subject.openId}`,
        );
        void this.terminalCard(
          cardMessageId,
          buildAuthFailedCard({ kind: "error", detail }),
        );
        return { kind: "error", message: detail };
      }
      const stored = await this.opts.store.storeGrant(
        req.userOpenId,
        poll.token,
      );
      void this.terminalCard(
        cardMessageId,
        buildAuthorizedCard(
          subjectResult.subject.name !== undefined
            ? { userName: subjectResult.subject.name }
            : {},
        ),
      );
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
    const scope = joinScopes(req.scopes ?? []);
    return requestDeviceAuthorization({
      appId: this.opts.appId,
      appSecret: this.opts.appSecret,
      baseUrl: this.opts.baseUrl,
      logger: this.opts.logger,
      ...(this.opts.fetchImpl !== undefined && {
        fetchImpl: this.opts.fetchImpl,
      }),
      ...(scope.length > 0 && { scope }),
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
