// Unit tests for the browser watchdog's state machine.
//
// Driven through `runOnce()` rather than `start()` so the decisions are
// exercised directly instead of raced against real timers; the injected
// `sleep` collapses the recheck/backoff waits to nothing.
//
// Imports `dist/test/watchdog.mjs` — the unminified second output esbuild
// emits precisely so this file can reach the module (see esbuild.config.mjs).

import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const modulePath = resolve(here, "..", "dist", "test", "watchdog.mjs");
if (!existsSync(modulePath)) {
    throw new Error(
        `watchdog test bundle missing at ${modulePath} — package.json's test script ` +
            `runs \`node esbuild.config.mjs\` before tests; rebuild and retry.`,
    );
}
const { BrowserActivity, BrowserWatchdog } = await import(modulePath);

const NOOP_LOG = { info() {}, debug() {}, warn() {}, error() {} };

/** An activity tracker that looks idle and long-untouched — the watching state. */
function idleActivity() {
    const activity = new BrowserActivity();
    activity.begin();
    activity.end(true);
    // `end()` stamps "just now", which would make the watchdog skip. Rewind it
    // past the probe interval by overriding the one accessor the loop consults.
    activity.msSinceLastCall = () => Number.POSITIVE_INFINITY;
    return activity;
}

function phaseSpy(initial = "ready") {
    return {
        phase: initial,
        reasons: [],
        isReady() {
            return this.phase === "ready";
        },
        enterRecovering(reason) {
            this.phase = "recovering";
            this.reasons.push(reason);
        },
        markReady() {
            this.phase = "ready";
        },
    };
}

/**
 * @param {object} opts
 * @param {Array<Error|null>} opts.checks results handed out per check() call
 * @param {Array<Error|null>} [opts.recoveries] results handed out per recover()
 */
function fakeHealer({ checks, recoveries = [], canEscalate = true, armed = true }) {
    const calls = { check: 0, recover: 0 };
    return {
        mode: "test",
        canEscalate,
        calls,
        armed: () => armed,
        outageText: (reason) => `outage: ${reason}`,
        check() {
            const outcome = checks[Math.min(calls.check, checks.length - 1)];
            calls.check += 1;
            return outcome ? Promise.reject(outcome) : Promise.resolve();
        },
        recover() {
            const outcome = recoveries[Math.min(calls.recover, recoveries.length - 1)];
            calls.recover += 1;
            return outcome ? Promise.reject(outcome) : Promise.resolve();
        },
    };
}

const TEST_PROBE_TIMEOUT_MS = 20;

function makeWatchdog(healer, { activity = idleActivity(), phase = phaseSpy() } = {}) {
    const gaveUp = [];
    const watchdog = new BrowserWatchdog({
        healer,
        activity,
        phase,
        log: NOOP_LOG,
        giveUp: (reason) => gaveUp.push(reason),
        sleep: () => Promise.resolve(),
        probeTimeoutMs: TEST_PROBE_TIMEOUT_MS,
    });
    return { watchdog, phase, activity, gaveUp };
}

test("a healthy browser is left alone", async () => {
    const healer = fakeHealer({ checks: [null] });
    const { watchdog, phase } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "healthy");
    assert.equal(phase.phase, "ready", "phase untouched while healthy");
    assert.equal(healer.calls.recover, 0, "no recovery attempted");
    assert.equal(healer.calls.check, 1, "a passing probe ends the streak immediately");
});

test("a single failed probe does not trigger recovery", async () => {
    // Recovery is destructive in docker mode (the container and every open tab
    // is replaced), so one transient error must not be enough.
    const healer = fakeHealer({ checks: [new Error("ECONNRESET"), null] });
    const { watchdog, phase } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "healthy");
    assert.equal(healer.calls.recover, 0, "second probe passed — nothing to repair");
    assert.equal(phase.phase, "ready");
});

test("a sustained outage recovers and re-opens the tool surface", async () => {
    const healer = fakeHealer({
        // two failed probes to declare it unhealthy, then the post-recovery probe passes
        checks: [new Error("Target closed"), new Error("Target closed"), null],
        recoveries: [null],
    });
    const { watchdog, phase } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "recovered");
    assert.equal(healer.calls.recover, 1);
    assert.equal(phase.phase, "ready", "phase returns to ready so tool calls flow again");
    assert.deepEqual(
        phase.reasons,
        ["outage: Target closed"],
        "the healer's own outage copy reaches the phase gate — GuardingTransport shows it to the model",
    );
});

test("traffic that resumes during the probe streak defers the destructive repair", async () => {
    // The probe streak takes seconds, so the idle check at the top of the tick
    // is stale by the time repair would run — and repair SIGKILLs Chrome or
    // replaces the container. Real traffic outranks a probe that just failed.
    const activity = idleActivity();
    const healer = fakeHealer({
        checks: [new Error("flaky"), new Error("queued behind a real call")],
        recoveries: [null],
    });
    const realCheck = healer.check.bind(healer);
    healer.check = () => {
        // The agent starts a call while the streak is in progress.
        activity.begin();
        return realCheck();
    };
    const { watchdog, phase } = makeWatchdog(healer, { activity });

    assert.equal(await watchdog.runOnce(), "skipped");
    assert.equal(healer.calls.recover, 0, "a busy browser is never killed");
    assert.equal(phase.phase, "ready", "and the tool surface stays open");
});

test("an unrecoverable outage logs once, not once per tick", async () => {
    // The gateway's stderr drain demotes everything past its per-spawn line
    // budget to debug!, so a cdp_url outage that logged every tick would
    // silence this sidecar's real warnings for the rest of the process.
    const lines = [];
    const healer = fakeHealer({
        checks: [new Error("ECONNREFUSED")],
        recoveries: [null],
        canEscalate: false,
    });
    const watchdog = new BrowserWatchdog({
        healer,
        activity: idleActivity(),
        phase: phaseSpy(),
        log: {
            info: (m) => lines.push(m),
            debug: (m) => lines.push(m),
            warn: (m) => lines.push(m),
            error: (m) => lines.push(m),
        },
        giveUp: () => {},
        sleep: () => Promise.resolve(),
        probeTimeoutMs: TEST_PROBE_TIMEOUT_MS,
    });

    for (let i = 0; i < 25; i += 1) await watchdog.runOnce();
    assert.equal(lines.length, 1, `expected one line for the whole outage, got ${lines.length}`);
});

test("quiesce waits for a tick that is already running", async () => {
    // A docker repair can be mid-spawnContainer when SIGTERM lands; exiting
    // under it orphans the container it is about to create.
    let releaseRecover;
    const healer = fakeHealer({ checks: [new Error("dead")] });
    healer.recover = () => new Promise((resolve) => (releaseRecover = resolve));
    const { watchdog } = makeWatchdog(healer);

    watchdog.start();
    // Let start()'s zero-length sleep run and the tick reach recover().
    while (releaseRecover === undefined) await new Promise((r) => setImmediate(r));

    let quiesced = false;
    const done = watchdog.quiesce().then(() => (quiesced = true));
    await new Promise((r) => setImmediate(r));
    assert.equal(quiesced, false, "quiesce must not resolve while a tick is in flight");

    releaseRecover();
    await done;
    assert.equal(quiesced, true, "and must resolve once it settles");
});

test("the phase is held off `ready` while recovery keeps failing", async () => {
    const healer = fakeHealer({
        checks: [new Error("dead"), new Error("dead")],
        recoveries: [new Error("docker run failed")],
    });
    const { watchdog, phase, gaveUp } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "recovering");
    assert.equal(phase.phase, "recovering", "agent calls get the retry message, not a raw error");
    assert.equal(gaveUp.length, 0, "one failure is not enough to escalate");
});

test("escalates to the gateway after repeated failed recoveries", async () => {
    const healer = fakeHealer({
        checks: [new Error("dead")],
        recoveries: [new Error("docker run failed")],
    });
    const { watchdog, gaveUp } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "recovering");
    assert.equal(await watchdog.runOnce(), "recovering");
    assert.equal(await watchdog.runOnce(), "gave-up");
    assert.equal(gaveUp.length, 1, "giveUp fires exactly once");
    assert.match(gaveUp[0], /respawns/, "the reason explains that the gateway takes over");
});

test("operator-managed CDP is never escalated, only reported", async () => {
    // `browser.docker.cdp_url` means the operator owns that Chrome: exiting
    // would crash-loop against the reconciler while fixing nothing.
    const healer = fakeHealer({
        checks: [new Error("ECONNREFUSED")],
        recoveries: [null],
        canEscalate: false,
    });
    const { watchdog, phase, gaveUp } = makeWatchdog(healer);

    for (let i = 0; i < 5; i += 1) {
        assert.equal(await watchdog.runOnce(), "recovering");
    }
    assert.equal(gaveUp.length, 0, "never exits — it waits for the operator's Chrome to return");
    assert.equal(phase.phase, "recovering");
});

test("an operator-managed browser that comes back is picked up", async () => {
    const healer = fakeHealer({
        checks: [new Error("ECONNREFUSED")],
        recoveries: [null],
        canEscalate: false,
    });
    const { watchdog, phase } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "recovering");
    // The operator restarts their Chrome: every subsequent probe passes.
    healer.check = () => Promise.resolve();
    assert.equal(await watchdog.runOnce(), "recovered");
    assert.equal(phase.phase, "ready");
});

test("stands down while a tool call is in flight", async () => {
    // CDDM serialises tool calls behind one mutex, so a probe fired mid-call
    // would queue behind it and then time out against a healthy browser.
    const activity = idleActivity();
    activity.begin();
    const healer = fakeHealer({ checks: [new Error("would have failed")] });
    const { watchdog, phase } = makeWatchdog(healer, { activity });

    assert.equal(await watchdog.runOnce(), "skipped");
    assert.equal(healer.calls.check, 0, "no probe while the browser is demonstrably in use");
    assert.equal(phase.phase, "ready");
});

test("stands down when traffic succeeded recently", async () => {
    const activity = new BrowserActivity();
    activity.begin();
    activity.end(true); // stamps "just now"
    const healer = fakeHealer({ checks: [new Error("would have failed")] });
    const { watchdog } = makeWatchdog(healer, { activity });

    assert.equal(await watchdog.runOnce(), "skipped");
    assert.equal(healer.calls.check, 0, "fresh traffic is better evidence than a probe");
});

test("failing traffic does NOT stand the watchdog down", async () => {
    // Regression: if a failed call counted as evidence of liveness, an agent
    // retrying every few seconds against a dead browser would suppress the
    // watchdog permanently — the exact situation it exists to fix.
    const activity = new BrowserActivity();
    activity.begin();
    activity.end(false);
    const healer = fakeHealer({
        checks: [new Error("Target closed"), new Error("Target closed"), null],
        recoveries: [null],
    });
    const { watchdog } = makeWatchdog(healer, { activity });

    assert.equal(await watchdog.runOnce(), "recovered");
    assert.ok(healer.calls.check > 0, "a failing call must not suppress the probe");
});

test("a failed call still arms the watchdog", async () => {
    // Even a failure means Chrome was launched, so there is now something to
    // watch — that is when supervision matters most.
    const activity = new BrowserActivity();
    assert.equal(activity.everCalled, false);
    activity.begin();
    activity.end(false);
    assert.equal(activity.everCalled, true);
});

test("stays idle until the healer is armed", async () => {
    // Host-headless launches Chrome on the first tool call; probing before
    // that would force a Chrome the operator may never use into memory.
    const healer = fakeHealer({ checks: [new Error("no browser yet")], armed: false });
    const { watchdog } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "skipped");
    assert.equal(healer.calls.check, 0);
});

test("stays out of the boot phases", async () => {
    // `installing` / `docker-*` / a terminal `failed` belong to main().
    const healer = fakeHealer({ checks: [new Error("no browser yet")] });
    const { watchdog } = makeWatchdog(healer, { phase: phaseSpy("installing") });

    assert.equal(await watchdog.runOnce(), "skipped");
    assert.equal(healer.calls.check, 0);
});

test("a hanging probe is failed rather than awaited forever", async () => {
    // The wedged-Chrome case: it never answers at all — no error, no close,
    // just silence. Without the deadline the watchdog would block on it, which
    // is precisely the failure it exists to catch.
    let wedged = true;
    const healer = {
        mode: "test",
        canEscalate: true,
        armed: () => true,
        outageText: (reason) => `outage: ${reason}`,
        check: (signal) =>
            wedged
                ? new Promise((_resolve, reject) => {
                      signal.addEventListener("abort", () => reject(new Error("aborted")));
                  })
                : Promise.resolve(),
        recover: () => {
            wedged = false;
            return Promise.resolve();
        },
    };
    const { watchdog, phase } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "recovered", "the hang is treated as death and repaired");
    assert.match(
        phase.reasons[0],
        /no response within/,
        "the timeout is reported as the diagnosis, not a generic abort error",
    );
});

test("a healer that throws does not kill the loop", async () => {
    const healer = fakeHealer({
        checks: [new Error("dead"), new Error("dead"), null],
        recoveries: [null],
    });
    healer.recover = () => {
        throw new Error("synchronous explosion");
    };
    const { watchdog, phase } = makeWatchdog(healer);

    assert.equal(await watchdog.runOnce(), "recovering");
    assert.equal(phase.phase, "recovering", "the throw is absorbed as a failed recovery");
});
