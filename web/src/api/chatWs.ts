// WebSocket transport for the /chat page.
//
// Connects to /v1/channel-ws with a channel-token minted by
// POST /v1/chat/sessions, encodes/decodes msgpack-framed Frame
// values, handles the Register → Subscribe sequence, and offers an
// async iterator over inbound Frames for the UI to consume. Auto-
// reconnect uses exponential backoff (1s, 2s, 4s, … capped at 30s);
// each reconnect re-issues Subscribe for the active session_ids the
// caller is interested in.

import { decode, encode } from '@msgpack/msgpack';

// Wire frame shape pulled straight from the Rust ts-rs generation.
// We re-declare a lightweight version here because the shipped
// channel-ts SDK targets Node (it imports `ws`), but the Frame type
// itself is a discriminated union on `kind` and is platform-agnostic.
// The exhaustive shape mirrors `crates/channels/src/wire.rs`.
export type WireRole = 'user' | 'assistant';

export interface WireAttachment {
  kind: 'image' | 'audio' | 'file';
  blob_id: string;
  mime_type: string;
  size: number;
  filename?: string;
}

export interface WireMessage {
  content: string;
  session_id: string;
  user_id: string;
  channel_type: string;
  bot_id?: string;
  attachments?: WireAttachment[];
  platform_msg_id?: string;
  role: WireRole;
  /** Persisted `session_messages.ordinal`, set by the server on
   *  catch-up replays so the client can advance its per-session
   *  cursor. Live emissions (echo / agent reply at emit time) leave
   *  this `undefined`. */
  ordinal?: number;
}

export interface ResourceAccess {
  kind: 'read_file' | 'write_file' | 'http' | 'exec_command' | 'env';
  path?: string;
  host?: string;
  command?: string;
  vars?: string[];
}

/** Sparse mutation surface — mirror of Rust `SessionPatch`. Every
 *  field independently optional; absent means "no change". A patch
 *  for an unknown session_id constructs a row iff it carries enough
 *  fields to render the sidebar (currently `created_at` +
 *  `last_active`). */
export interface SessionPatch {
  created_at?: string;
  last_active?: string;
  hidden?: boolean;
}

/** Source of a `Frame::SessionActivity` event — mirror of Rust
 *  `ActivityKind`. `user` = a user message landed on the session
 *  (typed in another tab or arrived via a non-http channel);
 *  `assistant` = the agent emitted toward the session (delta,
 *  message, or notice). */
export type ActivityKind = 'user' | 'assistant';

/** One entry in a session's planning checklist — mirror of Rust
 *  `TaskView`. `subject` is the title; `status` is one of `pending` /
 *  `in_progress` / `completed` (kept as a `string` to match the wire,
 *  narrowed at the render site). `depends_on` lists the ids of tasks
 *  this one waits on; absent when it has no prerequisites. */
export interface TaskView {
  id: string;
  subject: string;
  status: string;
  depends_on?: string[];
}

export type Frame =
  | { kind: 'register'; token: string; channel_type: string }
  | { kind: 'register_ack'; ok: boolean; reason: string | null }
  | { kind: 'subscribe'; session_id: string; since_ordinal?: number }
  | { kind: 'unsubscribe'; session_id: string }
  | { kind: 'reset'; reason: string }
  | ({ kind: 'message' } & WireMessage)
  | { kind: 'answer_delta'; session_id: string; user_id?: string; text: string }
  | { kind: 'reasoning'; session_id: string; user_id?: string; text: string }
  | {
      kind: 'tool_started';
      session_id: string;
      user_id?: string;
      call_id: string;
      tool: string;
      label?: string;
    }
  | {
      kind: 'tool_completed';
      session_id: string;
      user_id?: string;
      call_id: string;
      status: string;
      summary: string;
    }
  | { kind: 'status'; session_id: string; user_id?: string; phase: string }
  | { kind: 'task_list'; session_id: string; user_id?: string; tasks: TaskView[] }
  | {
      kind: 'notice';
      session_id: string;
      user_id?: string;
      level: string;
      text: string;
      /** `true` for a transient mid-turn progress update (the progress
       *  observer), which must fold into the open work block rather than
       *  end the turn. Absent/false for a terminal notice. */
      transient?: boolean;
    }
  | {
      kind: 'approval_requested';
      call_id: string;
      session_id: string;
      user_id?: string;
      tool: string;
      accesses: ResourceAccess[];
      params_preview: string;
      description?: string | null;
    }
  | { kind: 'approval_resolved'; call_id: string; decision: string }
  | { kind: 'resolve_approval'; call_id: string; decision: string }
  | { kind: 'pending_approvals_snapshot'; session_id: string; call_ids: string[] }
  | { kind: 'history_append'; session_id: string; entry: string }
  | { kind: 'history_snapshot'; session_id: string; entries: string[] }
  | { kind: 'start_bot'; bot_id: string; token: string }
  | { kind: 'stop_bot'; bot_id: string }
  | { kind: 'bot_status'; bot_id: string; ok: boolean; message?: string }
  | { kind: 'slash_manifest'; commands: { command: string; description: string }[] }
  | { kind: 'session_updated'; session_id: string; patch: SessionPatch }
  | { kind: 'session_activity'; session_id: string; source: ActivityKind; at: string }
  | { kind: 'ping' }
  | { kind: 'pong' };

const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 30_000;

/** How often the client sends an app-level Ping while connected.
 *  Tuned below the typical NAT idle window (30–120 s) so the same
 *  traffic that probes the server also keeps the path alive. */
const HEARTBEAT_PING_INTERVAL_MS = 20_000;
/** No frame at all (Pong, Delta, anything) for this long means we
 *  treat the WS as half-open and force-close. 2× the ping cadence
 *  plus slack so a single dropped Pong doesn't flap the connection. */
const HEARTBEAT_LIVENESS_TIMEOUT_MS = 45_000;
/** How often the watchdog wakes up to compare `lastFrameAt` against
 *  the liveness budget. Fine-grained enough to fire within ~5 s of
 *  the actual deadline without busy-spinning. */
const HEARTBEAT_TICK_MS = 5_000;

/** How many consecutive WS attempts that closed before reaching
 *  `register_ack` constitute a "token presumed dead" signal that
 *  triggers {@link ChatWsOptions.onTokenRejected}. A browser
 *  WebSocket can't see the HTTP upgrade status, so a 401 from the
 *  gateway's channel-auth middleware reaches us only as `onclose`
 *  with no prior frames. Two in a row rules out a one-off transient
 *  (DNS hiccup, server still booting) while still recovering in
 *  ~3 seconds via the existing backoff ladder. */
const PRESUME_TOKEN_DEAD_THRESHOLD = 2;

export type ConnectionStatus =
  | { state: 'connecting' }
  | { state: 'connected' }
  | { state: 'disconnected'; retryInMs: number; lastError?: string };

export interface ChatWsOptions {
  /** Base URL of the channel listener — typically the same origin as
   *  the admin listener in dev/prod (the gateway serves both on one
   *  axum router). */
  baseUrl: string;
  /** Channel-token from `POST /v1/chat/sessions`. */
  channelToken: string;
  /** Initial session_ids to subscribe to after RegisterAck. The
   *  caller is free to mutate the subscription set later via
   *  {@link ChatWs.subscribe} / {@link ChatWs.unsubscribe}. */
  initialSessionIds?: string[];
  /** Callback for every inbound frame after a successful handshake.
   *  Called from the WS message handler. */
  onFrame: (frame: Frame) => void;
  /** Callback for connection state changes. */
  onStatus?: (status: ConnectionStatus) => void;
  /** Fired when the server replies `register_ack { ok: false }`. The
   *  socket pauses auto-reconnect at this point; the caller is
   *  expected to mint a fresh channel-token and call
   *  {@link ChatWs.replaceToken} to resume. If the caller does
   *  nothing, the connection stays disconnected. */
  onTokenRejected?: (reason: string) => void;
  /** Fired when the server sends `Frame::Reset` — typically because
   *  the client's `since_ordinal` cursor implied a catch-up gap
   *  larger than the gateway's WS replay cap, or because the live
   *  stream is otherwise in an indeterminate state (slow-consumer
   *  drop, server-side reconfiguration, …). The caller is expected
   *  to invalidate any cached transcript for affected sessions and
   *  refetch via the REST `/v1/chat/sessions/:id` endpoint. The WS
   *  client clears its own per-session cursors automatically before
   *  firing this — the cursor itself is what triggered the Reset, so
   *  reusing it on the post-reconnect Subscribe would just produce
   *  another Reset. The socket is then torn down and the existing
   *  reconnect path takes over. */
  onReset?: (reason: string) => void;
}

export class ChatWs {
  private ws: WebSocket | null = null;
  private channelToken: string;
  /** Per-session subscription state. The value is the highest
   *  persisted `ordinal` the client has ever seen for that session;
   *  `undefined` means "no persisted cursor yet" (fresh subscribe).
   *  Sent back to the server as `Subscribe.since_ordinal` so a
   *  reconnect after a network dip gets the messages that landed
   *  during the gap streamed back as `Frame::Message` rather than
   *  silently dropped. */
  private subscriptions = new Map<string, number | undefined>();
  private retryAttempt = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private closed = false;
  /** Paused after the server rejected our token — auto-reconnect
   *  stops until {@link replaceToken} swaps in a fresh one. */
  private suspended = false;
  /** Combined heartbeat-send + liveness-watchdog tick. `null` while
   *  disconnected. Set in {@link startHeartbeat} after RegisterAck. */
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  /** Epoch ms of the most recently received frame on the current WS.
   *  Updated on every inbound (including Pong); compared against
   *  {@link HEARTBEAT_LIVENESS_TIMEOUT_MS} to detect half-open TCP
   *  state that the browser would otherwise hide. */
  private lastFrameAt = 0;
  /** True once the current connect attempt received `register_ack
   *  { ok: true }`. Reset on every {@link connect}; consulted by
   *  {@link onClose} to tell apart "WS closed mid-session" from "WS
   *  never came up". A 401 on the HTTP upgrade closes the socket
   *  before a single frame arrives, so the only signal we have for
   *  auth-time rejection from a browser WebSocket is "connect → close
   *  without ever flipping `reachedConnectedThisAttempt = true`". */
  private reachedConnectedThisAttempt = false;
  /** Consecutive WS attempts that died before reaching RegisterAck.
   *  Reset on a successful RegisterAck. When it crosses
   *  {@link PRESUME_TOKEN_DEAD_THRESHOLD} we presume the channel
   *  token is dead (revoked, gateway restart, …) and route to
   *  `onTokenRejected` so the caller can re-mint. */
  private consecutivePreAckCloses = 0;

  constructor(private readonly opts: ChatWsOptions) {
    this.channelToken = opts.channelToken;
    for (const sid of opts.initialSessionIds ?? []) this.subscriptions.set(sid, undefined);
    this.connect();
  }

  /** Subscribe this connection to one more session. Sends the frame
   *  on the next available WS open; persisted across reconnects. */
  subscribe(sessionId: string): void {
    if (this.subscriptions.has(sessionId)) return;
    this.subscriptions.set(sessionId, undefined);
    this.sendSubscribe(sessionId);
  }

  /** Tell the WS about a persisted `ordinal` the caller observed for
   *  `sessionId` — typically from a REST history fetch or a catch-up
   *  `Frame::Message`. Lifts the per-session cursor so the next
   *  Subscribe after a reconnect tells the server what we've already
   *  seen. Lower ordinals are ignored (cursor never goes backwards).
   *
   *  The sentinel `-1` is meaningful: callers seed it after the REST
   *  history fetch settles (even when the transcript is empty) so a
   *  brand-new session's reconnect carries `since_ordinal: -1`, which
   *  tells the server to replay every persisted row. Without it the
   *  next Subscribe would omit `since_ordinal` and miss messages
   *  that landed during the disconnect.
   *
   *  No-op if the caller hasn't subscribed to this session — we
   *  refuse to silently insert a phantom subscription entry that
   *  would later cause `subscribe(sessionId)` to skip its `if
   *  this.subscriptions.has(...)` early-return. */
  recordOrdinal(sessionId: string, ordinal: number): void {
    if (!this.subscriptions.has(sessionId)) return;
    const current = this.subscriptions.get(sessionId);
    if (current !== undefined && current >= ordinal) return;
    this.subscriptions.set(sessionId, ordinal);
  }

  /** Drop one subscription. */
  unsubscribe(sessionId: string): void {
    if (!this.subscriptions.delete(sessionId)) return;
    this.sendFrame({ kind: 'unsubscribe', session_id: sessionId });
  }

  private sendSubscribe(sessionId: string): void {
    const cursor = this.subscriptions.get(sessionId);
    this.sendFrame(
      cursor === undefined
        ? { kind: 'subscribe', session_id: sessionId }
        : { kind: 'subscribe', session_id: sessionId, since_ordinal: cursor },
    );
  }

  /** Send a user-authored Message frame for `sessionId`. The optional
   *  `clientMsgId` is the idempotency key surfaced to the gateway as
   *  `platform_msg_id`: if the call yields a duplicate (re-Send after
   *  the WS dropped between send and echo, double-click on the button,
   *  …) the gateway's `InboundDedup` rejects the retry inside its
   *  recency window instead of producing a second agent turn for the
   *  same user message. A fresh `crypto.randomUUID()` is generated
   *  per call when the caller doesn't supply one. */
  sendMessage(input: {
    sessionId: string;
    userId: string;
    content: string;
    channelType?: string;
    attachments?: WireAttachment[];
    clientMsgId?: string;
  }): void {
    const msgId = input.clientMsgId ?? crypto.randomUUID();
    this.sendFrame({
      kind: 'message',
      content: input.content,
      session_id: input.sessionId,
      user_id: input.userId,
      channel_type: input.channelType ?? 'http',
      role: 'user',
      platform_msg_id: msgId,
      ...(input.attachments && input.attachments.length > 0
        ? { attachments: input.attachments }
        : {}),
    });
  }

  /** Echo a ResolveApproval back. */
  resolveApproval(callId: string, decision: 'approve' | 'approve_always' | 'deny'): void {
    this.sendFrame({ kind: 'resolve_approval', call_id: callId, decision });
  }

  /** Swap the channel-token in (e.g. after the server rejected the
   *  previous one) and reconnect immediately. Clears the suspended
   *  flag so subsequent rejections can re-trigger
   *  `onTokenRejected`. */
  replaceToken(newToken: string): void {
    this.channelToken = newToken;
    this.suspended = false;
    this.retryAttempt = 0;
    this.consecutivePreAckCloses = 0;
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
    this.detachAndCloseWs();
    this.connect();
  }

  /** Tear the connection down permanently. No further reconnects. */
  close(): void {
    this.closed = true;
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }
    this.detachAndCloseWs();
  }

  // ── private ────────────────────────────────────────────────────────

  private detachAndCloseWs(): void {
    this.stopHeartbeat();
    const old = this.ws;
    if (!old) return;
    // Strip handlers before close() so the deferred onclose for THIS
    // socket can't race a fresh one we're about to open in connect().
    old.onopen = null;
    old.onmessage = null;
    old.onerror = null;
    old.onclose = null;
    try {
      old.close();
    } catch {
      /* ignore */
    }
    this.ws = null;
  }

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.lastFrameAt = Date.now();
    this.heartbeatTimer = setInterval(() => this.heartbeatTick(), HEARTBEAT_TICK_MS);
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private heartbeatTick(): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const now = Date.now();
    if (now - this.lastFrameAt > HEARTBEAT_LIVENESS_TIMEOUT_MS) {
      // No frames for too long — the WS is half-open. Closing the
      // socket here lets the existing `onclose` path mark us
      // disconnected and step into the reconnect ladder; the server's
      // next RegisterAck reseeds `lastFrameAt`.
      this.stopHeartbeat();
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      return;
    }
    // Stagger sends so we only probe roughly once per ping interval.
    // The watchdog wakes up more often than that so a wake near the
    // liveness deadline can act before the budget runs out.
    if (now - this.lastFrameAt >= HEARTBEAT_PING_INTERVAL_MS) {
      this.sendFrame({ kind: 'ping' });
    }
  }

  private connect(): void {
    if (this.closed || this.suspended) return;
    this.reachedConnectedThisAttempt = false;
    this.notifyStatus({ state: 'connecting' });
    const url = buildWsUrl(this.opts.baseUrl, this.channelToken);
    let ws: WebSocket;
    try {
      ws = new WebSocket(url);
    } catch (err) {
      this.scheduleReconnect(String(err));
      return;
    }
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    ws.onopen = () => this.onOpen();
    ws.onmessage = (e) => this.onMessage(e);
    ws.onerror = () => this.onError();
    ws.onclose = (e) => this.onClose(e);
  }

  private onOpen(): void {
    this.sendFrame({
      kind: 'register',
      token: '',
      channel_type: 'http',
    });
  }

  private onMessage(e: MessageEvent): void {
    let frame: Frame;
    try {
      const bytes = e.data instanceof ArrayBuffer ? new Uint8Array(e.data) : null;
      if (!bytes) return;
      frame = decode(bytes) as Frame;
    } catch {
      return;
    }
    // Any inbound bumps the liveness watchdog — including Pong, which
    // exists specifically so an otherwise-idle session still has
    // something to track.
    this.lastFrameAt = Date.now();
    switch (frame.kind) {
      case 'register_ack': {
        if (!frame.ok) {
          const reason = frame.reason ?? 'register rejected';
          // Pause auto-reconnect — looping with the same dead token
          // would just hammer the gateway. The caller's
          // onTokenRejected is expected to mint a fresh token and
          // call replaceToken().
          this.suspended = true;
          this.detachAndCloseWs();
          this.notifyStatus({
            state: 'disconnected',
            retryInMs: 0,
            lastError: `token rejected: ${reason}`,
          });
          this.opts.onTokenRejected?.(reason);
          return;
        }
        this.retryAttempt = 0;
        this.reachedConnectedThisAttempt = true;
        this.consecutivePreAckCloses = 0;
        this.notifyStatus({ state: 'connected' });
        // Replay every subscription. Cursor (if any) tells the server
        // what we've already seen so it can stream catch-up frames for
        // messages that landed while we were offline.
        for (const sid of this.subscriptions.keys()) {
          this.sendSubscribe(sid);
        }
        this.startHeartbeat();
        return;
      }
      case 'ping': {
        // Forward-compat for a future server-initiated probe.
        this.sendFrame({ kind: 'pong' });
        return;
      }
      case 'pong': {
        // Pure liveness signal — already bumped `lastFrameAt` above.
        return;
      }
      case 'reset': {
        // Server says our live stream is stale. The post-reconnect
        // Subscribe must NOT re-send the cursor that triggered this
        // Reset — that would just produce another Reset on a loop.
        // Forget every per-session cursor; the consumer is expected
        // to repopulate it via REST history fetch (see onReset
        // contract in ChatWsOptions).
        for (const sid of this.subscriptions.keys()) {
          this.subscriptions.set(sid, undefined);
        }
        this.notifyStatus({
          state: 'disconnected',
          retryInMs: 0,
          lastError: `reset: ${frame.reason}`,
        });
        // Notify the consumer before tearing the socket down so it
        // can clear its transcript view in the same tick as the
        // status flip, avoiding a visible flash of stale rows.
        this.opts.onReset?.(frame.reason);
        if (this.ws) {
          try {
            this.ws.close();
          } catch {
            /* ignore */
          }
        }
        return;
      }
      default:
        this.opts.onFrame(frame);
    }
  }

  private onError(): void {
    // Defer to onclose for the retry plumbing — browsers always fire
    // close after error.
  }

  private onClose(e: CloseEvent): void {
    this.stopHeartbeat();
    this.ws = null;
    if (this.closed) return;
    const reason = `ws close (${e.code}${e.reason ? `: ${e.reason}` : ''})`;
    // A close that lands before `register_ack { ok: true }` arrived
    // is suspicious. The most common cause in production is the
    // gateway's HTTP upgrade rejecting our token (401) — browsers
    // surface that as a plain `onclose` with no preceding frames, so
    // we can't distinguish it from a generic socket failure. After
    // two such attempts in a row we presume the token is dead and
    // route to `onTokenRejected` so the page can re-mint, mirroring
    // the explicit `register_ack { ok: false }` path. The threshold
    // rules out one-off transients (DNS, server still booting).
    if (!this.reachedConnectedThisAttempt) {
      this.consecutivePreAckCloses += 1;
      if (this.consecutivePreAckCloses >= PRESUME_TOKEN_DEAD_THRESHOLD) {
        this.consecutivePreAckCloses = 0;
        this.suspended = true;
        this.notifyStatus({
          state: 'disconnected',
          retryInMs: 0,
          lastError: `token presumed dead after upgrade close: ${reason}`,
        });
        this.opts.onTokenRejected?.(`upgrade-time close: ${reason}`);
        return;
      }
    }
    this.scheduleReconnect(reason);
  }

  private scheduleReconnect(reason: string): void {
    if (this.closed || this.suspended) return;
    const delay = Math.min(
      RECONNECT_BASE_MS * 2 ** this.retryAttempt,
      RECONNECT_MAX_MS,
    );
    this.retryAttempt += 1;
    this.notifyStatus({ state: 'disconnected', retryInMs: delay, lastError: reason });
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.retryTimer = setTimeout(() => {
      this.retryTimer = null;
      this.connect();
    }, delay);
  }

  private sendFrame(frame: Frame): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const bytes = encode(frame);
    ws.send(bytes);
  }

  private notifyStatus(status: ConnectionStatus): void {
    this.opts.onStatus?.(status);
  }
}

function buildWsUrl(baseUrl: string, token: string): string {
  // Same origin as the admin listener in production; the dev Vite
  // proxy rewrites /v1 (including the WS upgrade) to the gateway.
  const u = new URL(baseUrl);
  u.protocol = u.protocol === 'https:' ? 'wss:' : 'ws:';
  u.pathname = '/v1/channel-ws';
  u.search = '';
  u.searchParams.set('token', token);
  return u.toString();
}
