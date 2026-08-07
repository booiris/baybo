// Boots dist/bundle.mjs, sends MCP initialize + tools/list over stdio,
// and checks that experimentalPageIdRouting is wired through:
//   1. CDDM page-scoped tools (e.g. navigate_page) expose `pageId`
//   2. CDDM non-page-scoped tools (e.g. list_pages) do NOT
//   3. our injected `read_page` tool exposes its own `pageId`
//
// Why this is a black-box stdio test rather than a unit import:
// `dist/bundle.mjs` is the sidecar entrypoint, not a library — it has
// no exports we can reach into. The actual flag plumbing lives in
// CDDM's `registerTool` (build/src/index.js), so the only way to
// confirm the schema flip happened is to ask the running server for
// its tool list. CDDM registers tools synchronously without launching
// Chrome (browser launch is deferred to first `tools/call`), so the
// test stays fast (~200ms) and Chrome-free.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import assert from "node:assert/strict";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const bundlePath = resolve(here, "..", "dist", "bundle.mjs");

if (!existsSync(bundlePath)) {
    throw new Error(
        `bundle missing at ${bundlePath} — package.json's test script runs ` +
            `\`node esbuild.config.mjs\` before tests; rebuild and retry.`,
    );
}

function startSidecar() {
    // Pin "Chrome" to the running node binary so findExistingChrome
    // returns synchronously and skips the @puppeteer/browsers install
    // path entirely. We never actually launch this binary as Chrome —
    // tools/list in CDDM doesn't need a live browser.
    const profileDir = mkdtempSync(join(tmpdir(), "baybo-browser-test-"));
    const child = spawn(process.execPath, [bundlePath], {
        stdio: ["pipe", "pipe", "pipe"],
        env: {
            ...process.env,
            BAYBO_BROWSER_CHROME_PATH: process.execPath,
            BAYBO_BROWSER_PROFILE_DIR: profileDir,
            // Don't let the test inherit the operator's docker config;
            // the host-headless fast path is what we're exercising.
            BAYBO_BROWSER_DOCKER_ENABLE: "",
            BAYBO_BROWSER_DOCKER_CDP_URL: "",
        },
    });
    let stderrBuf = "";
    child.stderr.on("data", (chunk) => {
        // Sidecar logs to stderr; drain so the pipe doesn't fill up. The
        // boot summary is the only place several load-bearing CDDM flags
        // are observable without a live browser, so keep it.
        stderrBuf += chunk.toString("utf8");
    });

    let stdoutBuf = "";
    const queued = [];
    let pending = null;
    child.stdout.on("data", (chunk) => {
        stdoutBuf += chunk.toString("utf8");
        let nl;
        while ((nl = stdoutBuf.indexOf("\n")) !== -1) {
            const line = stdoutBuf.slice(0, nl).trim();
            stdoutBuf = stdoutBuf.slice(nl + 1);
            if (!line) continue;
            const msg = JSON.parse(line);
            if (pending) {
                const r = pending;
                pending = null;
                r(msg);
            } else {
                queued.push(msg);
            }
        }
    });

    function send(msg) {
        child.stdin.write(JSON.stringify(msg) + "\n");
    }
    function nextResponse() {
        return new Promise((resolve, reject) => {
            const t = setTimeout(
                () => reject(new Error("timed out waiting for sidecar response")),
                10_000,
            );
            const wrap = (m) => {
                clearTimeout(t);
                resolve(m);
            };
            if (queued.length > 0) wrap(queued.shift());
            else pending = wrap;
        });
    }

    return { child, send, nextResponse, stderr: () => stderrBuf };
}

// Production cleanup may use its full 25-second shutdown deadline.
const SIDECAR_EXIT_TIMEOUT_MS = 30_000;

function waitForExit(child, timeoutMs = SIDECAR_EXIT_TIMEOUT_MS) {
    return new Promise((resolve, reject) => {
        const timeout = setTimeout(
            () => reject(new Error("timed out waiting for sidecar exit")),
            timeoutMs,
        );
        child.once("exit", (code, signal) => {
            clearTimeout(timeout);
            resolve({ code, signal });
        });
    });
}

test("experimentalPageIdRouting is on; pageId surfaces on the right tools", async (t) => {
    const { child, send, nextResponse } = startSidecar();
    t.after(() => {
        child.kill("SIGTERM");
    });

    send({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
            protocolVersion: "2024-11-05",
            capabilities: {},
            clientInfo: { name: "baybo-browser-test", version: "0.0.1" },
        },
    });
    const initResp = await nextResponse();
    assert.equal(initResp.id, 1);
    assert.ok(initResp.result, "initialize returned a result");

    send({ jsonrpc: "2.0", method: "notifications/initialized" });
    send({ jsonrpc: "2.0", id: 2, method: "tools/list" });
    const listResp = await nextResponse();
    assert.equal(listResp.id, 2);
    const tools = listResp.result.tools;

    function get(name) {
        const t = tools.find((x) => x.name === name);
        assert.ok(t, `${name} present in tools/list`);
        return t;
    }

    // Page-scoped CDDM tool: the routing flag injects pageId into its schema.
    const navigate = get("navigate_page");
    assert.ok(
        navigate.inputSchema?.properties?.pageId,
        "navigate_page exposes pageId (experimentalPageIdRouting=on)",
    );

    // Non-page-scoped CDDM tool: pageId must NOT be added.
    const listPages = get("list_pages");
    assert.equal(
        listPages.inputSchema?.properties?.pageId,
        undefined,
        "list_pages stays pageId-free (it's not pageScoped — tool reflects all open pages)",
    );

    // CDDM's evaluate_script is non-page-scoped but its handler still
    // resolves a page; the tool itself adds pageId conditionally
    // (build/src/tools/script.js — when experimentalPageIdRouting is
    // on the schema gets pageIdSchema spread in). Required-ness is
    // not enforced at the schema layer (it stays optional), but its
    // *handler* throws "No page found" without one — that's the
    // documented quirk our read_page wrapper handles.
    const evalScript = get("evaluate_script");
    assert.ok(
        evalScript.inputSchema?.properties?.pageId,
        "evaluate_script exposes pageId (script.js spreads pageIdSchema when routing is on)",
    );

    // Our injected tool: schema must carry the new pageId field, and
    // since we proxy to evaluate_script (which throws without a pageId),
    // it must also be marked required at the schema layer so the model
    // doesn't omit it.
    const readPage = get("read_page");
    assert.ok(
        readPage.inputSchema?.properties?.pageId,
        "read_page exposes pageId for multi-agent routing",
    );
    assert.equal(
        readPage.inputSchema.properties.pageId.type,
        "number",
        "read_page.pageId typed as number to match CDDM's pageId convention",
    );
    assert.deepEqual(
        readPage.inputSchema.required,
        ["pageId"],
        "read_page.pageId is required (evaluate_script handler has no selected-page fallback)",
    );
});

// The tab cap is fully dependent on `experimentalStructuredContent`:
// CDDM's ToolHandler attaches `structuredContent` only when that arg is
// set, and the flag is off by default when CDDM is driven
// programmatically (only its own CLI passes it). Without it,
// `listOpenPages` throws on every call, `makeRoomForOne` swallows that
// and returns "nothing evicted", and the cap silently becomes a no-op —
// the exact leak it was added to fix, with every unit test still green
// because they inject fakes. The boot summary is the cheapest place to
// observe the real value without launching Chrome.
test("the CDDM flags the tab cap depends on are actually passed", async (t) => {
    const { child, send, nextResponse, stderr } = startSidecar();
    t.after(() => {
        child.kill("SIGTERM");
    });

    send({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
            protocolVersion: "2024-11-05",
            capabilities: {},
            clientInfo: { name: "baybo-browser-test", version: "0.0.1" },
        },
    });
    await nextResponse();
    send({ jsonrpc: "2.0", method: "notifications/initialized" });
    // tools/list round-trips after the boot summary is emitted, so its
    // response is the signal that stderr has the line.
    send({ jsonrpc: "2.0", id: 2, method: "tools/list" });
    await nextResponse();

    const log = stderr();
    assert.match(
        log,
        /structured_content=on/,
        "experimentalStructuredContent must be passed to CDDM or the page budget can never read a page list",
    );
    assert.match(log, /page_id_routing=on/);
    assert.match(log, /max_pages=\d+/, "the effective tab cap is reported at boot");
});

test("closed parent pipes shut down the sidecar without orphaning it", async (t) => {
    const { child, send, nextResponse } = startSidecar();
    t.after(() => {
        if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    });

    send({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
            protocolVersion: "2024-11-05",
            capabilities: {},
            clientInfo: { name: "baybo-browser-test", version: "0.0.1" },
        },
    });
    await nextResponse();
    send({ jsonrpc: "2.0", method: "notifications/initialized" });
    send({ jsonrpc: "2.0", id: 2, method: "tools/list" });
    await nextResponse();

    const exited = waitForExit(child);
    child.stdout.destroy();
    child.stderr.destroy();
    child.stdin.end();
    assert.deepEqual(await exited, { code: 0, signal: null });
});


// Pins the upstream behaviour the flag above exists for. CDDM is vendored
// and its internals are not a contract, so this reads the shipped source
// rather than driving its classes — a constructor signature change should
// not fail this test, but silently making `structuredContent`
// unconditional (or renaming the arg) should, because then our flag has
// stopped being what makes the tab cap work.
test("CDDM still gates structuredContent on the arg the sidecar passes", async () => {
    const handlerSrc = resolve(
        here,
        "..",
        "node_modules",
        "chrome-devtools-mcp",
        "build",
        "src",
        "ToolHandler.js",
    );
    const src = await readFile(handlerSrc, "utf8");
    assert.match(
        src,
        /if\s*\(\s*this\.serverArgs\.experimentalStructuredContent\s*\)/,
        "CDDM attaches structuredContent only under this guard; if it changed, re-check whether " +
            "the sidecar still needs experimentalStructuredContent for the page budget to see a page list",
    );
});
