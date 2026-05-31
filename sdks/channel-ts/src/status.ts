import type { Logger } from "./logger.js";

/**
 * Opaque id of the live "status" message on the platform (Telegram
 * `message_id`, …). The condenser only ever round-trips it back to the
 * sink — it never inspects the value.
 */
export type StatusMessageId = number | string;

/**
 * Tuning knobs for the in-flight progress status message. All optional;
 * the {@link BotChannel} fills sensible defaults. Surfaced on
 * `BotChannelOptions.statusProgress` alongside the typing knobs.
 */
export interface StatusProgressOptions {
  /**
   * How long a turn may run with no final answer before the status
   * message is materialized. The first tool call materializes it
   * immediately regardless; this only covers the no-tool-yet case
   * (slow first model call). Default: 6000 ms.
   */
  appearAfterMs?: number;
  /**
   * Floor between consecutive edits of the status message. Bursty
   * progress events coalesce in memory and only the latest is flushed
   * once per interval — keeps well under Telegram's same-chat edit
   * rate limit. Default: 2500 ms.
   */
  minEditIntervalMs?: number;
  /**
   * Cadence of the liveness re-edit that advances the elapsed clock and
   * spinner so a long single tool never looks frozen. Default: 3000 ms.
   */
  heartbeatMs?: number;
}

/**
 * Best-effort delivery surface the condenser drives. {@link BotChannel}
 * implements it by re-resolving the live `(handle, chat)` route for the
 * user and calling the platform's status hooks. Both calls are
 * best-effort: `send` returns `null` when there's no live route (bot
 * detached), and `edit` may reject — a {@link StatusRateLimited}
 * rejection asks the condenser to back off for `retryAfterMs`.
 */
export interface StatusSink {
  send(userId: string, text: string): Promise<StatusMessageId | null>;
  edit(userId: string, messageId: StatusMessageId, text: string): Promise<void>;
}

/**
 * A {@link StatusSink} may throw this to ask the condenser to suppress
 * edits for `retryAfterMs` (e.g. a Telegram 429 carrying `retry_after`).
 */
export class StatusRateLimited extends Error {
  constructor(public readonly retryAfterMs: number) {
    super(`status edit rate-limited; retry after ${retryAfterMs}ms`);
    this.name = "StatusRateLimited";
  }
}

/** What the agent is doing right now, fed by the runtime progress frames. */
type ProgressEvent =
  | { kind: "tool"; tool: string; label?: string }
  | { kind: "toolDone" }
  | { kind: "status"; phase: string }
  | { kind: "delta" };

interface ConfiguredOptions {
  appearAfterMs: number;
  minEditIntervalMs: number;
  heartbeatMs: number;
  safetyMs: number;
}

interface TurnState {
  /** Queued turns for this user; >1 only on a rapid double-send. */
  pending: number;
  startedAt: number;
  steps: number;
  /** The current action line (already glyph-prefixed). */
  action: string;
  /** Approval in flight — heartbeat + safety are paused while true. */
  blocked: boolean;
  /** Flipped once a delta arrives so per-token deltas don't churn. */
  responding: boolean;
  /** Wall-clock of the last activity, for the quiet-time safety cap. */
  lastActivityAt: number;
  /** Spinner frame index, advanced by the heartbeat. */
  spin: number;
  /** Set once finalized so late timers/flushes no-op. */
  terminal: boolean;
  /** Set on teardown so an in-flight flush stops rescheduling. */
  destroyed: boolean;

  messageId: StatusMessageId | null;
  /** Guards against a double `send` while the first is in flight. */
  materializing: boolean;

  // Throttled single-flight editor state.
  lastSentText: string;
  inFlight: boolean;
  lastEditAt: number;
  suppressUntil: number;
  flushTimer: ReturnType<typeof setTimeout> | null;

  appearTimer: ReturnType<typeof setTimeout> | null;
  heartbeatTimer: ReturnType<typeof setInterval> | null;
}

const DEFAULT_APPEAR_AFTER_MS = 6000;
const DEFAULT_MIN_EDIT_INTERVAL_MS = 2500;
const DEFAULT_HEARTBEAT_MS = 3000;

const THINKING = "🤔 working…";
const WORKING = "⚙️ working…";
const RESPONDING = "✍️ drafting reply…";
const COMPACTING = "🗜️ compacting context…";
const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] as const;

/**
 * Condenses an agent turn's mechanical progress (`ToolStarted`,
 * `ToolCompleted`, `Status`, answer deltas) into a single
 * edited-in-place "status message" per user. Platform-agnostic: it
 * talks only to a {@link StatusSink}, so it unit-tests against a fake
 * sink with real timers.
 *
 * Lifecycle (driven by {@link BotChannel}): `onInbound` opens a turn;
 * the first tool call (or `appearAfterMs`) materializes the message;
 * progress events update a coalesced "latest text" that a throttled
 * single-flight editor flushes; `onApprovalRequested` pauses it on a
 * `⏸ waiting` line; `onReply` finalizes it to a `✅ done` footer. A
 * quiet-time safety cap self-finalizes a wedged turn so a dropped
 * `onReply` can't strand the spinner.
 */
export class StatusCondenser {
  private readonly opts: ConfiguredOptions;
  private readonly sink: StatusSink;
  private readonly logger: Logger;
  private readonly turns = new Map<string, TurnState>();

  constructor(args: {
    sink: StatusSink;
    logger: Logger;
    options?: StatusProgressOptions;
    safetyMs: number;
  }) {
    this.sink = args.sink;
    this.logger = args.logger;
    this.opts = {
      appearAfterMs: args.options?.appearAfterMs ?? DEFAULT_APPEAR_AFTER_MS,
      minEditIntervalMs:
        args.options?.minEditIntervalMs ?? DEFAULT_MIN_EDIT_INTERVAL_MS,
      heartbeatMs: args.options?.heartbeatMs ?? DEFAULT_HEARTBEAT_MS,
      safetyMs: args.safetyMs,
    };
  }

  /** A user turn was accepted. Opens a fresh turn, or counts a queued
   * one (rapid double-send) so the running bubble survives until the
   * last reply. */
  onInbound(userId: string): void {
    const existing = this.turns.get(userId);
    if (existing && !existing.terminal) {
      existing.pending += 1;
      existing.lastActivityAt = Date.now();
      return;
    }
    this.openTurn(userId, 1);
  }

  /** A mechanical progress signal arrived for the user's in-flight turn. */
  onProgress(userId: string, ev: ProgressEvent): void {
    const state = this.turns.get(userId);
    if (!state || state.terminal) return;
    state.lastActivityAt = Date.now();
    state.blocked = false;
    let materializeNow = false;
    switch (ev.kind) {
      case "tool":
        state.steps += 1;
        state.action = `🔧 ${ev.label ?? ev.tool}`;
        state.responding = false;
        // First tool is the canonical "this turn has real work" signal.
        materializeNow = true;
        break;
      case "toolDone":
        // Keep the running tool line until the next signal — no flicker
        // back to a generic line between back-to-back tool calls.
        break;
      case "status":
        state.action = ev.phase === "compacting" ? COMPACTING : WORKING;
        state.responding = false;
        break;
      case "delta":
        if (state.responding) return;
        state.responding = true;
        state.action = RESPONDING;
        break;
    }
    if (state.messageId === null) {
      if (materializeNow) {
        this.clearTimer(state, "appearTimer");
        void this.materialize(userId, state);
      }
      return;
    }
    this.scheduleFlush(userId, state);
  }

  /** Liveness-only touch (e.g. an out-of-band notice) — resets the
   * quiet-time safety cap without changing the displayed action. */
  touch(userId: string): void {
    const state = this.turns.get(userId);
    if (state && !state.terminal) state.lastActivityAt = Date.now();
  }

  /** A tool call is blocked on the user. Switch to a `⏸ waiting` line
   * and pause the heartbeat — the bottleneck is the user, not the
   * agent, so don't burn the edit budget redrawing. */
  onApprovalRequested(userId: string, tool: string): void {
    const state = this.turns.get(userId);
    if (!state || state.terminal) return;
    state.blocked = true;
    state.action = `⏸ waiting for your approval: ${tool}`;
    if (state.messageId === null) {
      this.clearTimer(state, "appearTimer");
      void this.materialize(userId, state);
    } else {
      this.scheduleFlush(userId, state);
    }
  }

  /** The approval was decided (approve/deny) — resume Working. */
  onApprovalSettled(userId: string): void {
    const state = this.turns.get(userId);
    if (!state || state.terminal || !state.blocked) return;
    state.blocked = false;
    state.action = WORKING;
    state.lastActivityAt = Date.now();
    if (state.messageId !== null) this.scheduleFlush(userId, state);
  }

  /** The turn produced its final answer. Collapse the status message to
   * a `✅ done` footer, then advance to the next queued turn or tear
   * down. Short turns that never materialized show nothing. */
  onReply(userId: string): void {
    const state = this.turns.get(userId);
    if (!state) return;
    state.terminal = true;
    const steps = state.steps > 0 ? ` · ${state.steps}× tools` : "";
    this.finalize(userId, state, `✅ done · ${this.elapsed(state)}${steps}`);
    const remaining = state.pending - 1;
    this.teardown(userId, state);
    if (remaining > 0) this.openTurn(userId, remaining);
  }

  /** Abnormal end (bot detached/crashed, channel stop, quiet-time cap).
   * Best-effort `⏹ ended` footer; no-op when nothing materialized. */
  onForceEnd(userId: string): void {
    const state = this.turns.get(userId);
    if (!state) return;
    state.terminal = true;
    this.finalize(userId, state, `⏹ ended · ${this.elapsed(state)}`);
    this.teardown(userId, state);
  }

  /** Channel shutdown — force-end every live turn. */
  forceEndAll(): void {
    for (const userId of [...this.turns.keys()]) this.onForceEnd(userId);
  }

  private openTurn(userId: string, pending: number): void {
    const now = Date.now();
    const state: TurnState = {
      pending,
      startedAt: now,
      steps: 0,
      action: THINKING,
      blocked: false,
      responding: false,
      lastActivityAt: now,
      spin: 0,
      terminal: false,
      destroyed: false,
      messageId: null,
      materializing: false,
      lastSentText: "",
      inFlight: false,
      lastEditAt: 0,
      suppressUntil: 0,
      flushTimer: null,
      appearTimer: null,
      heartbeatTimer: null,
    };
    state.appearTimer = setTimeout(() => {
      state.appearTimer = null;
      if (!state.terminal && state.messageId === null) {
        void this.materialize(userId, state);
      }
    }, this.opts.appearAfterMs);
    state.appearTimer.unref?.();
    state.heartbeatTimer = setInterval(
      () => this.heartbeat(userId, state),
      this.opts.heartbeatMs,
    );
    state.heartbeatTimer.unref?.();
    this.turns.set(userId, state);
  }

  private heartbeat(userId: string, state: TurnState): void {
    if (state.terminal || state.blocked) return;
    if (Date.now() - state.lastActivityAt > this.opts.safetyMs) {
      this.onForceEnd(userId);
      return;
    }
    if (state.messageId === null) {
      // appearTimer is the primary trigger; retry here only if a prior
      // materialize couldn't send (no route at the time).
      if (Date.now() - state.startedAt >= this.opts.appearAfterMs) {
        void this.materialize(userId, state);
      }
      return;
    }
    state.spin = (state.spin + 1) % SPINNER.length;
    this.scheduleFlush(userId, state);
  }

  private async materialize(userId: string, state: TurnState): Promise<void> {
    if (state.materializing || state.messageId !== null || state.terminal) {
      return;
    }
    state.materializing = true;
    const text = this.render(state);
    let id: StatusMessageId | null = null;
    try {
      id = await this.sink.send(userId, text);
    } catch (err) {
      this.logger.debug("status send failed", err);
    }
    state.materializing = false;
    if (id === null || state.destroyed) return;
    state.messageId = id;
    state.lastSentText = text;
    state.lastEditAt = Date.now();
    // Apply anything that changed while the send was in flight.
    if (this.render(state) !== state.lastSentText) {
      this.scheduleFlush(userId, state);
    }
  }

  private scheduleFlush(userId: string, state: TurnState): void {
    if (state.inFlight || state.messageId === null || state.destroyed) return;
    const now = Date.now();
    const earliest = Math.max(
      state.lastEditAt + this.opts.minEditIntervalMs,
      state.suppressUntil,
    );
    if (now >= earliest) {
      void this.flush(userId, state);
      return;
    }
    if (state.flushTimer === null) {
      state.flushTimer = setTimeout(() => {
        state.flushTimer = null;
        this.scheduleFlush(userId, state);
      }, earliest - now);
      state.flushTimer.unref?.();
    }
  }

  private async flush(userId: string, state: TurnState): Promise<void> {
    if (state.inFlight || state.messageId === null) return;
    const text = this.render(state);
    if (text === state.lastSentText) return;
    state.inFlight = true;
    try {
      await this.sink.edit(userId, state.messageId, text);
      state.lastSentText = text;
      state.lastEditAt = Date.now();
    } catch (err) {
      const retryAfterMs =
        err instanceof StatusRateLimited
          ? err.retryAfterMs
          : this.opts.minEditIntervalMs;
      state.suppressUntil = Date.now() + retryAfterMs;
      this.logger.debug("status edit failed; backing off", err);
    } finally {
      state.inFlight = false;
    }
    if (
      !state.terminal &&
      !state.destroyed &&
      this.render(state) !== state.lastSentText
    ) {
      this.scheduleFlush(userId, state);
    }
  }

  /** Single best-effort terminal edit, bypassing the throttle. */
  private finalize(userId: string, state: TurnState, text: string): void {
    if (state.messageId === null || text === state.lastSentText) return;
    const messageId = state.messageId;
    state.lastSentText = text;
    this.sink.edit(userId, messageId, text).catch((err: unknown) => {
      this.logger.debug("status finalize edit failed", err);
    });
  }

  private teardown(userId: string, state: TurnState): void {
    state.destroyed = true;
    this.clearTimer(state, "appearTimer");
    this.clearTimer(state, "flushTimer");
    if (state.heartbeatTimer !== null) {
      clearInterval(state.heartbeatTimer);
      state.heartbeatTimer = null;
    }
    if (this.turns.get(userId) === state) this.turns.delete(userId);
  }

  private clearTimer(
    state: TurnState,
    field: "appearTimer" | "flushTimer",
  ): void {
    const timer = state[field];
    if (timer !== null) {
      clearTimeout(timer);
      state[field] = null;
    }
  }

  private render(state: TurnState): string {
    if (state.blocked) return state.action;
    const spinner = SPINNER[state.spin % SPINNER.length] ?? SPINNER[0];
    const steps = state.steps > 0 ? ` · ${state.steps}× tools` : "";
    return `${state.action}\n⏱ ${this.elapsed(state)} ${spinner}${steps}`;
  }

  private elapsed(state: TurnState): string {
    return fmtElapsed(Date.now() - state.startedAt);
  }
}

/** `8s`, `41s`, `2m04s` — seconds under a minute, `m`+zero-padded-`s` over. */
export function fmtElapsed(ms: number): string {
  const totalSec = Math.max(0, Math.floor(ms / 1000));
  if (totalSec < 60) return `${totalSec}s`;
  const m = Math.floor(totalSec / 60);
  const s = totalSec % 60;
  return `${m}m${s.toString().padStart(2, "0")}s`;
}
