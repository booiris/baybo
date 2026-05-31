import WebSocket from "ws";

import {
  RunnerError,
  type BotStatusReport,
  type Channel,
  type RunOptions,
  type ReconnectPolicy,
  type ApprovalDecision,
  type NoticeLevel,
  type ToolStatus,
} from "./channel.js";
import { defaultLogger, type Logger } from "./logger.js";
import { decodeFrame, encodeFrame, type Frame } from "./wire.js";

/** App-level Ping cadence. See [`Frame::Ping`] in
 *  `crates/channels/src/wire.rs` for the half-open-WS rationale —
 *  same trade-offs as the web chat client. */
const HEARTBEAT_PING_INTERVAL_MS = 20_000;
const HEARTBEAT_LIVENESS_TIMEOUT_MS = 45_000;
const HEARTBEAT_TICK_MS = 5_000;

/**
 * Run a {@link Channel} implementation. Handles the full WebSocket +
 * MessagePack transport: Register/Ack handshake, frame dispatch to the
 * implementation's hooks, outbound pump from `channel.inbound()`, and
 * concurrent approval handling.
 *
 * Auto-reconnects by default with exponential backoff + jitter on
 * transient failures; pass `opts.reconnect = false` to disable.
 * Resolves when the root abort signal fires (or after the first
 * disconnect when reconnect is disabled), after `onStop` has run.
 * Rejects on fatal failures (bad config, rejected registration,
 * protocol violation).
 */
export async function runChannel(
  channel: Channel,
  opts: RunOptions = {},
): Promise<void> {
  const wsUrl = opts.wsUrl ?? deriveWsUrl();
  const token = opts.token ?? process.env["AURA_CHANNEL_TOKEN"];
  if (!token) {
    throw new RunnerError(
      "missing token: pass opts.token or set AURA_CHANNEL_TOKEN",
      "config",
    );
  }
  const logger = opts.logger ?? defaultLogger(channel.channelType);
  const policy = resolvePolicy(opts.reconnect);

  const rootAbort = new AbortController();
  if (opts.abortSignal) {
    if (opts.abortSignal.aborted) rootAbort.abort();
    else
      opts.abortSignal.addEventListener("abort", () => rootAbort.abort(), {
        once: true,
      });
  }

  try {
    let attempt = 0;
    let delay = policy?.initialDelayMs ?? 0;

    while (!rootAbort.signal.aborted) {
      try {
        await runOnce(channel, wsUrl, token, rootAbort.signal, logger);
      } catch (err) {
        if (rootAbort.signal.aborted) return;
        if (!policy || !isTransient(err)) throw err;
        if (
          policy.maxAttempts !== undefined &&
          attempt + 1 >= policy.maxAttempts
        ) {
          throw err;
        }
        const waitMs = applyJitter(delay, policy.jitter);
        logger.warn(
          `connection lost (${describeErr(err)}); reconnecting in ${Math.round(waitMs)}ms (attempt ${attempt + 1})`,
        );
        await sleep(waitMs, rootAbort.signal);
        if (rootAbort.signal.aborted) return;
        delay = Math.min(delay * policy.multiplier, policy.maxDelayMs);
        attempt += 1;
        continue;
      }

      // runOnce returned without throwing: peer closed cleanly.
      if (rootAbort.signal.aborted) return;
      if (!policy) return;
      const waitMs = applyJitter(policy.initialDelayMs, policy.jitter);
      logger.warn(
        `peer closed; reconnecting in ${Math.round(waitMs)}ms`,
      );
      await sleep(waitMs, rootAbort.signal);
      // A previously-healthy connection means the next failure should
      // start its backoff ladder from the bottom, not pick up where
      // the last failure left off.
      attempt = 0;
      delay = policy.initialDelayMs;
    }
  } finally {
    if (channel.onStop) {
      try {
        await channel.onStop();
      } catch (err) {
        logger.error("onStop threw", err);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Single-connection lifecycle
// ---------------------------------------------------------------------------

async function runOnce(
  channel: Channel,
  wsUrl: string,
  token: string,
  rootSignal: AbortSignal,
  logger: Logger,
): Promise<void> {
  const connAbort = new AbortController();
  const onRootAbort = () => connAbort.abort();
  if (rootSignal.aborted) connAbort.abort();
  else rootSignal.addEventListener("abort", onRootAbort, { once: true });

  const dialUrl = appendToken(wsUrl, token);
  logger.info(
    `connecting ${wsUrl} (token ${token.slice(0, 6)}…, len ${token.length})`,
  );
  const ws = new WebSocket(dialUrl, { handshakeTimeout: 5_000 });
  ws.binaryType = "arraybuffer";
  const frames = new FrameQueue();
  const liveness: Liveness = { lastFrameAt: Date.now() };
  attachWsHandlers(ws, frames, logger, liveness);

  let stopHeartbeat: (() => void) | null = null;
  try {
    await waitOpen(ws);
    await performRegister(ws, frames, channel.channelType, token);
    logger.info("registered");

    connAbort.signal.addEventListener("abort", () => safeClose(ws), {
      once: true,
    });
    // RegisterAck already bumped lastFrameAt; arming after the
    // handshake means a hung Register surfaces through
    // `handshakeTimeout` rather than the liveness watchdog.
    stopHeartbeat = startHeartbeat(ws, liveness, connAbort, logger);

    const outbound = pumpOutbound(channel, ws, connAbort.signal, logger);
    const inbound = pumpInbound(channel, ws, frames, connAbort, logger);
    await Promise.all([outbound, inbound]);
  } finally {
    if (stopHeartbeat) stopHeartbeat();
    connAbort.abort();
    safeClose(ws);
    rootSignal.removeEventListener("abort", onRootAbort);
  }
}

// ---------------------------------------------------------------------------
// Reconnect policy
// ---------------------------------------------------------------------------

interface ResolvedPolicy {
  initialDelayMs: number;
  maxDelayMs: number;
  multiplier: number;
  jitter: number;
  maxAttempts: number | undefined;
}

const DEFAULT_POLICY: ResolvedPolicy = {
  initialDelayMs: 500,
  maxDelayMs: 30_000,
  multiplier: 2,
  jitter: 0.25,
  maxAttempts: undefined,
};

function resolvePolicy(
  raw: boolean | ReconnectPolicy | undefined,
): ResolvedPolicy | null {
  if (raw === false) return null;
  if (raw === undefined || raw === true) return { ...DEFAULT_POLICY };
  return {
    initialDelayMs: raw.initialDelayMs ?? DEFAULT_POLICY.initialDelayMs,
    maxDelayMs: raw.maxDelayMs ?? DEFAULT_POLICY.maxDelayMs,
    multiplier: raw.multiplier ?? DEFAULT_POLICY.multiplier,
    jitter: raw.jitter ?? DEFAULT_POLICY.jitter,
    maxAttempts: raw.maxAttempts,
  };
}

function isTransient(err: unknown): boolean {
  if (!(err instanceof RunnerError)) return false;
  return (
    err.kind === "connect" ||
    err.kind === "peer_closed" ||
    err.kind === "transport"
  );
}

function describeErr(err: unknown): string {
  if (err instanceof RunnerError) return `${err.kind}: ${err.message}`;
  if (err instanceof Error) return err.message;
  return String(err);
}

function applyJitter(base: number, jitter: number): number {
  const delta = base * jitter;
  return Math.max(0, base + (Math.random() * 2 - 1) * delta);
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, Math.max(0, ms));
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

// ---------------------------------------------------------------------------
// Connection setup
// ---------------------------------------------------------------------------

function deriveWsUrl(): string {
  const url = process.env["AURA_CHANNEL_URL"];
  if (!url) {
    throw new RunnerError(
      "missing connection target: pass opts.wsUrl or set AURA_CHANNEL_URL",
      "config",
    );
  }
  return url;
}

/// Append `token=<encoded>` to a ws:// URL's query string. Idempotent
/// — if an existing `token=` is present it's replaced.
function appendToken(url: string, token: string): string {
  const encoded = encodeURIComponent(token);
  const [base, query = ""] = url.split("?", 2);
  const kept = query
    .split("&")
    .filter((p) => p.length > 0 && !p.startsWith("token="));
  kept.push(`token=${encoded}`);
  return `${base}?${kept.join("&")}`;
}

function waitOpen(ws: WebSocket): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const onOpen = () => {
      ws.removeListener("error", onErr);
      resolve();
    };
    const onErr = (err: Error) => {
      ws.removeListener("open", onOpen);
      reject(new RunnerError(`connect failed: ${err.message}`, "connect"));
    };
    ws.once("open", onOpen);
    ws.once("error", onErr);
  });
}

async function performRegister(
  ws: WebSocket,
  frames: FrameQueue,
  channelType: string,
  token: string,
): Promise<void> {
  ws.send(
    encodeFrame({
      kind: "register",
      token,
      channel_type: channelType,
    }),
  );
  const ack = await frames.pop();
  if (ack.kind !== "register_ack") {
    throw new RunnerError(
      `expected register_ack, got ${ack.kind}`,
      "protocol_violation",
    );
  }
  if (!ack.ok) {
    throw new RunnerError(
      `register rejected: ${ack.reason ?? ""}`,
      "register_rejected",
    );
  }
}

function safeClose(ws: WebSocket): void {
  if (
    ws.readyState === WebSocket.OPEN ||
    ws.readyState === WebSocket.CONNECTING
  ) {
    try {
      ws.close();
    } catch {
      // swallow — already closing or closed
    }
  }
}

// ---------------------------------------------------------------------------
// Frame ingest queue
// ---------------------------------------------------------------------------

class FrameQueue {
  private readonly frames: Frame[] = [];
  private readonly waiters: Array<{
    resolve: (f: Frame) => void;
    reject: (err: unknown) => void;
  }> = [];
  private error: unknown | null = null;
  private closed = false;

  push(frame: Frame): void {
    if (this.closed) return;
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve(frame);
      return;
    }
    this.frames.push(frame);
  }

  fail(err: unknown): void {
    if (this.closed) return;
    this.error = err;
    this.closed = true;
    for (const w of this.waiters.splice(0)) w.reject(err);
  }

  pop(): Promise<Frame> {
    const ready = this.frames.shift();
    if (ready) return Promise.resolve(ready);
    if (this.error) return Promise.reject(this.error);
    if (this.closed) {
      return Promise.reject(new RunnerError("peer closed", "peer_closed"));
    }
    return new Promise<Frame>((resolve, reject) => {
      this.waiters.push({ resolve, reject });
    });
  }
}

interface Liveness {
  /** Epoch ms of the last MessagePack frame the WS delivered. Bumped
   *  on every `message` event regardless of payload — the watchdog
   *  only cares that *some* byte arrived from the peer recently. */
  lastFrameAt: number;
}

function attachWsHandlers(
  ws: WebSocket,
  frames: FrameQueue,
  logger: Logger,
  liveness: Liveness,
): void {
  ws.on("message", (data) => {
    liveness.lastFrameAt = Date.now();
    const bytes = normalizeBinary(data);
    if (!bytes) {
      frames.fail(
        new RunnerError("expected binary frame", "protocol_violation"),
      );
      return;
    }
    try {
      frames.push(decodeFrame(bytes));
    } catch (err) {
      logger.error("decode failed", err);
      frames.fail(
        err instanceof RunnerError
          ? err
          : new RunnerError(String(err), "decode"),
      );
    }
  });
  ws.on("close", () => {
    frames.fail(new RunnerError("peer closed", "peer_closed"));
  });
  ws.on("error", (err) => {
    frames.fail(
      new RunnerError(`transport error: ${err.message}`, "transport"),
    );
  });
}

/**
 * Tick interval that probes the WS with an app-level Ping and
 * aborts the connection when no frame has arrived within
 * {@link HEARTBEAT_LIVENESS_TIMEOUT_MS}. Aborting routes through the
 * existing `connAbort` plumbing so the outer reconnect ladder runs
 * the same way as any other transient failure. Returns a teardown
 * function the caller invokes from `finally`.
 */
function startHeartbeat(
  ws: WebSocket,
  liveness: Liveness,
  connAbort: AbortController,
  logger: Logger,
): () => void {
  const timer = setInterval(() => {
    if (ws.readyState !== WebSocket.OPEN) return;
    const now = Date.now();
    if (now - liveness.lastFrameAt > HEARTBEAT_LIVENESS_TIMEOUT_MS) {
      logger.warn(
        `heartbeat: no frame in ${Math.round((now - liveness.lastFrameAt) / 1000)}s; aborting half-open ws`,
      );
      connAbort.abort();
      return;
    }
    if (now - liveness.lastFrameAt >= HEARTBEAT_PING_INTERVAL_MS) {
      try {
        ws.send(encodeFrame({ kind: "ping" }));
      } catch (err) {
        logger.warn("heartbeat ping send failed", err);
      }
    }
  }, HEARTBEAT_TICK_MS);
  // Unref so a running heartbeat doesn't block process exit when the
  // sidecar is otherwise idle. `setInterval` returns `Timeout` on
  // Node but `number` on the DOM lib types; guard the unref call.
  if (typeof (timer as { unref?: () => void }).unref === "function") {
    (timer as { unref: () => void }).unref();
  }
  return () => clearInterval(timer);
}

function normalizeBinary(data: WebSocket.RawData): Uint8Array | null {
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) {
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  }
  if (Array.isArray(data)) {
    const total = data.reduce((n, b) => n + b.byteLength, 0);
    const out = new Uint8Array(total);
    let cursor = 0;
    for (const chunk of data) {
      out.set(
        new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength),
        cursor,
      );
      cursor += chunk.byteLength;
    }
    return out;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Outbound: channel.inbound() -> Frame::Message
// ---------------------------------------------------------------------------

async function pumpOutbound(
  channel: Channel,
  ws: WebSocket,
  signal: AbortSignal,
  logger: Logger,
): Promise<void> {
  try {
    for await (const msg of channel.inbound(signal)) {
      if (signal.aborted || ws.readyState !== WebSocket.OPEN) return;
      ws.send(
        encodeFrame({
          kind: "message",
          content: msg.content,
          session_id: msg.sessionId,
          user_id: msg.userId,
          channel_type: channel.channelType,
          // Sidecar inbound is always user-authored. The server's
          // echo path uses this to render the message correctly when
          // it fans out to other subscribers of the same session.
          role: "user",
          ...(msg.botId ? { bot_id: msg.botId } : {}),
          ...(msg.platformMsgId
            ? { platform_msg_id: msg.platformMsgId }
            : {}),
          ...(msg.attachments && msg.attachments.length > 0
            ? { attachments: msg.attachments }
            : {}),
        }),
      );
    }
  } catch (err) {
    if (!signal.aborted) {
      logger.error("inbound generator threw", err);
    }
  }
}

// ---------------------------------------------------------------------------
// Inbound dispatch: Frame -> channel hooks
// ---------------------------------------------------------------------------

async function pumpInbound(
  channel: Channel,
  ws: WebSocket,
  frames: FrameQueue,
  controller: AbortController,
  logger: Logger,
): Promise<void> {
  while (!controller.signal.aborted) {
    let frame: Frame;
    try {
      frame = await frames.pop();
    } catch (err) {
      if (
        err instanceof RunnerError &&
        (err.kind === "peer_closed" || err.kind === "transport")
      ) {
        controller.abort();
        return;
      }
      throw err;
    }
    dispatchFrame(frame, channel, ws, logger);
  }
}

function dispatchFrame(
  frame: Frame,
  channel: Channel,
  ws: WebSocket,
  logger: Logger,
): void {
  switch (frame.kind) {
    case "message": {
      // Defensive: a `Frame::Message` with `role: "user"` should only
      // ever reach a Subscribed-kind subscriber (the web SPA, the
      // TUI). It must NOT reach a Multiplexed-kind sidecar — the
      // SDK's `onMessage` forwards to `platform.sendText`, which
      // would echo the user's own input back to the upstream
      // platform. The gateway already guards this at the source (see
      // `Channel::echo_inbound` early-return on Multiplexed), so a
      // user-role message reaching this point is a protocol violation
      // / regression; drop it loudly rather than forwarding it.
      if (frame.role === "user") {
        logger.warn(
          "dropping user-role Message frame; sidecars should only receive agent-authored messages",
          { sessionId: frame.session_id, userId: frame.user_id },
        );
        return;
      }
      void safeInvoke(
        () =>
          channel.onMessage({
            sessionId: frame.session_id,
            userId: frame.user_id,
            content: frame.content,
            ...(frame.attachments && frame.attachments.length > 0
              ? { attachments: frame.attachments }
              : {}),
          }),
        "onMessage",
        logger,
      );
      return;
    }
    case "answer_delta": {
      if (!channel.onDelta) return;
      void safeInvoke(
        () =>
          channel.onDelta!({
            sessionId: frame.session_id,
            userId: frame.user_id ?? "",
            text: frame.text,
          }),
        "onDelta",
        logger,
      );
      return;
    }
    case "notice": {
      if (!channel.onNotice) return;
      const level: NoticeLevel =
        frame.level === "error"
          ? "error"
          : frame.level === "info"
            ? "info"
            : "warn";
      void safeInvoke(
        () =>
          channel.onNotice!({
            sessionId: frame.session_id,
            userId: frame.user_id ?? "",
            level,
            text: frame.text,
          }),
        "onNotice",
        logger,
      );
      return;
    }
    case "tool_started": {
      if (!channel.onToolStarted) return;
      void safeInvoke(
        () =>
          channel.onToolStarted!({
            sessionId: frame.session_id,
            userId: frame.user_id ?? "",
            callId: frame.call_id,
            tool: frame.tool,
            ...(frame.label !== undefined ? { label: frame.label } : {}),
          }),
        "onToolStarted",
        logger,
      );
      return;
    }
    case "tool_completed": {
      if (!channel.onToolCompleted) return;
      void safeInvoke(
        () =>
          channel.onToolCompleted!({
            sessionId: frame.session_id,
            userId: frame.user_id ?? "",
            callId: frame.call_id,
            status: normalizeToolStatus(frame.status),
            summary: frame.summary,
          }),
        "onToolCompleted",
        logger,
      );
      return;
    }
    case "status": {
      if (!channel.onStatus) return;
      void safeInvoke(
        () =>
          channel.onStatus!({
            sessionId: frame.session_id,
            userId: frame.user_id ?? "",
            phase: frame.phase,
          }),
        "onStatus",
        logger,
      );
      return;
    }
    case "reasoning":
      // Thinking trace — no bot-side surface today; drop it.
      return;
    case "approval_requested": {
      void handleApproval(frame, channel, ws, logger);
      return;
    }
    case "approval_resolved": {
      if (!channel.onApprovalResolved) return;
      const decision = normalizeDecision(frame.decision);
      if (!decision) {
        logger.warn("unknown approval decision", frame.decision);
        return;
      }
      void safeInvoke(
        () => channel.onApprovalResolved!(frame.call_id, decision),
        "onApprovalResolved",
        logger,
      );
      return;
    }
    case "start_bot": {
      void handleBotCommand(
        "onStartBot",
        frame.bot_id,
        channel.onStartBot
          ? () => channel.onStartBot!({ botId: frame.bot_id, token: frame.token })
          : null,
        ws,
        logger,
      );
      return;
    }
    case "stop_bot": {
      void handleBotCommand(
        "onStopBot",
        frame.bot_id,
        channel.onStopBot
          ? () => channel.onStopBot!({ botId: frame.bot_id })
          : null,
        ws,
        logger,
      );
      return;
    }
    case "slash_manifest": {
      if (!channel.onSlashManifest) return;
      void safeInvoke(
        () => channel.onSlashManifest!(frame.commands),
        "onSlashManifest",
        logger,
      );
      return;
    }
    // History frames are TUI-only; sidecars silently drop them.
    case "history_append":
    case "history_snapshot":
      return;
    // SessionUpdated / SessionActivity drive the web chat sidebar;
    // sidecars don't have a session list and silently drop them.
    case "session_updated":
    case "session_activity":
      return;
    // Subscribe / Unsubscribe are selective-channel client→server
    // frames. Broadcast-kind sidecars never send them, and the
    // gateway never sends them in this direction. If one shows up
    // it's protocol noise — log and ignore.
    case "subscribe":
    case "unsubscribe":
      logger.warn("unexpected subscribe/unsubscribe on sidecar", frame.kind);
      return;
    // PendingApprovalsSnapshot is the server's reply to a Subscribe;
    // broadcast-kind sidecars don't subscribe so they never see it.
    case "pending_approvals_snapshot":
      logger.warn("unexpected pending_approvals_snapshot on sidecar", frame.kind);
      return;
    // Reset means "your live stream is stale". For a broadcast
    // sidecar the right reaction is to log and rely on auto-reconnect
    // (subsequent server output will flow normally after the WS
    // re-handshakes).
    case "reset": {
      logger.warn("server requested reset", frame.reason);
      safeClose(ws);
      return;
    }
    case "ping": {
      // Forward-compat: a future server-initiated keepalive should
      // still see a Pong from the sidecar so its own watchdog can
      // tell the connection apart from a stuck client.
      if (ws.readyState === WebSocket.OPEN) {
        try {
          ws.send(encodeFrame({ kind: "pong" }));
        } catch (err) {
          logger.warn("reply pong failed", err);
        }
      }
      return;
    }
    case "pong":
      // Pure liveness signal — `attachWsHandlers` already bumped
      // `lastFrameAt` on receipt, which is what the watchdog reads.
      return;
    // These are client->server shapes the sidecar will never receive.
    case "register":
    case "register_ack":
    case "resolve_approval":
    case "bot_status":
      logger.warn("unexpected server->client frame", frame.kind);
      return;
  }
}

/**
 * Run a bot-control hook and encode a `BotStatus` ack on the WS.
 * Errors thrown by the hook become `ok: false` acks so aura always
 * hears back about every `Start`/`StopBot` it dispatched. A sidecar
 * that doesn't implement the hook at all acks with `ok: false` and a
 * "not supported" message so the admin dashboard surfaces the gap
 * instead of silently dropping the command.
 */
async function handleBotCommand(
  label: string,
  botId: string,
  runner: (() => Promise<BotStatusReport>) | null,
  ws: WebSocket,
  logger: Logger,
): Promise<void> {
  let report: BotStatusReport;
  if (!runner) {
    logger.warn(
      `${label} command ignored: sidecar did not implement the hook`,
      botId,
    );
    report = { botId, ok: false, message: `${label} not supported` };
  } else {
    try {
      report = await runner();
    } catch (err) {
      logger.error(`${label} threw`, err);
      report = {
        botId,
        ok: false,
        message: err instanceof Error ? err.message : String(err),
      };
    }
  }
  if (ws.readyState !== WebSocket.OPEN) return;
  try {
    ws.send(
      encodeFrame({
        kind: "bot_status",
        bot_id: report.botId,
        ok: report.ok,
        ...(report.message !== undefined ? { message: report.message } : {}),
      }),
    );
  } catch (err) {
    logger.error("failed to send bot_status", err);
  }
}

async function handleApproval(
  frame: Frame & { kind: "approval_requested" },
  channel: Channel,
  ws: WebSocket,
  logger: Logger,
): Promise<void> {
  if (!channel.onApprovalRequested) return;
  let decision: ApprovalDecision;
  try {
    decision = await channel.onApprovalRequested({
      callId: frame.call_id,
      sessionId: frame.session_id,
      userId: frame.user_id ?? "",
      tool: frame.tool,
      accesses: frame.accesses,
      paramsPreview: frame.params_preview,
      ...(frame.description != null ? { description: frame.description } : {}),
    });
  } catch (err) {
    logger.error("onApprovalRequested threw; defaulting to deny", err);
    decision = "deny";
  }
  if (ws.readyState !== WebSocket.OPEN) return;
  try {
    ws.send(
      encodeFrame({
        kind: "resolve_approval",
        call_id: frame.call_id,
        decision,
      }),
    );
  } catch (err) {
    logger.error("failed to send resolve_approval", err);
  }
}

/**
 * Exposed for the package's own tests — the WS `approval_resolved`
 * frame carries `decision: string` because the wire schema stays
 * untyped across languages. `normalizeDecision` is the sole check
 * that the value is one of the three Rust variants before the runner
 * dispatches it.
 */
export function normalizeDecision(raw: string): ApprovalDecision | null {
  if (raw === "approve" || raw === "approve_always" || raw === "deny") {
    return raw;
  }
  return null;
}

/** Map the wire's lower-case tool status to the typed union; anything
 * unexpected reads as an error so a degraded call never looks `ok`. */
function normalizeToolStatus(raw: string): ToolStatus {
  return raw === "ok" || raw === "denied" ? raw : "error";
}

async function safeInvoke(
  fn: () => Promise<void>,
  label: string,
  logger: Logger,
): Promise<void> {
  try {
    await fn();
  } catch (err) {
    logger.error(`${label} threw`, err);
  }
}
