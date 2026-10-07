// End to end: the built bundle as a child process, a unix socket this test
// listens on standing in for the gateway, and MCP over the child's stdio.
//
// - The handshake test needs no browser and always runs.
// - The screencast test drives a REAL Chrome and self-skips when none is
//   cached (the Chrome for Testing that `pnpm install-chrome` /
//   findExistingChrome put under $XDG_CACHE_HOME/baybo/browser/chrome).

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import assert from "node:assert/strict";
import { test } from "node:test";

import { Browser, getInstalledBrowsers } from "@puppeteer/browsers";

const here = dirname(fileURLToPath(import.meta.url));
const bundlePath = resolve(here, "..", "dist", "bundle.mjs");
if (!existsSync(bundlePath)) {
    throw new Error(`bundle missing at ${bundlePath}; run \`node esbuild.config.mjs\` first`);
}

const SECRET = "e2e-link-secret-7c1f";
const HELLO_ACK = {
    type: "hello_ack",
    protocol: 1,
    limits: {
        max_frame_bytes: 2_097_152,
        max_json_bytes: 262_144,
        max_targets: 32,
        max_target_id_chars: 64,
        max_target_url_chars: 512,
        max_text_chars: 256,
    },
};
const FRAME_HEADER_KEYS = [
    "browser_gen",
    "captured_at_ms",
    "device_height",
    "device_width",
    "offset_top",
    "page_scale_factor",
    "scroll_offset_x",
    "scroll_offset_y",
    "seq",
    "target_id",
];
const ANIMATED_PAGE =
    "data:text/html,<title>e2e-cast</title><body><script>let i=0;setInterval(()=>{" +
    "document.body.style.background='hsl('+((i+=7)%25360)+',60%25,60%25)'},16)</script>";

async function cachedChrome() {
    const xdg = process.env.XDG_CACHE_HOME;
    const cacheDir = join(xdg && xdg.length > 0 ? xdg : join(homedir(), ".cache"), "baybo", "browser", "chrome");
    if (!existsSync(cacheDir)) return null;
    try {
        const installed = await getInstalledBrowsers({ cacheDir });
        const chrome = installed.find((b) => b.browser === Browser.CHROME);
        return chrome && existsSync(chrome.executablePath) ? chrome.executablePath : null;
    } catch {
        return null;
    }
}

/** The fake gateway: one unix socket, the link framing, a message queue. */
async function fakeGateway(dir) {
    const socketPath = join(dir, "link.sock");
    const messages = [];
    const waiters = [];
    let conn = null;
    let buf = Buffer.alloc(0);
    const deliver = () => {
        for (const w of [...waiters]) {
            const idx = messages.findIndex(w.pred);
            if (idx === -1) continue;
            waiters.splice(waiters.indexOf(w), 1);
            w.resolve(messages[idx]);
        }
    };
    const server = createServer((socket) => {
        conn = socket;
        socket.on("data", (chunk) => {
            buf = Buffer.concat([buf, chunk]);
            while (buf.length >= 5) {
                const len = buf.readUInt32BE(0);
                if (buf.length < 4 + len) break;
                const kind = buf.readUInt8(4);
                const body = buf.subarray(5, 4 + len);
                buf = buf.subarray(4 + len);
                if (kind === 0) {
                    messages.push({ json: JSON.parse(body.toString("utf8")) });
                } else {
                    const hdrLen = body.readUInt32BE(0);
                    messages.push({
                        header: JSON.parse(body.subarray(4, 4 + hdrLen).toString("utf8")),
                        jpeg: Buffer.from(body.subarray(4 + hdrLen)),
                    });
                }
            }
            deliver();
        });
        socket.on("error", () => {});
    });
    await new Promise((r) => server.listen(socketPath, r));
    return {
        socketPath,
        messages,
        send(obj) {
            const body = Buffer.from(JSON.stringify(obj));
            const out = Buffer.alloc(5 + body.length);
            out.writeUInt32BE(1 + body.length, 0);
            out.writeUInt8(0, 4);
            body.copy(out, 5);
            conn.write(out);
        },
        waitFor(pred, what, timeoutMs = 20_000) {
            const hit = messages.find(pred);
            if (hit) return Promise.resolve(hit);
            return new Promise((resolve, reject) => {
                const w = { pred, resolve };
                waiters.push(w);
                setTimeout(() => {
                    const i = waiters.indexOf(w);
                    if (i !== -1) {
                        waiters.splice(i, 1);
                        reject(new Error(`timed out waiting for ${what}`));
                    }
                }, timeoutMs).unref();
            });
        },
        close() {
            conn?.destroy();
            return new Promise((r) => server.close(() => r()));
        },
    };
}

function startSidecar(env) {
    const child = spawn(process.execPath, [bundlePath], {
        stdio: ["pipe", "pipe", "pipe"],
        env: {
            ...process.env,
            BAYBO_BROWSER_DOCKER_ENABLE: "",
            BAYBO_BROWSER_DOCKER_CDP_URL: "",
            BAYBO_BROWSER_NO_SANDBOX: "1",
            ...env,
        },
    });
    let stderr = "";
    child.stderr.on("data", (c) => {
        stderr += c.toString("utf8");
    });
    let out = "";
    let nextId = 1;
    const pending = new Map();
    child.stdout.on("data", (chunk) => {
        out += chunk.toString("utf8");
        let nl;
        while ((nl = out.indexOf("\n")) !== -1) {
            const line = out.slice(0, nl).trim();
            out = out.slice(nl + 1);
            if (!line) continue;
            const msg = JSON.parse(line);
            const p = pending.get(msg.id);
            if (p) {
                pending.delete(msg.id);
                p(msg);
            }
        }
    });
    const request = (method, params) =>
        new Promise((resolve, reject) => {
            const id = nextId++;
            const t = setTimeout(() => reject(new Error(`no reply to ${method}`)), 60_000);
            pending.set(id, (m) => {
                clearTimeout(t);
                resolve(m);
            });
            child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
        });
    const init = async () => {
        await request("initialize", {
            protocolVersion: "2024-11-05",
            capabilities: {},
            clientInfo: { name: "baybo-e2e-screencast", version: "0.0.1" },
        });
        child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
    };
    const stop = () =>
        new Promise((resolve) => {
            if (child.exitCode !== null || child.signalCode !== null) return resolve();
            const t = setTimeout(() => child.kill("SIGKILL"), 30_000);
            child.once("exit", () => {
                clearTimeout(t);
                resolve();
            });
            child.kill("SIGTERM");
        });
    return { child, request, init, stop, stderr: () => stderr };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const text = (res) => (res.result?.content ?? []).map((c) => c.text ?? "").join("\n");

async function callUntilReady(sidecar, name, args, deadlineMs = 60_000) {
    const end = Date.now() + deadlineMs;
    for (;;) {
        const res = await sidecar.request("tools/call", { name, arguments: args });
        const body = text(res);
        const parked = res.result?.isError === true && /retry this tool call|starting|installing/i.test(body);
        if (!parked) return res;
        if (Date.now() > end) throw new Error(`${name} never became ready: ${body}`);
        await sleep(250);
    }
}

test("link handshake and lazy launch, without a browser", async (t) => {
    const dir = mkdtempSync(join(tmpdir(), "baybo-cast-e2e-"));
    const gw = await fakeGateway(dir);
    const sidecar = startSidecar({
        BAYBO_BROWSER_LINK_SOCKET: gw.socketPath,
        BAYBO_BROWSER_LINK_SECRET: SECRET,
        // Never launched: no tool call reaches CDDM in this test.
        BAYBO_BROWSER_CHROME_PATH: process.execPath,
        BAYBO_BROWSER_PROFILE_DIR: join(dir, "profile"),
    });
    t.after(async () => {
        await sidecar.stop();
        await gw.close();
        rmSync(dir, { recursive: true, force: true });
    });

    const hello = await gw.waitFor((m) => m.json?.type === "hello", "Hello");
    assert.equal(hello.json.secret, SECRET);
    assert.equal(hello.json.protocol, 1);
    assert.equal(hello.json.pid, sidecar.child.pid);
    assert.deepEqual(hello.json.capabilities, ["screencast"]);
    gw.send(HELLO_ACK);

    await gw.waitFor(
        (m) => m.json?.type === "availability" && m.json.unavailable === null,
        "Availability{null} (the tap installed on the pinned CDDM)",
    );
    const status = await gw.waitFor((m) => m.json?.type === "status", "Status");
    assert.deepEqual(status.json, {
        type: "status",
        mode: "host",
        phase: { type: "idle" },
        browser_gen: 0,
    });

    gw.send({ type: "start_screencast" });
    const stream = await gw.waitFor((m) => m.json?.type === "stream", "Stream");
    assert.deepEqual(stream.json, { type: "stream", state: "idle", browser_gen: 0, target: null });

    // A viewer must not launch Chrome: tools/list is all there is, and the
    // boot summary still says deferred.
    await sidecar.init();
    const list = await sidecar.request("tools/list", {});
    const names = list.result.tools.map((x) => x.name);
    assert.ok(names.includes("list_pages"));
    assert.ok(
        !names.some((n) => n.startsWith("screencast")),
        `CDDM's experimental screencast tools must stay off: ${names.join(",")}`,
    );
    assert.doesNotMatch(sidecar.stderr(), /chrome-devtools-mcp ready: mode=/);
    assert.ok(!sidecar.stderr().includes(SECRET), "the secret never reaches the log");
});

test("real Chrome: frames, targets, stop, and CDDM's page list is undisturbed", async (t) => {
    const chrome = await cachedChrome();
    if (chrome === null) {
        t.skip("no cached Chrome for Testing (run `pnpm install-chrome`)");
        return;
    }
    const dir = mkdtempSync(join(tmpdir(), "baybo-cast-e2e-"));
    const profileDir = join(dir, "profile");
    const gw = await fakeGateway(dir);
    const sidecar = startSidecar({
        BAYBO_BROWSER_LINK_SOCKET: gw.socketPath,
        BAYBO_BROWSER_LINK_SECRET: SECRET,
        BAYBO_BROWSER_CHROME_PATH: chrome,
        BAYBO_BROWSER_PROFILE_DIR: profileDir,
        BAYBO_BROWSER_VIEWPORT: "1280x800",
    });
    t.after(async () => {
        await sidecar.stop();
        await gw.close();
        rmSync(dir, { recursive: true, force: true });
    });

    await gw.waitFor((m) => m.json?.type === "hello", "Hello");
    gw.send(HELLO_ACK);
    await sidecar.init();

    const opened = await callUntilReady(sidecar, "new_page", { url: ANIMATED_PAGE });
    assert.notEqual(opened.result.isError, true, text(opened));
    const status = await gw.waitFor(
        (m) => m.json?.type === "status" && m.json.browser_gen >= 1 && m.json.phase.type === "ready",
        "Status for the launched browser",
    );
    assert.equal(status.json.mode, "host");

    const pagesBefore = text(await sidecar.request("tools/call", { name: "list_pages", arguments: {} }));

    gw.send({ type: "start_screencast" });
    const live = await gw.waitFor(
        (m) => m.json?.type === "stream" && m.json.state === "live",
        "Stream live",
    );
    assert.match(live.json.target.url, /^data:text\/html/);
    const frame = await gw.waitFor((m) => m.header !== undefined, "a frame");
    assert.deepEqual(Object.keys(frame.header).sort(), FRAME_HEADER_KEYS);
    assert.equal(frame.header.target_id, live.json.target.target_id);
    assert.equal(frame.header.browser_gen, live.json.browser_gen);
    assert.equal(frame.jpeg[0], 0xff);
    assert.equal(frame.jpeg[1], 0xd8);
    assert.equal(frame.jpeg.at(-2), 0xff);
    assert.equal(frame.jpeg.at(-1), 0xd9);
    assert.ok(frame.header.device_width > 0 && frame.header.device_height > 0);

    const targets = await gw.waitFor(
        (m) =>
            m.json?.type === "targets" &&
            m.json.followed === live.json.target.target_id &&
            m.json.targets.some((x) => x.target_id === live.json.target.target_id),
        "Targets listing the followed page",
    );
    assert.ok(targets.json.targets.every((x) => !x.url.startsWith("chrome://")));

    // Host mode caps at 5 fps; give it a second of an animating page.
    const countFrames = () => gw.messages.filter((m) => m.header !== undefined).length;
    const n0 = countFrames();
    await sleep(1_000);
    const perSecond = countFrames() - n0;
    assert.ok(perSecond >= 1 && perSecond <= 7, `${perSecond} frames/s`);

    const pagesDuring = text(await sidecar.request("tools/call", { name: "list_pages", arguments: {} }));
    assert.equal(pagesDuring, pagesBefore, "the screencast session must not add or rename pages");

    gw.send({ type: "stop_screencast" });
    await sleep(500);
    const afterStop = countFrames();
    await sleep(1_000);
    assert.equal(countFrames(), afterStop, "frames keep coming after StopScreencast");

    const pagesAfter = text(await sidecar.request("tools/call", { name: "list_pages", arguments: {} }));
    assert.equal(pagesAfter, pagesBefore);
});
