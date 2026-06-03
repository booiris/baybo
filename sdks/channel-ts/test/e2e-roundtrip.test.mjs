/**
 * End-to-end round-trip test.
 *
 * Stands up a fixture WS server that speaks the same MessagePack wire
 * protocol as `crates/gateway/src/channel/`. Drives a stub `Channel`
 * through `runChannel` and asserts every frame encodes / decodes
 * correctly across the boundary.
 *
 * This is the belt-and-braces coverage the TODO called out: type
 * generation via ts-rs catches schema drift, but rename-all flips,
 * MessagePack tag renames, and serde-attribute tweaks only surface
 * when a real encoder talks to a real decoder.
 */
import { WebSocketServer } from "ws";
import test from "node:test";
import assert from "node:assert/strict";

import {
  runChannel,
  defaultLogger,
} from "../dist/index.js";
import { decodeFrame, encodeFrame } from "../dist/wire.js";

const TOKEN = "fixture-token-abc";

/** Spin up one-shot fixture server; resolves with { url, recorded }. */
function startFixture() {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  const recorded = {
    register: null,
    outboundMessage: null,
    resolveApproval: null,
  };
  const done = new Promise((resolve) => {
    wss.on("connection", (ws) => {
      ws.on("message", (data) => {
        const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data;
        const buf = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
        const frame = decodeFrame(buf);
        switch (frame.kind) {
          case "register": {
            recorded.register = frame;
            ws.send(encodeFrame({ kind: "register_ack", ok: true, reason: null }));

            // Stream some server → client frames now that the handshake is done.
            ws.send(
              encodeFrame({
                kind: "answer_delta",
                session_id: "sess-1",
                user_id: "tg_42",
                text: "par",
              }),
            );
            ws.send(
              encodeFrame({
                kind: "notice",
                session_id: "sess-1",
                user_id: "tg_42",
                level: "warn",
                text: "low battery",
              }),
            );
            ws.send(
              encodeFrame({
                kind: "approval_requested",
                call_id: "call-1",
                session_id: "sess-1",
                user_id: "tg_42",
                tool: "fs.read",
                accesses: [],
                params_preview: "{}",
              }),
            );
            // Echo an approval_resolved for the same call_id so we
            // exercise the `approve_always` pass-through on inbound.
            ws.send(
              encodeFrame({
                kind: "approval_resolved",
                call_id: "call-2",
                decision: "approve_always",
              }),
            );
            ws.send(
              encodeFrame({
                kind: "message",
                content: "pong",
                session_id: "sess-1",
                user_id: "",
                channel_type: "fixture",
              }),
            );
            // Defensive: a user-role Message frame must not reach
            // `onMessage` on a sidecar — the SDK forwards onMessage to
            // `platform.sendText` and would echo the user's own input
            // back to the upstream platform. The gateway already
            // guards this at `Channel::echo_inbound`; the SDK guard is
            // belt-and-suspenders against a future regression.
            ws.send(
              encodeFrame({
                kind: "message",
                content: "echo-of-user-input",
                session_id: "sess-1",
                user_id: "tg_42",
                channel_type: "fixture",
                role: "user",
              }),
            );
            break;
          }
          case "message": {
            recorded.outboundMessage = frame;
            break;
          }
          case "resolve_approval": {
            recorded.resolveApproval = frame;
            break;
          }
          default: {
            // ignore frames we didn't expect for this test
          }
        }
      });
    });
    wss.once("listening", () => {
      const addr = wss.address();
      resolve({
        url: `ws://127.0.0.1:${addr.port}`,
        recorded,
        close: () => new Promise((r) => wss.close(() => r())),
        closeClient: () =>
          new Promise((r) => {
            for (const client of wss.clients) client.close();
            r();
          }),
      });
    });
  });
  return done;
}

test("runChannel round-trips every frame shape across a real WS hop", async () => {
  const fixture = await startFixture();

  const gotDelta = { count: 0, last: null };
  const gotNotice = { count: 0, last: null };
  const gotMessage = { count: 0, last: null };
  const gotApprovalResolved = { calls: [] };
  let approvalResolver;
  const approvalSeen = new Promise((r) => (approvalResolver = r));

  const inboundYielded = { fired: false };
  const stub = {
    channelType: "fixture",
    async onMessage(msg) {
      gotMessage.count += 1;
      gotMessage.last = msg;
    },
    async onDelta(delta) {
      gotDelta.count += 1;
      gotDelta.last = delta;
    },
    async onNotice(notice) {
      gotNotice.count += 1;
      gotNotice.last = notice;
    },
    async onApprovalRequested(req) {
      approvalResolver(req);
      return "approve_always";
    },
    async onApprovalResolved(callId, decision) {
      gotApprovalResolved.calls.push({ callId, decision });
    },
    async *inbound(signal) {
      yield {
        sessionId: "sess-1",
        userId: "tg_42",
        content: "ping",
      };
      inboundYielded.fired = true;
      // Park here until the runner aborts; returning earlier would
      // close the outbound pump before the runner has finished
      // consuming the server's frames.
      await new Promise((resolve) => {
        if (signal.aborted) resolve();
        else signal.addEventListener("abort", () => resolve(), { once: true });
      });
    },
  };

  const controller = new AbortController();
  const logger = defaultLogger("fixture");
  const done = runChannel(stub, {
    wsUrl: fixture.url,
    token: TOKEN,
    abortSignal: controller.signal,
    reconnect: false,
    logger,
  });

  // Wait for the approval to hit the stub so we know the server fully
  // flushed its batch of frames.
  const approvalReq = await approvalSeen;
  assert.equal(approvalReq.callId, "call-1");

  // Give the event loop a beat so inbound deliveries + outbound sends
  // settle before we inspect.
  await new Promise((r) => setTimeout(r, 50));

  // Emit a log line through the SDK's default logger. The default
  // logger writes one NDJSON record per call to stdout (info/debug)
  // or stderr (warn/error); the gateway's pipe drain parses them
  // back into structured log records. Capture stdout to assert the
  // line lands there in the expected shape.
  const stdoutLines = captureStdout(() => {
    logger.info("hello from stub");
  });

  controller.abort();
  await done;
  await fixture.close();

  // ---- Handshake ---------------------------------------------------
  assert.equal(fixture.recorded.register.kind, "register");
  assert.equal(fixture.recorded.register.token, TOKEN);
  assert.equal(fixture.recorded.register.channel_type, "fixture");

  // ---- Server → client -------------------------------------------
  assert.equal(gotDelta.count, 1);
  assert.deepEqual(gotDelta.last, {
    sessionId: "sess-1",
    userId: "tg_42",
    text: "par",
  });
  assert.equal(gotNotice.count, 1);
  assert.deepEqual(gotNotice.last, {
    sessionId: "sess-1",
    userId: "tg_42",
    level: "warn",
    text: "low battery",
    // A plain notice carries no `transient` flag on the wire; the runner
    // normalizes the absent field to `false` before `onNotice`.
    transient: false,
  });
  // Only the assistant-role message reaches onMessage — the role:"user"
  // frame the fixture also sent is dropped by the runner's defensive
  // guard. Without the guard `count` would be 2 and the bot SDK would
  // forward "echo-of-user-input" back to the platform.
  assert.equal(gotMessage.count, 1);
  assert.equal(gotMessage.last.content, "pong");
  assert.equal(gotMessage.last.sessionId, "sess-1");

  // approve_always survives the string-encoded hop in both directions.
  assert.deepEqual(gotApprovalResolved.calls, [
    { callId: "call-2", decision: "approve_always" },
  ]);

  // ---- Client → server -------------------------------------------
  assert.ok(inboundYielded.fired, "inbound() generator produced a message");
  assert.ok(
    fixture.recorded.outboundMessage,
    "server received the outbound message frame",
  );
  assert.equal(fixture.recorded.outboundMessage.content, "ping");
  assert.equal(fixture.recorded.outboundMessage.session_id, "sess-1");
  assert.equal(fixture.recorded.outboundMessage.user_id, "tg_42");
  assert.equal(fixture.recorded.outboundMessage.channel_type, "fixture");

  // `onApprovalRequested` returned "approve_always"; the runner must
  // have encoded the matching ResolveApproval frame.
  assert.ok(
    fixture.recorded.resolveApproval,
    "server received resolve_approval frame",
  );
  assert.equal(fixture.recorded.resolveApproval.call_id, "call-1");
  assert.equal(
    fixture.recorded.resolveApproval.decision,
    "approve_always",
    "approve_always round-trips back to the server intact",
  );

  // ---- Default-logger NDJSON on stdout -------------------------------
  const helloLog = stdoutLines
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .find((rec) => rec && rec.msg && rec.msg.includes("hello from stub"));
  assert.ok(helloLog, "logger.info wrote NDJSON to stdout");
  assert.equal(helloLog.level, "info");
});

/**
 * Monkey-patch `process.stdout.write` for the duration of `fn`,
 * collecting every newline-terminated UTF-8 line written through it.
 * Returns the captured lines in order. Restores the original `write`
 * before returning even if `fn` throws.
 */
function captureStdout(fn) {
  const original = process.stdout.write.bind(process.stdout);
  const lines = [];
  let pending = "";
  process.stdout.write = (chunk, ...rest) => {
    const text =
      typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8");
    pending += text;
    let nl = pending.indexOf("\n");
    while (nl >= 0) {
      lines.push(pending.slice(0, nl));
      pending = pending.slice(nl + 1);
      nl = pending.indexOf("\n");
    }
    return original(chunk, ...rest);
  };
  try {
    fn();
  } finally {
    process.stdout.write = original;
  }
  if (pending.length > 0) lines.push(pending);
  return lines;
}

test("runChannel register_ack=false is treated as a fatal config failure", async () => {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  const url = await new Promise((resolve) => {
    wss.once("listening", () => {
      const addr = wss.address();
      resolve(`ws://127.0.0.1:${addr.port}`);
    });
  });
  wss.on("connection", (ws) => {
    ws.once("message", () => {
      ws.send(
        encodeFrame({
          kind: "register_ack",
          ok: false,
          reason: "bad channel_type",
        }),
      );
    });
  });

  const stub = {
    channelType: "fixture",
    async onMessage() {},
    async *inbound() {},
  };

  await assert.rejects(
    runChannel(stub, {
      wsUrl: url,
      token: TOKEN,
      reconnect: false,
    }),
    (err) => {
      assert.equal(err.kind, "register_rejected");
      assert.match(err.message, /bad channel_type/);
      return true;
    },
  );

  await new Promise((r) => wss.close(() => r()));
});
