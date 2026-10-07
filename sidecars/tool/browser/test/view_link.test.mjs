// The sidecar end of the browser-view link: codec (against the shapes and
// byte layout fixed by crates/browser-view's codec.rs / wire.rs tests),
// handshake, backpressure, reconnect and env hygiene.

import { EventEmitter } from "node:events";

import assert from "node:assert/strict";
import { test } from "node:test";

import {
    ENV_LINK_SECRET,
    ENV_LINK_SOCKET,
    KIND_FRAME,
    KIND_JSON,
    LinkCodecError,
    LinkDownDecoder,
    PRE_ACK_MAX_JSON_BYTES,
    ViewLink,
    encodeFrame,
    encodeJson,
    takeLinkEnv,
} from "../dist/test/view_link.mjs";

// JSON fixtures copied from crates/browser-view/src/wire.rs tests.
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
const HELLO = {
    type: "hello",
    protocol: 1,
    secret: "s3cr3t",
    boot_id: "boot-1",
    pid: 4242,
    capabilities: ["screencast"],
};
const STATUS = {
    type: "status",
    mode: "docker",
    phase: { type: "docker", phase: "docker-building-image" },
    browser_gen: 3,
};
const HEADER = {
    target_id: "T1",
    browser_gen: 1,
    seq: 9,
    captured_at_ms: 12.5,
    device_width: 1280.0,
    device_height: 800.0,
    offset_top: 0.0,
    page_scale_factor: 1.0,
    scroll_offset_x: 0.0,
    scroll_offset_y: 0.0,
};

const NOOP_LOG = { info() {}, debug() {}, warn() {}, error() {} };

/** Gateway-side framing, written independently of the code under test. */
function gatewayMessage(obj) {
    const body = Buffer.from(JSON.stringify(obj));
    const out = Buffer.alloc(5 + body.length);
    out.writeUInt32BE(1 + body.length, 0);
    out.writeUInt8(KIND_JSON, 4);
    body.copy(out, 5);
    return out;
}

/** Split a sidecar byte stream the way GatewayLinkCodec does. */
function gatewayDecode(buf) {
    const out = [];
    let at = 0;
    while (at < buf.length) {
        const len = buf.readUInt32BE(at);
        const kind = buf.readUInt8(at + 4);
        const body = buf.subarray(at + 5, at + 4 + len);
        if (kind === KIND_JSON) {
            out.push({ json: JSON.parse(body.toString()) });
        } else {
            const hdrLen = body.readUInt32BE(0);
            out.push({
                header: JSON.parse(body.subarray(4, 4 + hdrLen).toString()),
                jpeg: body.subarray(4 + hdrLen),
                headerAndJpeg: body,
            });
        }
        at += 4 + len;
    }
    return out;
}

test("json encoding is [u32 BE len][kind 0][json], as codec.rs writes it", () => {
    const buf = encodeJson(HELLO, PRE_ACK_MAX_JSON_BYTES);
    const json = Buffer.from(JSON.stringify(HELLO));
    assert.equal(buf.readUInt32BE(0), json.length + 1);
    assert.equal(buf[4], KIND_JSON);
    assert.deepEqual(buf.subarray(5), json);
    assert.deepEqual(gatewayDecode(buf), [{ json: HELLO }]);
    assert.deepEqual(gatewayDecode(encodeJson(STATUS, 1000)), [{ json: STATUS }]);
});

test("json larger than the limit is refused", () => {
    assert.throws(() => encodeJson({ type: "x", pad: "y".repeat(100) }, 50), LinkCodecError);
});

test("frame encoding is [len][kind 1][u32 BE hdr_len][header][jpeg]", () => {
    const jpeg = Buffer.from("\xFF\xD8jpeg-bytes\xFF\xD9", "latin1");
    const buf = encodeFrame(HEADER, jpeg, HELLO_ACK.limits.max_frame_bytes);
    assert.equal(buf[4], KIND_FRAME);
    const [decoded] = gatewayDecode(buf);
    assert.deepEqual(decoded.header, HEADER);
    assert.deepEqual(decoded.jpeg, jpeg);
    const headerJson = Buffer.from(JSON.stringify(HEADER));
    const expected = Buffer.alloc(4);
    expected.writeUInt32BE(headerJson.length, 0);
    assert.deepEqual(decoded.headerAndJpeg, Buffer.concat([expected, headerJson, jpeg]));
});

test("frame encoding enforces the frame bounds", () => {
    assert.throws(() => encodeFrame(HEADER, Buffer.alloc(0), 10), LinkCodecError);
    assert.throws(() => encodeFrame(HEADER, Buffer.alloc(11), 10), LinkCodecError);
    assert.throws(
        () => encodeFrame({ ...HEADER, target_id: "x".repeat(5000) }, Buffer.alloc(1), 10),
        LinkCodecError,
    );
    assert.doesNotThrow(() => encodeFrame(HEADER, Buffer.alloc(10), 10));
});

test("decoder reads LinkDown messages byte at a time", () => {
    const msgs = [
        HELLO_ACK,
        { type: "hello_reject", reason: "already_connected" },
        { type: "start_screencast" },
        { type: "stop_screencast" },
    ];
    const full = Buffer.concat(msgs.map(gatewayMessage));
    const decoder = new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES);
    const out = [];
    for (const byte of full) out.push(...decoder.push(Buffer.from([byte])));
    assert.deepEqual(
        out,
        msgs.map((ok) => ({ ok })),
    );
});

test("decoder rejects oversized, empty and frame-kind messages from the prefix alone", () => {
    const over = Buffer.alloc(4);
    over.writeUInt32BE(PRE_ACK_MAX_JSON_BYTES + 2, 0);
    assert.throws(() => new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES).push(over), LinkCodecError);
    assert.throws(
        () => new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES).push(Buffer.from([0, 0, 0, 0])),
        LinkCodecError,
    );
    assert.throws(
        () => new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES).push(Buffer.from([0, 0, 0, 3, KIND_FRAME])),
        LinkCodecError,
    );
    assert.throws(
        () => new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES).push(Buffer.from([0, 0, 0, 3, 7])),
        LinkCodecError,
    );
});

test("decoder skips malformed and unknown messages without losing the stream", () => {
    const bad = Buffer.from("{x}");
    const raw = Buffer.alloc(5 + bad.length);
    raw.writeUInt32BE(1 + bad.length, 0);
    bad.copy(raw, 5);
    const out = new LinkDownDecoder(PRE_ACK_MAX_JSON_BYTES).push(
        Buffer.concat([
            raw,
            gatewayMessage({ type: "acquire_control", request_id: 1 }),
            gatewayMessage({ type: "start_screencast" }),
        ]),
    );
    assert.equal(out.length, 3);
    assert.ok("malformed" in out[0]);
    assert.ok("malformed" in out[1]);
    assert.deepEqual(out[2], { ok: { type: "start_screencast" } });
});

test("takeLinkEnv reads both values and deletes them", () => {
    const env = { [ENV_LINK_SOCKET]: "/run/x.sock", [ENV_LINK_SECRET]: "hunter2", OTHER: "1" };
    assert.deepEqual(takeLinkEnv(env), { socketPath: "/run/x.sock", secret: "hunter2" });
    assert.deepEqual(env, { OTHER: "1" });
});

test("takeLinkEnv: a half configuration is off, and still scrubbed", () => {
    const env = { [ENV_LINK_SECRET]: "hunter2" };
    assert.equal(takeLinkEnv(env), null);
    assert.deepEqual(env, {});
    assert.equal(takeLinkEnv({}), null);
    assert.equal(takeLinkEnv({ [ENV_LINK_SOCKET]: "", [ENV_LINK_SECRET]: "s" }), null);
});

class FakeSocket extends EventEmitter {
    written = [];
    writeResult = true;
    destroyed = false;
    write(buf) {
        this.written.push(buf);
        return this.writeResult;
    }
    destroy() {
        if (this.destroyed) return;
        this.destroyed = true;
        queueMicrotask(() => this.emit("close"));
    }
    decoded() {
        return gatewayDecode(Buffer.concat(this.written));
    }
    reply(obj) {
        this.emit("data", gatewayMessage(obj));
    }
}

function harness(extra = {}) {
    const sockets = [];
    const events = [];
    const link = new ViewLink({
        socketPath: "/fake.sock",
        secret: "s3cr3t",
        log: NOOP_LOG,
        reconnectBaseMs: 1,
        reconnectMaxMs: 2,
        connect: (path) => {
            assert.equal(path, "/fake.sock");
            const s = new FakeSocket();
            sockets.push(s);
            return s;
        },
        handler: {
            linkUp: (limits) => events.push({ up: limits }),
            linkDown: () => events.push("down"),
            startScreencast: () => events.push("start"),
            stopScreencast: () => events.push("stop"),
        },
        ...extra,
    });
    return { link, sockets, events };
}

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const header = (seq) => ({ ...HEADER, seq });

test("hello first, then HelloAck limits are adopted and commands flow", async () => {
    const { link, sockets, events } = harness();
    link.start();
    const sock = sockets[0];
    const [hello] = sock.decoded();
    assert.equal(hello.json.type, "hello");
    assert.equal(hello.json.protocol, 1);
    assert.equal(hello.json.secret, "s3cr3t");
    assert.equal(hello.json.pid, process.pid);
    assert.deepEqual(hello.json.capabilities, ["screencast"]);
    assert.equal(typeof hello.json.boot_id, "string");
    assert.ok(hello.json.boot_id.length > 0);
    // Nothing but Hello before the ack.
    link.status({ mode: "host", phase: { type: "idle" }, browser_gen: 0 });
    link.frame(header(0), Buffer.from([1]));
    assert.equal(sock.written.length, 1);

    sock.reply(HELLO_ACK);
    assert.deepEqual(events, [{ up: HELLO_ACK.limits }]);
    assert.equal(link.isUp, true);
    sock.reply({ type: "start_screencast" });
    sock.reply({ type: "stop_screencast" });
    assert.deepEqual(events.slice(1), ["start", "stop"]);
    link.availability(null);
    assert.deepEqual(sock.decoded().at(-1), { json: { type: "availability", unavailable: null } });
    await link.shutdown();
});

test("write() === false drops frames until 'drain', then sends the newest", async () => {
    const { link, sockets } = harness();
    link.start();
    const sock = sockets[0];
    sock.reply(HELLO_ACK);
    sock.writeResult = false;
    link.frame(header(1), Buffer.from([1]));
    link.frame(header(2), Buffer.from([2]));
    link.frame(header(3), Buffer.from([3]));
    // JSON still goes out while frames are held back.
    link.stream({ state: "live", browser_gen: 1, target: null });
    let frames = sock.decoded().filter((m) => m.header);
    assert.deepEqual(
        frames.map((f) => f.header.seq),
        [1],
    );
    assert.equal(sock.decoded().at(-1).json.type, "stream");
    sock.writeResult = true;
    sock.emit("drain");
    link.frame(header(4), Buffer.from([4]));
    frames = sock.decoded().filter((m) => m.header);
    assert.deepEqual(
        frames.map((f) => f.header.seq),
        [1, 3, 4],
    );
    await link.shutdown();
});

test("a dropped link reports down and reconnects with the same boot id", async () => {
    const { link, sockets, events } = harness();
    link.start();
    sockets[0].reply(HELLO_ACK);
    sockets[0].destroy();
    await wait(30);
    assert.ok(events.includes("down"));
    assert.ok(sockets.length >= 2, `${sockets.length} dials`);
    const first = sockets[0].decoded()[0].json;
    const second = sockets[1].decoded()[0].json;
    assert.equal(second.type, "hello");
    assert.equal(second.boot_id, first.boot_id);
    await link.shutdown();
});

test("already_connected is retried; unauthorized stops the link for good", async () => {
    const { link, sockets } = harness();
    link.start();
    sockets[0].reply({ type: "hello_reject", reason: "already_connected" });
    await wait(30);
    assert.ok(sockets.length >= 2);
    const n = sockets.length;
    sockets[n - 1].reply({ type: "hello_reject", reason: "unauthorized" });
    await wait(30);
    assert.equal(sockets.length, n);
    await link.shutdown();
});

test("a HelloAck for another protocol stops the link", async () => {
    const { link, sockets, events } = harness();
    link.start();
    sockets[0].reply({ ...HELLO_ACK, protocol: 2 });
    await wait(30);
    assert.equal(sockets.length, 1);
    assert.deepEqual(events, []);
    await link.shutdown();
});

test("shutdown stops reconnecting before it closes the socket", async () => {
    const { link, sockets } = harness();
    link.start();
    sockets[0].reply(HELLO_ACK);
    await link.shutdown();
    await wait(30);
    assert.equal(sockets.length, 1);
    assert.equal(sockets[0].destroyed, true);
});

test("a protocol error from the gateway drops and redials the link", async () => {
    const { link, sockets } = harness();
    link.start();
    sockets[0].emit("data", Buffer.from([0, 0, 0, 0]));
    await wait(30);
    assert.equal(sockets[0].destroyed, true);
    assert.ok(sockets.length >= 2);
    await link.shutdown();
});
