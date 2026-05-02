import type { Logger } from "@aura/channel-sdk";

import type { AuthFlowController, AuthFlowOutcome } from "./auth-flow.js";
import { refreshUAT } from "./device-flow.js";
import type { StoredUAToken, UATStore } from "./uat-store.js";

/** Lark API error codes that mean "your user_access_token is no longer
 * usable, get a new one." Caught by the interceptor below to trigger
 * a single retry through the auth flow.
 *
 * - `99991663` — UAT missing / malformed
 * - `99991664` — UAT expired
 * - `99991668` — refresh_token expired
 * - `99991669` — refresh_token invalid */
export const UAT_DEAD_CODES = new Set<number>([
  99991663, 99991664, 99991668, 99991669,
]);

/** Refresh-ahead margin: if a stored UAT will expire within this many
 * ms, the interceptor refreshes proactively before calling the API.
 * Wider than the scheduler's window (5min) so an interceptor that
 * runs between sweeps still hits a fresh token. */
const PROACTIVE_REFRESH_MS = 60_000;

/** Per-user mutex shared between the refresh scheduler and the
 * auto-auth interceptor.
 *
 * OAuth 2.1 mandates one-time-use refresh tokens — if both the
 * scheduler and an on-demand interceptor try to refresh at the same
 * time, one of the two attempts will fail with `invalid_grant` and
 * (worse) leave the failing party convinced the UAT is dead. The
 * mutex serialises every operation that mutates a user's UAT entry
 * so a refresh always sees the most recent token. */
export class UATMutex {
  private readonly locks = new Map<string, Promise<void>>();

  /** Run `fn` with exclusive access to `userOpenId`'s UAT row. */
  async withLock<T>(userOpenId: string, fn: () => Promise<T>): Promise<T> {
    const prev = this.locks.get(userOpenId) ?? Promise.resolve();
    let release: () => void = () => undefined;
    const next = new Promise<void>((r) => {
      release = r;
    });
    this.locks.set(userOpenId, prev.then(() => next));
    try {
      await prev;
      return await fn();
    } finally {
      release();
      // Drop the entry once the chain has drained — the next
      // acquirer will repopulate it. Avoids an unbounded growth of
      // resolved-promise entries for users that auth once and never
      // again.
      const cur = this.locks.get(userOpenId);
      if (cur) {
        // Schedule cleanup AFTER the current microtask so concurrent
        // acquirers waiting on `prev` don't get rescheduled into an
        // empty map (which would let them race without serialisation).
        cur.then(() => {
          if (this.locks.get(userOpenId) === cur) {
            this.locks.delete(userOpenId);
          }
        });
      }
    }
  }
}

export interface UATAccessorOptions {
  store: UATStore;
  authFlow: AuthFlowController;
  mutex: UATMutex;
  appId: string;
  appSecret: string;
  baseUrl: string;
  logger: Logger;
  fetchImpl?: typeof fetch;
}

export interface UATInvokeRequest {
  userOpenId: string;
  chatId: string;
  reason: string;
  scope?: string;
}

export type UATInvokeResult<T> =
  | { kind: "ok"; result: T }
  | { kind: "auth_failed"; outcome: AuthFlowOutcome };

/** Auto-auth wrapper around UAT-requiring Lark API calls.
 *
 * Lifecycle of one `invoke`:
 *   1. Resolve a usable UAT (refresh-or-reauth if needed).
 *   2. Call the handler with that UAT.
 *   3. If the handler throws / returns a Lark `99991663` / `99991664`
 *      response, the UAT became invalid mid-flight (revoked server-
 *      side, etc.). Drop it, run the auth flow, retry once. */
export class UATAccessor {
  constructor(private readonly opts: UATAccessorOptions) {}

  async invoke<T>(
    req: UATInvokeRequest,
    handler: (uat: string) => Promise<T>,
  ): Promise<UATInvokeResult<T>> {
    const uatRes = await this.resolveUAT(req);
    if (uatRes.kind !== "ok") return { kind: "auth_failed", outcome: uatRes };

    let result: T;
    try {
      result = await handler(uatRes.uat.accessToken);
    } catch (err) {
      if (!isLarkUATDeadError(err)) throw err;
      return await this.retryAfterReauth(req, handler);
    }
    if (isLarkUATDeadResponse(result)) {
      return await this.retryAfterReauth(req, handler);
    }
    return { kind: "ok", result };
  }

  private async retryAfterReauth<T>(
    req: UATInvokeRequest,
    handler: (uat: string) => Promise<T>,
  ): Promise<UATInvokeResult<T>> {
    // The stored UAT was just rejected by Lark — drop it before
    // running a fresh flow so the flow doesn't pick up the dead row.
    await this.opts.mutex.withLock(req.userOpenId, () =>
      this.opts.store.delete(req.userOpenId),
    );
    const flow = await this.opts.authFlow.requestUAT(req);
    if (flow.kind !== "ok") return { kind: "auth_failed", outcome: flow };
    try {
      const result = await handler(flow.uat.accessToken);
      return { kind: "ok", result };
    } catch (err) {
      if (isLarkUATDeadError(err)) {
        // Same UAT failing twice in a row likely means a permission
        // / scope issue masquerading as an auth error. Don't loop —
        // surface as auth_failed with the underlying message.
        return {
          kind: "auth_failed",
          outcome: {
            kind: "error",
            message: `UAT rejected after fresh re-auth: ${String(err)}`,
          },
        };
      }
      throw err;
    }
  }

  /** Internal: get a UAT that's safe to use *now*. Will refresh,
   * trigger the auth flow, or return the existing token depending on
   * what's in the store. Holds the per-user mutex throughout so the
   * refresh-scheduler can't race us. */
  private async resolveUAT(
    req: UATInvokeRequest,
  ): Promise<{ kind: "ok"; uat: StoredUAToken } | AuthFlowOutcome> {
    return await this.opts.mutex.withLock(req.userOpenId, async () => {
      const stored = await this.opts.store.get(req.userOpenId);
      const now = Date.now();
      if (
        stored &&
        stored.expiresAt - now > PROACTIVE_REFRESH_MS
      ) {
        return { kind: "ok", uat: stored };
      }
      // Try refresh first if the refresh_token is alive.
      if (
        stored &&
        stored.refreshToken.length > 0 &&
        stored.refreshExpiresAt > now
      ) {
        const refreshed = await refreshUAT({
          appId: this.opts.appId,
          appSecret: this.opts.appSecret,
          baseUrl: this.opts.baseUrl,
          logger: this.opts.logger,
          ...(this.opts.fetchImpl !== undefined && {
            fetchImpl: this.opts.fetchImpl,
          }),
          refreshToken: stored.refreshToken,
        });
        if (refreshed.ok) {
          const fresh = await this.opts.store.storeGrant(
            req.userOpenId,
            refreshed.token,
            stored.grantedAt,
          );
          return { kind: "ok", uat: fresh };
        }
        // Refresh failed terminally — drop and fall through to flow.
        if (refreshed.error === "expired_token" || refreshed.error === "access_denied") {
          await this.opts.store.delete(req.userOpenId);
        }
        // For network errors, we still attempt the auth flow — the
        // user is asking right now, so prompt them rather than
        // silently failing on a transient Lark hiccup.
      }
      // Fall through: trigger device-flow auth. Drop the mutex first
      // so the user-facing card flow (slow, multi-second) doesn't
      // hold the lock and starve the scheduler.
      // Note: dropping the mutex via `return` here means the refresh
      // scheduler could theoretically interleave between us deciding
      // to call requestUAT and the actual call. That's fine — the
      // scheduler would just observe an empty UAT and skip; meanwhile
      // requestUAT does its own dedup.
      return await this.runAuthFlow(req);
    });
  }

  private async runAuthFlow(
    req: UATInvokeRequest,
  ): Promise<{ kind: "ok"; uat: StoredUAToken } | AuthFlowOutcome> {
    const flow = await this.opts.authFlow.requestUAT(req);
    if (flow.kind === "ok") return { kind: "ok", uat: flow.uat };
    return flow;
  }
}

function isLarkUATDeadError(err: unknown): boolean {
  if (err === null || typeof err !== "object") return false;
  const e = err as { code?: unknown; response?: { data?: { code?: unknown } } };
  if (typeof e.code === "number" && UAT_DEAD_CODES.has(e.code)) return true;
  const respCode = e.response?.data?.code;
  return typeof respCode === "number" && UAT_DEAD_CODES.has(respCode);
}

function isLarkUATDeadResponse(result: unknown): boolean {
  if (result === null || typeof result !== "object") return false;
  const r = result as { code?: unknown };
  return typeof r.code === "number" && UAT_DEAD_CODES.has(r.code);
}
