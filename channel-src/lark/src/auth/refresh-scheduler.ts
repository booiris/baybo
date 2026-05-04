import type { Logger } from "@aura/channel-sdk";

import type { UATMutex } from "./auto-auth.js";
import { refreshUAT } from "./device-flow.js";
import type { StoredUAToken, UATStore } from "./uat-store.js";

export interface UATRefreshSchedulerOptions {
  store: UATStore;
  /** Per-user mutex shared with the auto-auth interceptor. Required
   * even when no interceptor is wired (e.g. tests) — OAuth 2.1's
   * one-time-use refresh tokens make a missing mutex a footgun the
   * moment any other code path also refreshes. */
  mutex: UATMutex;
  appId: string;
  appSecret: string;
  baseUrl: string;
  logger: Logger;
  /** Override fetch for tests. Defaults to global `fetch`. */
  fetchImpl?: typeof fetch;
  /** How often to sweep, in ms. Default 60_000 (one minute). */
  sweepIntervalMs?: number;
  /** Refresh a UAT when `expiresAt - now < refreshAheadMs`. Default
   * 300_000 (5 minutes), comfortably wider than the sweep interval so
   * we get at least one refresh attempt before expiry. */
  refreshAheadMs?: number;
}

export interface SweepResult {
  scanned: number;
  /** userOpenIds successfully refreshed this sweep. */
  refreshed: string[];
  /** userOpenIds whose refresh_token died — UAT was deleted from
   * the store so the next tool call triggers a fresh auth flow. */
  expired: string[];
  /** Refresh attempts that hit a transient error (network, etc.).
   * Stays in the store so the next sweep retries. */
  errors: { userOpenId: string; error: string }[];
}

/** Background UAT refresh scheduler.
 *
 * Periodically scans the store and refreshes any UAT close to expiry,
 * deleting tokens whose refresh_token has died so the next tool call
 * triggers a fresh device-flow auth instead of failing with a stale
 * 99991664. One scheduler per bot — multi-bot deployments run one
 * each.
 *
 * Race with on-demand refresh (slice B.4): if a tool call refreshes
 * at the same time as the scheduler, OAuth 2.1's one-time-use
 * refresh_token rule means one of the two will fail. The fix is a
 * per-user mutex shared between this scheduler and the auto-auth
 * interceptor — landed in slice B.4 alongside the interceptor itself
 * since they're the two sides of the same race. For now the
 * scheduler is the only refresh path, so the race window doesn't
 * exist yet. */
export class UATRefreshScheduler {
  private timer: ReturnType<typeof setInterval> | null = null;
  private inflight = false;
  private readonly sweepIntervalMs: number;
  private readonly refreshAheadMs: number;

  constructor(private readonly opts: UATRefreshSchedulerOptions) {
    this.sweepIntervalMs = opts.sweepIntervalMs ?? 60_000;
    this.refreshAheadMs = opts.refreshAheadMs ?? 300_000;
  }

  start(): void {
    if (this.timer !== null) return;
    this.timer = setInterval(() => {
      void this.runSweep().catch((err) => {
        this.opts.logger.warn(
          `lark UAT refresh scheduler tick failed: ${String(err)}`,
        );
      });
    }, this.sweepIntervalMs);
    // Don't keep the event loop alive on the scheduler alone — the
    // sidecar's WS keeps the process up; if the WS dies the sidecar
    // should exit cleanly without waiting on this timer.
    if (typeof this.timer === "object" && this.timer && "unref" in this.timer) {
      (this.timer as { unref: () => void }).unref();
    }
  }

  async stop(): Promise<void> {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    // Wait out a single in-flight sweep so we don't tear down state
    // mid-sweep. Race window is small and bounded; we don't model
    // cancellation deeper than this.
    while (this.inflight) {
      await new Promise((r) => setTimeout(r, 10));
    }
  }

  /** Run one sweep immediately, returning the per-user breakdown.
   * Visible for tests + manual triggering from `onDiagnoseRequested`. */
  async sweepOnce(now: number = Date.now()): Promise<SweepResult> {
    if (this.inflight) {
      return { scanned: 0, refreshed: [], expired: [], errors: [] };
    }
    this.inflight = true;
    try {
      return await this.runSweepImpl(now);
    } finally {
      this.inflight = false;
    }
  }

  private async runSweep(): Promise<void> {
    if (this.inflight) return;
    this.inflight = true;
    try {
      const result = await this.runSweepImpl(Date.now());
      if (
        result.refreshed.length > 0 ||
        result.expired.length > 0 ||
        result.errors.length > 0
      ) {
        this.opts.logger.info(
          `lark UAT refresh sweep: scanned=${result.scanned} refreshed=${result.refreshed.length} expired=${result.expired.length} errors=${result.errors.length}`,
        );
      }
    } finally {
      this.inflight = false;
    }
  }

  private async runSweepImpl(now: number): Promise<SweepResult> {
    const users = await this.opts.store.listUsers();
    const refreshed: string[] = [];
    const expired: string[] = [];
    const errors: { userOpenId: string; error: string }[] = [];

    for (const userOpenId of users) {
      // Hold the per-user mutex while we look + refresh + write so
      // an on-demand auto-auth interceptor on the same user can't
      // race us into invalid_grant.
      const outcome = await this.opts.mutex.withLock(userOpenId, () =>
        this.refreshOneIfDue(userOpenId, now),
      );
      if (outcome.kind === "skipped") continue;
      if (outcome.kind === "refreshed") refreshed.push(userOpenId);
      else if (outcome.kind === "expired") expired.push(userOpenId);
      else errors.push({ userOpenId, error: outcome.error });
    }
    return { scanned: users.length, refreshed, expired, errors };
  }

  private async refreshOneIfDue(
    userOpenId: string,
    now: number,
  ): Promise<
    | { kind: "skipped" }
    | { kind: "refreshed" }
    | { kind: "expired" }
    | { kind: "error"; error: string }
  > {
    const stored = await this.opts.store.get(userOpenId);
    if (!stored) return { kind: "skipped" };

    // Refresh-token already dead — drop the UAT so the next tool
    // call triggers a fresh device-flow auth.
    if (stored.refreshExpiresAt <= now) {
      await this.opts.store.delete(userOpenId);
      return { kind: "expired" };
    }
    // No refresh_token at all (Lark sometimes omits one) — leave it
    // be; it'll be cleaned up when the access_token finally expires
    // and a tool call reports 99991664.
    if (stored.refreshToken.length === 0) return { kind: "skipped" };
    // Not yet inside the refresh window — interceptor refreshed
    // proactively, or the timer fired before expiry; either way
    // we have nothing to do this sweep.
    if (stored.expiresAt - now > this.refreshAheadMs) return { kind: "skipped" };

    return await this.refreshOne(userOpenId, stored);
  }

  private async refreshOne(
    userOpenId: string,
    stored: StoredUAToken,
  ): Promise<
    | { kind: "refreshed" }
    | { kind: "expired" }
    | { kind: "error"; error: string }
  > {
    const result = await refreshUAT({
      appId: this.opts.appId,
      appSecret: this.opts.appSecret,
      baseUrl: this.opts.baseUrl,
      logger: this.opts.logger,
      ...(this.opts.fetchImpl !== undefined && {
        fetchImpl: this.opts.fetchImpl,
      }),
      refreshToken: stored.refreshToken,
    });
    if (result.ok) {
      // Preserve the original grantedAt so the auth card's "Authorized
      // at" subtitle remains stable across refreshes.
      await this.opts.store.storeGrant(userOpenId, result.token, stored.grantedAt);
      return { kind: "refreshed" };
    }
    // `expired_token` from Lark on a refresh = the refresh_token itself
    // is dead. Drop the UAT so re-auth kicks in.
    if (result.error === "expired_token" || result.error === "access_denied") {
      await this.opts.store.delete(userOpenId);
      return { kind: "expired" };
    }
    return { kind: "error", error: result.message };
  }
}
