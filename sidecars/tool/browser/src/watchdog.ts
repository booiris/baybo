// Liveness watchdog for the browser this sidecar drives.
//
// The gateway already supervises the sidecar *process*: `McpReconciler`
// probes every embedded MCP server on a 5s tick and respawns a child whose
// stdio pipe broke (crates/tools/src/mcp/reconciler.rs). What that probe
// cannot see is a dead **browser** behind a live sidecar. The probe is a
// `tools/list`, and our proxy answers it in-process — so Chrome can be gone
// for hours while the MCP server looks perfectly healthy and every
// `browser/*` call comes back "Could not connect to Chrome".
//
// This watchdog closes that gap: it periodically asks the browser itself
// whether it is alive, repairs it in place when it isn't, and — once
// in-place repair has failed repeatedly — exits so the gateway's reconciler
// respawns a sidecar that re-runs the whole boot sequence (stale-lock
// clearing, container sweep, Chrome install). Delegating the hopeless case
// upward is deliberate: full re-initialisation already exists at boot and
// does not need a second, subtly-different implementation here.
//
// Per-mode behaviour plugs in through `BrowserHealer` rather than branching
// inside the loop. Host-headless, docker, and operator-managed CDP differ in
// what proves liveness and what repair means, but share one
// detect → repair → escalate lifecycle.

import { errText, type SidecarLogger } from "./log.js";

/** Idle cadence between liveness probes. */
const PROBE_INTERVAL_MS = 30_000;

/**
 * A probe that hasn't answered by now counts as failed. A wedged Chrome
 * (renderer hung, container alive but unresponsive) never answers at all,
 * so without a deadline the watchdog would block on it forever — the exact
 * failure it exists to catch.
 */
const PROBE_TIMEOUT_MS = 10_000;

/** Pause between the probes of one unhealthy streak. */
const RECHECK_DELAY_MS = 2_000;

/**
 * Consecutive failed probes before recovery runs. Recovery is destructive
 * in docker mode — the container, and every tab open in it, is replaced —
 * so one transient loopback error must not be enough to trigger it.
 */
const UNHEALTHY_PROBES = 2;

/**
 * Consecutive failed recoveries before the sidecar gives up on in-place
 * repair. See `giveUp` on {@link WatchdogDeps}.
 */
const MAX_RECOVERY_ATTEMPTS = 3;

/** Cadence while unhealthy — tighter than the idle interval so a recovering
 * browser comes back quickly, loose enough not to hammer a broken host. */
const RECOVERY_RETRY_MS = 5_000;

/**
 * Tracks whether the browser is being used, so the watchdog can stay out of
 * the way. Live traffic is better evidence of liveness than any synthetic
 * probe, and chrome-devtools-mcp serialises every tool call behind a single
 * mutex — a probe fired mid-call would queue behind it and time out against
 * a perfectly healthy browser.
 */
export class BrowserActivity {
    #inFlight = 0;
    #lastCompletedAt = 0;
    #everCalled = false;

    begin(): void {
        this.#inFlight += 1;
    }

    /**
     * @param ok whether the call actually worked. Only a success counts as
     * evidence of liveness — otherwise an agent retrying every few seconds
     * against a dead browser would keep the watchdog permanently stood down,
     * which is the exact situation it exists for. A *failed* call still arms
     * the watchdog: it means Chrome was at least launched, so there is now
     * something to watch.
     */
    end(ok: boolean): void {
        this.#inFlight = Math.max(0, this.#inFlight - 1);
        if (ok) this.#lastCompletedAt = Date.now();
        this.#everCalled = true;
    }

    get busy(): boolean {
        return this.#inFlight > 0;
    }

    /**
     * Whether a tool call has ever completed. Host-headless launches Chrome
     * lazily on the first call, so this doubles as "does a browser exist yet".
     */
    get everCalled(): boolean {
        return this.#everCalled;
    }

    msSinceLastCall(): number {
        return this.#everCalled
            ? Date.now() - this.#lastCompletedAt
            : Number.POSITIVE_INFINITY;
    }
}

/**
 * The watchdog's window onto `InstallState.phase`, which server.ts owns.
 *
 * Flipping off `ready` is what makes `GuardingTransport` answer an agent's
 * `tools/call` with "the browser is restarting, retry shortly" instead of
 * letting a raw puppeteer "Target closed" reach the model.
 */
export interface PhaseGate {
    isReady(): boolean;
    /** `reason` reaches the model verbatim, so it should read as a diagnosis. */
    enterRecovering(reason: string): void;
    markReady(): void;
}

export interface BrowserHealer {
    /** Label for logs: `host`, `docker`, `cdp_url`. */
    readonly mode: string;

    /**
     * Whether exiting the process could plausibly fix things. False for
     * operator-managed CDP: we don't own that Chrome, so respawning
     * ourselves changes nothing and would just crash-loop against the
     * gateway's reconciler until the operator's browser comes back.
     */
    readonly canEscalate: boolean;

    /**
     * Whether there is a browser to watch yet. Host-headless defers the
     * Chrome launch to the first tool call, so watching from boot would both
     * mis-report "unhealthy" and force a Chrome the operator may never use
     * into memory.
     */
    armed(): boolean;

    /** Resolve = alive. Reject with a human-readable reason = not alive. */
    check(signal: AbortSignal): Promise<void>;

    /**
     * What the model is told while this mode is down. Per-healer because the
     * honest answer differs: two of the modes are restarting the browser, and
     * the operator-managed one is only waiting for someone else to.
     */
    outageText(reason: string): string;

    /**
     * Repair the browser in place, and prove the repair took. Implementations
     * end with a real end-to-end call rather than resolving optimistically:
     * a respawned container whose CDP port answers can still be unreachable
     * through the cached puppeteer connection. Reject if repair is impossible
     * or did not work.
     */
    recover(): Promise<void>;
}

export type TickOutcome =
    | "skipped"
    | "healthy"
    | "recovered"
    | "recovering"
    | "gave-up";

export interface WatchdogDeps {
    healer: BrowserHealer;
    activity: BrowserActivity;
    phase: PhaseGate;
    log: SidecarLogger;
    /**
     * Called when in-place recovery is exhausted and the healer allows
     * escalation. server.ts wires this to container cleanup + `process.exit(1)`
     * so the gateway's reconciler respawns a sidecar from a clean slate.
     */
    giveUp: (reason: string) => void;
    /** Injected so tests drive the loop without real time passing. */
    sleep?: (ms: number) => Promise<void>;
    /** Injected for the same reason; defaults to {@link PROBE_TIMEOUT_MS}. */
    probeTimeoutMs?: number;
}

const defaultSleep = (ms: number): Promise<void> =>
    new Promise((resolve) => {
        setTimeout(resolve, ms).unref();
    });

export class BrowserWatchdog {
    readonly #deps: WatchdogDeps;
    readonly #sleep: (ms: number) => Promise<void>;
    readonly #probeTimeoutMs: number;
    #recovering = false;
    #recoveryAttempts = 0;
    #reportedUnhealthy = false;
    #stopped = false;
    #tick: Promise<unknown> | null = null;

    constructor(deps: WatchdogDeps) {
        this.#deps = deps;
        this.#sleep = deps.sleep ?? defaultSleep;
        this.#probeTimeoutMs = deps.probeTimeoutMs ?? PROBE_TIMEOUT_MS;
    }

    /** True while the browser is known-broken and repair is in flight. */
    get recovering(): boolean {
        return this.#recovering;
    }

    /**
     * Run one decision cycle. Split out from {@link start} so tests drive the
     * state machine directly instead of racing real timers.
     */
    async runOnce(): Promise<TickOutcome> {
        const { healer, activity, phase, log } = this.#deps;

        // Boot phases (installing, docker-*) and a terminal install failure
        // belong to main(); the watchdog only owns the post-ready lifecycle.
        // Once it has declared `recovering` itself, though, it must keep
        // working — that phase is its own.
        if (!phase.isReady() && !this.#recovering) return "skipped";
        if (!healer.armed()) return "skipped";
        // Live traffic already proves the browser answers, and a probe would
        // only queue behind it on chrome-devtools-mcp's tool mutex.
        if (this.#inUse()) return "skipped";

        let reason = await this.#probeStreak();

        // The probe streak costs up to PROBE_TIMEOUT_MS * 2 + RECHECK_DELAY_MS,
        // so the gate above is a stale reading by the time repair would start —
        // and repair is destructive (it SIGKILLs Chrome or replaces the
        // container). Traffic that started, or succeeded, during the streak is
        // stronger evidence than the probe that just failed, so yield to it and
        // let the next tick decide against fresh state.
        if (reason !== undefined && this.#inUse()) {
            log.debug(`probe failed (${reason}) but the browser is serving calls; deferring`);
            return "skipped";
        }

        if (reason === undefined) {
            if (this.#recovering) {
                this.#recovering = false;
                this.#recoveryAttempts = 0;
                this.#reportedUnhealthy = false;
                phase.markReady();
                log.info(`browser recovered (mode=${healer.mode})`);
                return "recovered";
            }
            return "healthy";
        }

        // Logged once per outage, not once per tick. An outage the watchdog
        // cannot end — `cdp_url` waiting on the operator's Chrome — otherwise
        // emits lines forever, and the gateway's stderr drain demotes
        // everything past MCP_STDERR_INFO_LINE_BUDGET to debug!, which would
        // silence this sidecar's real warnings for the rest of the process.
        if (!this.#reportedUnhealthy) {
            this.#reportedUnhealthy = true;
            log.warn(
                `browser is not responding (mode=${healer.mode}): ${reason}; attempting recovery`,
            );
        }

        this.#recovering = true;
        phase.enterRecovering(healer.outageText(reason));

        try {
            await healer.recover();
            reason = await this.#probe();
        } catch (e) {
            reason = errText(e);
        }

        if (reason === undefined) {
            this.#recovering = false;
            this.#recoveryAttempts = 0;
            this.#reportedUnhealthy = false;
            phase.markReady();
            log.info(`browser recovered (mode=${healer.mode})`);
            return "recovered";
        }

        this.#recoveryAttempts += 1;
        if (healer.canEscalate && this.#recoveryAttempts >= MAX_RECOVERY_ATTEMPTS) {
            const summary =
                `browser recovery failed ${this.#recoveryAttempts} times (mode=${healer.mode}, ` +
                `last error: ${reason}); exiting so the gateway respawns this sidecar from a clean boot`;
            log.error(summary);
            this.#deps.giveUp(summary);
            return "gave-up";
        }
        return "recovering";
    }

    start(): void {
        void (async () => {
            while (!this.#stopped) {
                await this.#sleep(this.#retryDelayMs());
                // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition -- `stop()` flips this during the await above; TS narrowing can't model a field mutated across a suspension point.
                if (this.#stopped) return;
                const tick = this.runOnce().catch((e: unknown) => {
                    // A watchdog that dies on an unexpected throw is worse than
                    // no watchdog: the browser looks supervised and is not.
                    this.#deps.log.warn(`watchdog tick threw: ${errText(e)}`);
                });
                this.#tick = tick;
                await tick;
                this.#tick = null;
            }
        })();
    }

    /** Stop scheduling ticks. Does not interrupt one already running. */
    stop(): void {
        this.#stopped = true;
    }

    /**
     * Resolve once no tick is running. A single docker repair can be in flight
     * for minutes (`docker rm` → image resolve → `docker run` → CDP wait), and
     * a `stop()` that only gates the *next* tick would let shutdown exit while
     * `spawnContainer` is still running — orphaning a container that no live
     * pid owns, which the next-boot sweep skips.
     */
    async quiesce(): Promise<void> {
        this.#stopped = true;
        await this.#tick;
    }

    #retryDelayMs(): number {
        // A healer that cannot repair (operator-managed Chrome) is only
        // waiting, so poll it at the idle cadence rather than the tighter
        // repair one — there is nothing for the faster loop to accomplish.
        if (!this.#recovering) return PROBE_INTERVAL_MS;
        return this.#deps.healer.canEscalate ? RECOVERY_RETRY_MS : PROBE_INTERVAL_MS;
    }

    /** Whether real traffic is proving the browser alive right now. */
    #inUse(): boolean {
        const { activity } = this.#deps;
        return activity.busy || activity.msSinceLastCall() < PROBE_INTERVAL_MS;
    }

    /** Probe until one succeeds or the unhealthy streak is complete. */
    async #probeStreak(): Promise<string | undefined> {
        let reason: string | undefined;
        for (let attempt = 1; attempt <= UNHEALTHY_PROBES; attempt += 1) {
            if (attempt > 1) await this.#sleep(RECHECK_DELAY_MS);
            reason = await this.#probe();
            if (reason === undefined) return undefined;
        }
        return reason;
    }

    /** One bounded probe. Returns the failure reason, or undefined if alive. */
    async #probe(): Promise<string | undefined> {
        const controller = new AbortController();
        const timer = setTimeout(() => {
            controller.abort();
        }, this.#probeTimeoutMs);
        timer.unref();
        try {
            await this.#deps.healer.check(controller.signal);
            return undefined;
        } catch (e) {
            return controller.signal.aborted
                ? `no response within ${this.#probeTimeoutMs}ms`
                : errText(e);
        } finally {
            clearTimeout(timer);
        }
    }
}
