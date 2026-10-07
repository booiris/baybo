// The McpContext.from tap must never change what CDDM sees, and must refuse
// a CDDM it was not verified against.

import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import assert from "node:assert/strict";
import { test } from "node:test";

import {
    TAPPED_CDDM_VERSION,
    installCddmTap,
    tapContextFrom,
} from "../dist/test/cddm_tap.mjs";

const here = dirname(fileURLToPath(import.meta.url));

function quietLog() {
    const lines = [];
    const push = (level) => (msg) => lines.push(`${level}: ${msg}`);
    return {
        lines,
        info: push("info"),
        debug: push("debug"),
        warn: push("warn"),
        error: push("error"),
    };
}

const tick = () => new Promise((r) => setImmediate(r));

function fakeContextClass() {
    const ctx = { browser: {} };
    class Ctx {
        static async from(browser) {
            return { ...ctx, browser };
        }
    }
    return Ctx;
}

test("the pinned CDDM is the version the tap was verified against", () => {
    const pkg = JSON.parse(readFileSync(resolve(here, "..", "package.json"), "utf8"));
    assert.equal(
        pkg.dependencies["chrome-devtools-mcp"],
        TAPPED_CDDM_VERSION,
        "chrome-devtools-mcp was bumped: re-run the McpContext.from spike, then update TAPPED_CDDM_VERSION",
    );
});

test("the real CDDM exposes McpContext.from and the pinned VERSION", async () => {
    const ctxModule = await import("../dist/cddm/build/src/McpContext.js");
    const versionModule = await import("../dist/cddm/build/src/version.js");
    assert.equal(typeof ctxModule.McpContext.from, "function");
    assert.equal(versionModule.VERSION, TAPPED_CDDM_VERSION);
});

test("tap delivers the context and hands CDDM the very same promise", async () => {
    const Ctx = fakeContextClass();
    const seen = [];
    const original = Ctx.from;
    assert.deepEqual(tapContextFrom(Ctx, (c) => seen.push(c), quietLog()), { installed: true });
    assert.notEqual(Ctx.from, original);
    const p = Ctx.from("B1");
    assert.ok(p instanceof Promise);
    const ctx = await p;
    assert.deepEqual(ctx, { browser: "B1" });
    await tick();
    assert.deepEqual(seen, [ctx]);
});

test("a throwing callback cannot change CDDM's result", async () => {
    const Ctx = fakeContextClass();
    const log = quietLog();
    tapContextFrom(
        Ctx,
        () => {
            throw new Error("viewer bug");
        },
        log,
    );
    const ctx = await Ctx.from("B2");
    assert.deepEqual(ctx, { browser: "B2" });
    await tick();
    assert.ok(log.lines.some((l) => l.includes("viewer bug")), log.lines.join("\n"));
});

test("CDDM's own rejection still reaches CDDM, untouched and handled once", async () => {
    const boom = new Error("launch failed");
    const Ctx = {
        from: () => Promise.reject(boom),
    };
    const seen = [];
    tapContextFrom(Ctx, (c) => seen.push(c), quietLog());
    await assert.rejects(Ctx.from(), (e) => e === boom);
    await tick();
    assert.deepEqual(seen, []);
});

test("tap is idempotent", async () => {
    const Ctx = fakeContextClass();
    const seen = [];
    tapContextFrom(Ctx, () => seen.push("first"), quietLog());
    const once = Ctx.from;
    assert.deepEqual(tapContextFrom(Ctx, () => seen.push("second"), quietLog()), {
        installed: true,
    });
    assert.equal(Ctx.from, once);
    await Ctx.from("B");
    await tick();
    assert.deepEqual(seen, ["first"]);
});

test("missing McpContext.from reports a version mismatch", () => {
    for (const holder of [undefined, null, {}, { from: 42 }, "x"]) {
        assert.deepEqual(tapContextFrom(holder, () => {}, quietLog()), {
            installed: false,
            reason: "cddm_version_mismatch",
        });
    }
});

test("installCddmTap refuses an unverified CDDM version without patching it", async () => {
    const Ctx = fakeContextClass();
    const original = Ctx.from;
    const outcome = await installCddmTap(() => {}, quietLog(), async () => ({
        mcpContext: Ctx,
        version: "9.9.9",
    }));
    assert.deepEqual(outcome, { installed: false, reason: "cddm_version_mismatch" });
    assert.equal(Ctx.from, original);
});

test("installCddmTap survives CDDM internals that fail to load", async () => {
    const outcome = await installCddmTap(() => {}, quietLog(), async () => {
        throw new Error("Cannot find module McpContext.js");
    });
    assert.deepEqual(outcome, { installed: false, reason: "cddm_version_mismatch" });
});

test("installCddmTap reports a mismatch when the verified version lost `from`", async () => {
    const outcome = await installCddmTap(() => {}, quietLog(), async () => ({
        mcpContext: {},
        version: TAPPED_CDDM_VERSION,
    }));
    assert.deepEqual(outcome, { installed: false, reason: "cddm_version_mismatch" });
});

test("installCddmTap installs on a matching CDDM", async () => {
    const Ctx = fakeContextClass();
    const seen = [];
    const outcome = await installCddmTap((c) => seen.push(c), quietLog(), async () => ({
        mcpContext: Ctx,
        version: TAPPED_CDDM_VERSION,
    }));
    assert.deepEqual(outcome, { installed: true });
    await Ctx.from("B3");
    await tick();
    assert.equal(seen.length, 1);
});
