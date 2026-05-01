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
    sidecarLogs: [],
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
            ws.send(
              encodeFrame({
                kind: "register_ack",
                ok: true,
                reason: null,
                capabilities: ["secrets"],
              }),
            );

            // Stream some server → client frames now that the handshake is done.
            ws.send(
              encodeFrame({
                kind: "delta",
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
                kind: "tool_call_started",
                session_id: "sess-1",
                user_id: "tg_42",
                call_id: "call-tool-1",
                tool: "Bash",
                params_preview: '{"cmd":"ls"}',
                description: "list files",
              }),
            );
            ws.send(
              encodeFrame({
                kind: "tool_call_completed",
                session_id: "sess-1",
                user_id: "tg_42",
                call_id: "call-tool-1",
                tool: "Bash",
                result_preview: "a\nb\nc",
                duration_ms: 12,
              }),
            );
            ws.send(
              encodeFrame({
                kind: "diagnose_request",
                request_id: "diag-1",
                bot_id: "alpha",
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
          case "diagnose_reply": {
            recorded.diagnoseReply = frame;
            break;
          }
          case "sidecar_log": {
            recorded.sidecarLogs.push(frame);
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
  const gotToolStarted = { count: 0, last: null };
  const gotToolCompleted = { count: 0, last: null };
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
    async onToolCallStarted(ev) {
      gotToolStarted.count += 1;
      gotToolStarted.last = ev;
    },
    async onToolCallCompleted(ev) {
      gotToolCompleted.count += 1;
      gotToolCompleted.last = ev;
    },
    async onApprovalRequested(req) {
      approvalResolver(req);
      return "approve_always";
    },
    async onApprovalResolved(callId, decision) {
      gotApprovalResolved.calls.push({ callId, decision });
    },
    async onDiagnoseRequested(req) {
      return [
        { name: "bot_id", status: "ok", detail: req.botId },
        { name: "ws", status: "ok", detail: "connected" },
      ];
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

  // Emit a log line through the SDK's default logger. The runner
  // should have installed the wire sink, so this becomes a SidecarLog
  // frame on the server side.
  logger.info("hello from stub");
  await new Promise((r) => setTimeout(r, 30));

  controller.abort();
  await done;
  await fixture.close();

  // ---- Handshake ---------------------------------------------------
  assert.equal(fixture.recorded.register.kind, "register");
  assert.equal(fixture.recorded.register.token, TOKEN);
  assert.equal(fixture.recorded.register.channel_type, "fixture");
  // Old `protocol_version` field has been replaced by `capabilities`.
  // Default sidecars advertise nothing → field omitted on the wire.
  assert.equal(fixture.recorded.register.protocol_version, undefined);
  // The fixture's stub channel implements `onDiagnoseRequested`,
  // `onToolCallStarted`, and `onToolCallCompleted`, so the runner
  // auto-advertises both gating capabilities. Per-cap assertions
  // (rather than `deepEqual`) so adding a future capability doesn't
  // churn this test.
  assert.ok(
    fixture.recorded.register.capabilities?.includes("diagnose"),
    "diagnose should be advertised",
  );
  assert.ok(
    fixture.recorded.register.capabilities?.includes("tool_telemetry"),
    "tool_telemetry should be advertised",
  );

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
  });
  assert.equal(gotToolStarted.count, 1);
  assert.deepEqual(gotToolStarted.last, {
    sessionId: "sess-1",
    userId: "tg_42",
    callId: "call-tool-1",
    tool: "Bash",
    paramsPreview: '{"cmd":"ls"}',
    description: "list files",
  });
  assert.equal(gotToolCompleted.count, 1);
  // duration_ms rides as bigint on the wire; the SDK normalises to number.
  assert.equal(gotToolCompleted.last.callId, "call-tool-1");
  assert.equal(gotToolCompleted.last.resultPreview, "a\nb\nc");
  assert.equal(gotToolCompleted.last.error, undefined);
  assert.equal(gotToolCompleted.last.durationMs, 12);
  assert.equal(gotMessage.count, 1);
  assert.equal(gotMessage.last.content, "pong");
  assert.equal(gotMessage.last.sessionId, "sess-1");

  // approve_always survives the string-encoded hop in both directions.
  assert.deepEqual(gotApprovalResolved.calls, [
    { callId: "call-2", decision: "approve_always" },
  ]);

  // Diagnose round-trip: the runner advertised the capability (the
  // stub implements onDiagnoseRequested) and emitted a reply matching
  // the request_id with the hook's checks.
  assert.ok(
    fixture.recorded.register.capabilities?.includes("diagnose"),
    `register frame should advertise diagnose; got ${JSON.stringify(fixture.recorded.register.capabilities)}`,
  );
  assert.ok(
    fixture.recorded.diagnoseReply,
    "server received the diagnose_reply",
  );
  assert.equal(fixture.recorded.diagnoseReply.request_id, "diag-1");
  assert.equal(fixture.recorded.diagnoseReply.ok, true);
  assert.equal(fixture.recorded.diagnoseReply.checks.length, 2);
  assert.equal(fixture.recorded.diagnoseReply.checks[0].name, "bot_id");
  assert.equal(fixture.recorded.diagnoseReply.checks[0].detail, "alpha");

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

  // ---- SidecarLog forwarding -------------------------------------
  const helloLog = fixture.recorded.sidecarLogs.find((l) =>
    l.text.includes("hello from stub"),
  );
  assert.ok(helloLog, "logger.info forwarded as SidecarLog");
  assert.equal(helloLog.level, "info");
});

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
