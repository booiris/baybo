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

export type ChatListEvent = 'created' | 'hidden' | 'unhidden';

export type Frame =
  | { kind: 'register'; token: string; channel_type: string }
  | { kind: 'register_ack'; ok: boolean; reason: string | null }
  | { kind: 'subscribe'; session_id: string; since_ordinal?: number }
  | { kind: 'unsubscribe'; session_id: string }
  | { kind: 'reset'; reason: string }
  | ({ kind: 'message' } & WireMessage)
  | { kind: 'delta'; session_id: string; user_id?: string; text: string }
  | { kind: 'notice'; session_id: string; user_id?: string; level: string; text: string }
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
  | { kind: 'chat_session_list_changed'; event: ChatListEvent; session_id: string };

const RECONNECT_BASE_MS = 1000;
const RECONNECT_MAX_MS = 30_000;

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
   *  seen. Lower ordinals are ignored (cursor never goes backwards). */
  recordOrdinal(sessionId: string, ordinal: number): void {
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

  private connect(): void {
    if (this.closed || this.suspended) return;
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
        this.notifyStatus({ state: 'connected' });
        // Replay every subscription. Cursor (if any) tells the server
        // what we've already seen so it can stream catch-up frames for
        // messages that landed while we were offline.
        for (const sid of this.subscriptions.keys()) {
          this.sendSubscribe(sid);
        }
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
    this.ws = null;
    if (this.closed) return;
    this.scheduleReconnect(`ws close (${e.code}${e.reason ? `: ${e.reason}` : ''})`);
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
