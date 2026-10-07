// ScreencastHub against fake puppeteer / CDDM ports and a fake clock.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
    CHROME_SCREENCAST_FPS,
    DEFAULT_FPS,
    HOST_FPS,
    ScreencastHub,
    clampText,
    toTargetInfo,
} from "../dist/test/screencast.mjs";

// `LinkDown::hello_ack()` limits, as asserted in crates/browser-view/src/wire.rs.
const LIMITS = {
    max_frame_bytes: 2_097_152,
    max_json_bytes: 262_144,
    max_targets: 32,
    max_target_id_chars: 64,
    max_target_url_chars: 512,
    max_text_chars: 256,
};

const NOOP_LOG = { info() {}, debug() {}, warn() {}, error() {} };
const JPEG_B64 = Buffer.from([0xff, 0xd8, 0xff, 0xd9]).toString("base64");

class Emitter {
    #listeners = new Map();
    on(event, fn) {
        if (!this.#listeners.has(event)) this.#listeners.set(event, new Set());
        this.#listeners.get(event).add(fn);
        return this;
    }
    off(event, fn) {
        this.#listeners.get(event)?.delete(fn);
        return this;
    }
    emit(event, payload) {
        for (const fn of [...(this.#listeners.get(event) ?? [])]) fn(payload);
    }
    listenerCount(event) {
        return this.#listeners.get(event)?.size ?? 0;
    }
}

class FakeSession extends Emitter {
    sent = [];
    detached = false;
    async send(method, params) {
        this.sent.push({ method, params });
        return {};
    }
    async detach() {
        this.detached = true;
    }
    count(method) {
        return this.sent.filter((s) => s.method === method).length;
    }
    frame(sessionId, metadata = {}) {
        this.emit("Page.screencastFrame", {
            data: JPEG_B64,
            sessionId,
            metadata: {
                offsetTop: 0,
                pageScaleFactor: 1,
                deviceWidth: 1280,
                deviceHeight: 800,
                scrollOffsetX: 0,
                scrollOffsetY: 0,
                timestamp: 1_700_000_000.5,
                ...metadata,
            },
        });
    }
}

function fakeTarget(id, url, title = "", type = "page") {
    return {
        type: () => type,
        url: () => url,
        _getTargetInfo: () => ({ targetId: id, url, title, type }),
    };
}

function fakePage(id, url) {
    const target = fakeTarget(id, url, `title ${id}`);
    const page = {
        closed: false,
        sessions: [],
        isClosed: () => page.closed,
        target: () => target,
        async createCDPSession() {
            const s = new FakeSession();
            page.sessions.push(s);
            return s;
        },
    };
    return page;
}

function fakeContext(pages, extraTargets = []) {
    const browser = new Emitter();
    browser.targets = () => [...pages.map((p) => p.target()), ...extraTargets];
    const ctx = {
        browser,
        selected: pages[0],
        getPageById(id) {
            const p = pages[id - 1];
            if (!p) throw new Error("No page found");
            return { pptrPage: p };
        },
        getSelectedPptrPage() {
            if (!ctx.selected) throw new Error("No page selected");
            return ctx.selected;
        },
    };
    return ctx;
}

function fakeClock() {
    let now = 0;
    let nextId = 1;
    const timers = new Map();
    return {
        now: () => now,
        setTimeout(fn, ms) {
            const id = nextId++;
            timers.set(id, { at: now + ms, fn });
            return id;
        },
        clearTimeout(id) {
            timers.delete(id);
        },
        advance(ms) {
            const target = now + ms;
            for (;;) {
                const due = [...timers.entries()]
                    .filter(([, t]) => t.at <= target)
                    .sort((a, b) => a[1].at - b[1].at)[0];
                if (!due) break;
                timers.delete(due[0]);
                now = due[1].at;
                due[1].fn();
            }
            now = target;
        },
    };
}

function fakeSink() {
    const sink = { status: [], targets: [], stream: [], availability: [], frames: [] };
    return {
        sink,
        port: {
            status: (m) => sink.status.push(m),
            targets: (m) => sink.targets.push(m),
            stream: (m) => sink.stream.push(m),
            availability: (m) => sink.availability.push(m),
            frame: (header, jpeg) => sink.frames.push({ header, jpeg }),
        },
    };
}

function newHub({ mode = "docker" } = {}) {
    const clock = fakeClock();
    const { sink, port } = fakeSink();
    const hub = new ScreencastHub({ sink: port, log: NOOP_LOG, mode, clock });
    hub.linkUp(LIMITS);
    return { hub, clock, sink };
}

const flush = async () => {
    for (let i = 0; i < 5; i++) await new Promise((r) => setImmediate(r));
};
const last = (xs) => xs[xs.length - 1];

test("before any browser exists the stream is idle and nothing is launched", async () => {
    const { hub, sink } = newHub();
    await hub.start();
    assert.deepEqual(last(sink.stream), { state: "idle", browser_gen: 0, target: null });
    assert.deepEqual(last(sink.status), {
        mode: "docker",
        phase: { type: "idle" },
        browser_gen: 0,
    });
    assert.deepEqual(sink.availability, [null]);
});

test("no CDP session is opened while nobody watches", async () => {
    const { hub, sink, clock } = newHub();
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    clock.advance(1000);
    assert.equal(page.sessions.length, 0);
    assert.equal(last(sink.status).browser_gen, 1);
    assert.deepEqual(last(sink.targets), {
        browser_gen: 1,
        targets: [{ target_id: "T1", url: "https://a.example/", title: "title T1" }],
        followed: "T1",
    });
});

test("start and stop are idempotent", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    await hub.start();
    assert.equal(page.sessions.length, 1);
    const s = page.sessions[0];
    assert.equal(s.count("Page.startScreencast"), 1);
    assert.deepEqual(s.sent[0].params, {
        format: "jpeg",
        quality: 60,
        maxWidth: 1280,
        maxHeight: 1280,
        everyNthFrame: CHROME_SCREENCAST_FPS / DEFAULT_FPS,
    });
    assert.deepEqual(last(sink.stream), {
        state: "live",
        browser_gen: 1,
        target: { target_id: "T1", url: "https://a.example/", title: "title T1" },
    });
    await hub.stop();
    await hub.stop();
    await flush();
    assert.equal(s.count("Page.stopScreencast"), 1);
    assert.equal(s.detached, true);
    assert.equal(s.listenerCount("Page.screencastFrame"), 0);
    await hub.start();
    assert.equal(page.sessions.length, 2);
});

test("every frame is acked at once but only fps frames per second are sent", async () => {
    const { hub, sink, clock } = newHub({ mode: "docker" });
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    const s = page.sessions[0];
    const interval = 1000 / DEFAULT_FPS;
    // 30 fps for one second.
    for (let i = 0; i < 30; i++) {
        s.frame(i + 1, { scrollOffsetY: i });
        clock.advance(1000 / 30);
    }
    clock.advance(interval);
    assert.equal(s.count("Page.screencastFrameAck"), 30);
    assert.ok(
        sink.frames.length >= DEFAULT_FPS && sink.frames.length <= DEFAULT_FPS + 1,
        `${sink.frames.length} frames`,
    );
    // The trailing slot carries the newest frame, so a page that goes still
    // ends on its final state.
    assert.equal(last(sink.frames).header.scroll_offset_y, 29);
    const seqs = sink.frames.map((f) => f.header.seq);
    assert.deepEqual(
        seqs,
        seqs.map((_, i) => i),
    );
});

test("host mode streams at the host fps", async () => {
    const { hub, sink, clock } = newHub({ mode: "host" });
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    const s = page.sessions[0];
    for (let i = 0; i < 60; i++) {
        s.frame(i + 1);
        clock.advance(1000 / 60);
    }
    assert.ok(
        sink.frames.length >= HOST_FPS && sink.frames.length <= HOST_FPS + 1,
        `${sink.frames.length} frames`,
    );
});

test("frame header carries the FrameHeader fields only", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    page.sessions[0].frame(1, { scrollOffsetY: 240 });
    assert.equal(sink.frames.length, 1);
    const { header, jpeg } = sink.frames[0];
    assert.deepEqual(header, {
        target_id: "T1",
        browser_gen: 1,
        seq: 0,
        captured_at_ms: 1_700_000_000_500,
        device_width: 1280,
        device_height: 800,
        offset_top: 0,
        page_scale_factor: 1,
        scroll_offset_x: 0,
        scroll_offset_y: 240,
    });
    assert.deepEqual([...jpeg], [0xff, 0xd8, 0xff, 0xd9]);
});

test("the stream follows the page the agent last used", async () => {
    const { hub, sink } = newHub();
    const p1 = fakePage("T1", "https://one.example/");
    const p2 = fakePage("T2", "https://two.example/");
    await hub.attach(fakeContext([p1, p2]));
    await hub.start();
    assert.equal(p1.sessions.length, 1);
    hub.noteAgentPage(2);
    await flush();
    assert.equal(p1.sessions[0].detached, true);
    assert.equal(p2.sessions.length, 1);
    assert.equal(last(sink.stream).target.target_id, "T2");
    // A late frame from the old session is ignored.
    const before = sink.frames.length;
    p1.sessions[0].frame(9);
    assert.equal(sink.frames.length, before);
    hub.noteAgentPage(2);
    await flush();
    assert.equal(p2.sessions.length, 1);
    // An unknown id falls back to CDDM's selected page.
    hub.noteAgentPage(7);
    await flush();
    assert.equal(p1.sessions.length, 2);
});

test("a closed followed page reports target_gone", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    const ctx = fakeContext([page]);
    await hub.attach(ctx);
    await hub.start();
    page.closed = true;
    ctx.browser.emit("targetdestroyed", page.target());
    await flush();
    assert.equal(page.sessions[0].detached, true);
    assert.deepEqual(last(sink.stream), { state: "target_gone", browser_gen: 1, target: null });
});

test("recovering pauses the stream and ready resumes it", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    await hub.setStatus("docker", { type: "recovering" });
    await flush();
    assert.equal(page.sessions[0].detached, true);
    assert.equal(last(sink.stream).state, "paused");
    assert.deepEqual(last(sink.status).phase, { type: "recovering" });
    await hub.setStatus("docker", { type: "ready" });
    assert.equal(page.sessions.length, 2);
    assert.equal(last(sink.stream).state, "live");
});

test("a replaced browser bumps browser_gen and drops the old session", async () => {
    const { hub, sink } = newHub();
    const old = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([old]));
    await hub.start();
    const fresh = fakePage("T9", "https://b.example/");
    await hub.attach(fakeContext([fresh]));
    await flush();
    assert.equal(old.sessions[0].detached, true);
    assert.equal(fresh.sessions.length, 1);
    assert.equal(last(sink.status).browser_gen, 2);
    assert.deepEqual(last(sink.stream), {
        state: "live",
        browser_gen: 2,
        target: { target_id: "T9", url: "https://b.example/", title: "title T9" },
    });
});

test("an unavailable viewer never opens a session", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    await hub.setAvailability("cddm_version_mismatch");
    await hub.attach(fakeContext([page]));
    await hub.start();
    assert.equal(page.sessions.length, 0);
    assert.deepEqual(last(sink.availability), "cddm_version_mismatch");
    assert.equal(last(sink.stream).state, "unavailable");
});

test("link down stops the stream and nothing is sent until the next link", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    await hub.linkDown();
    await flush();
    assert.equal(page.sessions[0].detached, true);
    const counts = [sink.status.length, sink.targets.length, sink.stream.length];
    await hub.setStatus("docker", { type: "failed", error: "x" });
    assert.deepEqual([sink.status.length, sink.targets.length, sink.stream.length], counts);
    hub.linkUp(LIMITS);
    assert.deepEqual(last(sink.status).phase, { type: "failed", error: "x" });
});

test("failed error text is clamped to max_text_chars", async () => {
    const { hub, sink } = newHub();
    await hub.setStatus("host", { type: "failed", error: "e".repeat(1000) });
    assert.equal(last(sink.status).phase.error.length, LIMITS.max_text_chars);
});

test("targets: pages only, internal URLs hidden, clamped to the announced limits", () => {
    const limits = { ...LIMITS, max_target_id_chars: 8, max_target_url_chars: 20, max_text_chars: 5 };
    assert.equal(toTargetInfo(fakeTarget("T1", "chrome://newtab/"), limits), null);
    assert.equal(toTargetInfo(fakeTarget("T1", "devtools://devtools/x"), limits), null);
    assert.equal(toTargetInfo(fakeTarget("T1", "https://a/", "", "service_worker"), limits), null);
    assert.equal(toTargetInfo(fakeTarget("T".repeat(9), "https://a/"), limits), null);
    assert.deepEqual(
        toTargetInfo(fakeTarget("T1", `https://a.example/${"x".repeat(50)}`, "long title"), limits),
        { target_id: "T1", url: "https://a.example/xx", title: "long " },
    );
    assert.deepEqual(toTargetInfo(fakeTarget("T1", "about:blank"), limits), {
        target_id: "T1",
        url: "about:blank",
        title: "",
    });
});

test("targets list is capped at max_targets", async () => {
    const clock = fakeClock();
    const { sink, port } = fakeSink();
    const hub = new ScreencastHub({ sink: port, log: NOOP_LOG, mode: "docker", clock });
    hub.linkUp({ ...LIMITS, max_targets: 2 });
    const pages = [1, 2, 3].map((i) => fakePage(`T${i}`, `https://${i}.example/`));
    await hub.attach(fakeContext(pages, [fakeTarget("W", "https://w/", "", "service_worker")]));
    assert.deepEqual(
        last(sink.targets).targets.map((t) => t.target_id),
        ["T1", "T2"],
    );
});

test("target events are coalesced into one Targets refresh", async () => {
    const { hub, sink, clock } = newHub();
    const pages = [fakePage("T1", "https://a.example/")];
    const ctx = fakeContext(pages);
    await hub.attach(ctx);
    const before = sink.targets.length;
    pages.push(fakePage("T2", "https://b.example/"));
    for (let i = 0; i < 10; i++) ctx.browser.emit("targetcreated", pages[1].target());
    clock.advance(1000);
    assert.equal(sink.targets.length, before + 1);
    assert.equal(last(sink.targets).targets.length, 2);
});

test("Chrome is asked for about fps frames, and a mode change reopens at the new rate", async () => {
    const { hub, sink } = newHub({ mode: "docker" });
    const page = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([page]));
    await hub.start();
    assert.equal(page.sessions[0].sent[0].params.everyNthFrame, CHROME_SCREENCAST_FPS / DEFAULT_FPS);
    await hub.setStatus("host", { type: "ready" });
    await flush();
    assert.equal(page.sessions[0].detached, true);
    assert.equal(page.sessions.length, 2);
    assert.equal(page.sessions[1].sent[0].params.everyNthFrame, CHROME_SCREENCAST_FPS / HOST_FPS);
    assert.equal(last(sink.stream).state, "live");
});

test("a disconnected browser pauses the stream and is never reopened", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    const ctx = fakeContext([page]);
    await hub.attach(ctx);
    await hub.start();
    assert.equal(last(sink.stream).state, "live");
    // Puppeteer does not mark the page closed on a browser disconnect.
    ctx.browser.emit("disconnected");
    await flush();
    assert.equal(page.sessions[0].detached, true);
    assert.deepEqual(last(sink.stream), { state: "paused", browser_gen: 1, target: null });
    assert.deepEqual(last(sink.targets), { browser_gen: 1, targets: [], followed: null });
    await hub.setStatus("docker", { type: "recovering" });
    await hub.setStatus("docker", { type: "ready" });
    await flush();
    assert.equal(page.sessions.length, 1, "no session on the dead context");
    assert.equal(last(sink.stream).state, "paused");
    // A new browser resumes.
    const fresh = fakePage("T2", "https://b.example/");
    await hub.attach(fakeContext([fresh]));
    await flush();
    assert.equal(fresh.sessions.length, 1);
    assert.equal(last(sink.stream).state, "live");
});

test("a browser reporting connected=false is treated as gone", async () => {
    const { hub, sink } = newHub();
    const page = fakePage("T1", "https://a.example/");
    const ctx = fakeContext([page]);
    ctx.browser.connected = false;
    await hub.attach(ctx);
    await hub.start();
    assert.equal(page.sessions.length, 0);
    assert.equal(last(sink.stream).state, "paused");
});

test("a new browser never sends the old stream's target under the new gen", async () => {
    const { hub, sink } = newHub();
    const old = fakePage("T1", "https://a.example/");
    await hub.attach(fakeContext([old]));
    await hub.start();
    const before = sink.targets.length;
    const fresh = fakePage("T9", "https://b.example/");
    await hub.attach(fakeContext([fresh]));
    for (const msg of sink.targets.slice(before)) {
        assert.equal(msg.browser_gen, 2);
        assert.notEqual(msg.followed, "T1");
    }
    for (const msg of sink.stream) {
        if (msg.browser_gen === 2 && msg.target !== null) assert.equal(msg.target.target_id, "T9");
    }
});

test("clamped text is always well-formed UTF-16", () => {
    const emoji = "\u{1F600}";
    assert.equal(clampText("a".repeat(255) + emoji, 256), "a".repeat(255));
    assert.equal(clampText("a" + emoji, 3), "a" + emoji);
    assert.equal(clampText("\ud800", 256), "\ufffd");
    assert.equal(clampText("x\udc00y", 256), "x\ufffdy");
    for (const s of [clampText("a".repeat(255) + emoji, 256), clampText("\ud800", 256)]) {
        assert.equal(JSON.stringify(s).includes("\\ud"), false, JSON.stringify(s));
    }
    const limits = { ...LIMITS, max_text_chars: 2 };
    const info = toTargetInfo(fakeTarget("T1", "https://a/", "a" + emoji), limits);
    assert.equal(info.title, "a");
});
