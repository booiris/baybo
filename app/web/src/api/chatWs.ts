// WebSocket transport for the /chat page.
//
// Connects to /v1/channel-ws with the admin bearer, encodes/decodes
// msgpack-framed Frame values, handles the Register → Subscribe sequence,
// and offers an async iterator over inbound Frames for the UI to consume. Auto-
// reconnect uses exponential backoff (1s, 2s, 4s, … capped at 30s);
// each reconnect re-issues Subscribe for the active session_ids the
// caller is interested in.

import { decode, encode } from '@msgpack/msgpack';
import { uuid } from '../uuid';

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
  /** Persisted `session_messages.ordinal`, stamped onto the live final
   *  assistant reply at emit time so the client can advance its
   *  per-session sync cursor past live emissions. The server's
   *  pre-persist Echo of inbound (role=user) leaves it `undefined` by
   *  design — durability for a send is confirmed by an ordinal-stamped
   *  row from the REST sync/backfill surface, keyed by
   *  `platform_msg_id`. */
  ordinal?: number;
}

export interface ResourceAccess {
  kind: 'read_file' | 'write_file' | 'http' | 'exec_command' | 'env';
  path?: string;
  host?: string;
  command?: string;
  vars?: string[];
}

/** Mirror of Rust `FolderView` — one folder in a `Frame::FoldersChanged`
 *  snapshot. `position` orders siblings within their parent. `parent_id`
 *  absent ⇒ top-level. */
export interface WireFolder {
  id: string;
  parent_id?: string;
  name: string;
  position: number;
  created_at: string;
}

/** Mirror of Rust `FolderChange` — the two-state folder reassignment
 *  carried on `SessionPatch.folder_id`. `{ set: { id } }` files the
 *  session under a folder; `'uncategorized'` clears it. The field being
 *  absent (undefined) means "no change". */
export type FolderChange = { set: { id: string } } | 'uncategorized';

/** Sparse mutation surface — mirror of Rust `SessionPatch`. Every
 *  field independently optional; absent means "no change". A patch
 *  for an unknown session_id constructs a row iff it carries enough
 *  fields to render the sidebar (currently `created_at` +
 *  `last_active`). */
export interface SessionPatch {
  created_at?: string;
  last_active?: string;
  hidden?: boolean;
  /** Flipped by `PUT /v1/chat/sessions/:id/pin`. `true` moves the row
   *  into the sidebar's pinned block; `false` moves it back. */
  pinned?: boolean;
  /** Changed by `PUT /v1/chat/sessions/:id/folder` and on folder delete.
   *  Present means the assignment changed to this value; absent means no
   *  change. `{ set: { id } }` files under a folder; `'uncategorized'`
   *  clears it. */
  folder_id?: FolderChange;
  /** Generated conversation title; absent means no change. */
  title?: string;
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

/** One step inside a turn's in-flight work block, carried in the
 *  `subscribe_state` bundle — mirror of Rust `WireWorkStep`. A `tool`
 *  step carries the call's `call_id` (so a later live `tool_completed`
 *  still pairs by id) plus `status`/`summary` once the call finished
 *  within the buffered turn; `reasoning` / `prose` bodies live in
 *  `text`. */
export interface WireWorkStep {
  kind: 'reasoning' | 'prose' | 'tool' | 'status';
  text?: string;
  call_id?: string;
  tool?: string;
  label?: string;
  status?: string;
  summary?: string;
  /** `'approve'` / `'approve_always'` / `'deny'` — the decision this call's
   *  approval prompt returned, once it completed within the buffered turn.
   *  Absent when the call never prompted. */
  approval?: string;
}

/** One pending tool-approval prompt in the `subscribe_state` bundle —
 *  mirror of Rust `ApprovalCard`: field-compatible with the live
 *  `approval_requested` frame minus `session_id` (the bundle carries it
 *  once). */
export interface WireApprovalCard {
  call_id: string;
  /** `call_id` of the TOOL call this prompt blocks (what `tool_started` /
   *  `tool_completed` carry) — NOT the prompt's own `call_id`, which is minted
   *  per prompt. Lets the work block badge the step that is waiting. */
  tool_call_id?: string;
  user_id?: string;
  tool: string;
  accesses: ResourceAccess[];
  params_preview: string;
  description?: string | null;
}

export type Frame =
  | { kind: 'register'; token: string; channel_type: string }
  | { kind: 'register_ack'; ok: boolean; reason: string | null }
  | { kind: 'subscribe'; session_id: string }
  | { kind: 'unsubscribe'; session_id: string }
  /** Server → client, once, immediately after every Subscribe: the
   *  atomic state-plane REPLACE bundle (turn activity + in-flight work
   *  steps + pending approvals + task list). Empty arrays are OMITTED
   *  on the wire (msgpack skip) — default them to `[]`. The turn/work
   *  halves go stale by turn identity (`started_at`), never by
   *  comparing a sync cursor against `as_of_ordinal`. */
  | {
      kind: 'subscribe_state';
      session_id: string;
      /** Session's newest persisted ordinal at snapshot time. */
      as_of_ordinal?: number | null;
      turn: { active: boolean; started_at?: string | null };
      work_steps?: WireWorkStep[];
      pending_approvals?: WireApprovalCard[];
      tasks?: TaskView[];
    }
  /** Server → client gap nudge: the server dropped frames for this
   *  connection. `session_id` set → run sync for that session;
   *  absent → sync EVERY subscribed session and refetch the session
   *  list + folders (that plane has no cursor). */
  | { kind: 'gap'; session_id?: string | null }
  | ({ kind: 'message' } & WireMessage)
  /** Client → server. Several user messages for one session that the
   *  server runs as a single coalesced turn (the "send every queued
   *  message at once" path). Delivered to the actor atomically so its
   *  coalescing can't lose stragglers to per-message intake latency. */
  | { kind: 'messages'; messages: WireMessage[] }
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
      /** The decision this call's approval prompt returned; absent when it
       *  never prompted. Persisted server-side, so a reload re-labels it. */
      approval?: string;
    }
  | { kind: 'status'; session_id: string; user_id?: string; phase: string }
  | { kind: 'task_list'; session_id: string; user_id?: string; tasks: TaskView[] }
  | {
      kind: 'turn_state';
      session_id: string;
      user_id?: string;
      /** Whether a turn (the session's in-flight reply) is currently
       *  being produced. Broadcast at every turn start/end AND sent as
       *  a snapshot on every Subscribe, so a late joiner (new tab,
       *  reconnect) learns about a turn whose progress frames it never
       *  received. */
      active: boolean;
      /** ISO instant the in-flight turn started; present iff `active`.
       *  Seeds the work block's elapsed timer with true turn age. */
      started_at?: string | null;
    }
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
      /** The blocked TOOL call — see `WireApprovalCard.tool_call_id`. */
      tool_call_id?: string;
      session_id: string;
      user_id?: string;
      tool: string;
      accesses: ResourceAccess[];
      params_preview: string;
      description?: string | null;
    }
  | { kind: 'approval_resolved'; call_id: string; decision: string }
  | { kind: 'resolve_approval'; call_id: string; decision: string }
  | { kind: 'history_append'; session_id: string; entry: string }
  | { kind: 'history_snapshot'; session_id: string; entries: string[] }
  | { kind: 'start_bot'; bot_id: string; token: string }
  | { kind: 'stop_bot'; bot_id: string }
  | { kind: 'bot_status'; bot_id: string; ok: boolean; message?: string }
  | { kind: 'slash_manifest'; commands: { command: string; description: string }[] }
  | { kind: 'folders_changed'; folders: WireFolder[] }
  | { kind: 'session_updated'; session_id: string; patch: SessionPatch }
  | { kind: 'session_activity'; session_id: string; source: ActivityKind; at: string }
  /** Server → client deck stream — no deck UI here; the native deck shell
   *  consumes them. The page ignores them (routeInboundFrame's default arm). */
  | { kind: 'deck_card_data'; card_id: string; seq: number; payload: string }
  | { kind: 'deck_changed' }
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

export type ConnectionStatus =
  | { state: 'connecting' }
  | { state: 'connected' }
  | { state: 'disconnected'; retryInMs: number; lastError?: string };

export interface ChatWsOptions {
  /** Base URL of the channel listener — typically the same origin as
   *  the admin listener in dev/prod (the gateway serves both on one
   *  axum router). */
  baseUrl: string;
  /** Admin bearer token for the gateway admin listener. */
  adminToken: string;
  /** Initial session_ids to subscribe to after RegisterAck. The
   *  caller is free to mutate the subscription set later via
   *  {@link ChatWs.subscribe} / {@link ChatWs.unsubscribe}. */
  initialSessionIds?: string[];
  /** Callback for every inbound frame after a successful handshake.
   *  Called from the WS message handler. */
  onFrame: (frame: Frame) => void;
  /** Callback for connection state changes. */
  onStatus?: (status: ConnectionStatus) => void;
}

export class ChatWs {
  private ws: WebSocket | null = null;
  private adminToken: string;
  /** Sessions this connection subscribes to. Replayed on every
   *  (re)connect so the live stream resumes; transcript recovery is
   *  NOT the subscription's job — the caller runs the REST sync loop
   *  (`GET …/sync?since_ordinal`) on each reconnect edge. */
  private subscriptions = new Set<string>();
  private retryAttempt = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private closed = false;
  /** Combined heartbeat-send + liveness-watchdog tick. `null` while
   *  disconnected. Set in {@link startHeartbeat} after RegisterAck. */
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  /** Epoch ms of the most recently received frame on the current WS.
   *  Updated on every inbound (including Pong); compared against
   *  {@link HEARTBEAT_LIVENESS_TIMEOUT_MS} to detect half-open TCP
   *  state that the browser would otherwise hide. */
  private lastFrameAt = 0;
  constructor(private readonly opts: ChatWsOptions) {
    this.adminToken = opts.adminToken;
    for (const sid of opts.initialSessionIds ?? []) this.subscriptions.add(sid);
    this.connect();
  }

  /** Subscribe this connection to one more session. Sends the frame
   *  on the next available WS open; persisted across reconnects. */
  subscribe(sessionId: string): void {
    if (this.subscriptions.has(sessionId)) return;
    this.subscriptions.add(sessionId);
    this.sendFrame({ kind: 'subscribe', session_id: sessionId });
  }

  /** Sessions currently subscribed — the set the caller must sync on a
   *  reconnect edge or a session-less `gap` nudge. */
  subscribedSessions(): string[] {
    return [...this.subscriptions];
  }

  /** Drop one subscription. */
  unsubscribe(sessionId: string): void {
    if (!this.subscriptions.delete(sessionId)) return;
    this.sendFrame({ kind: 'unsubscribe', session_id: sessionId });
  }

  /** Send a user-authored Message frame for `sessionId`. The optional
   *  `clientMsgId` is the idempotency key surfaced to the gateway as
   *  `platform_msg_id`: if the call yields a duplicate (re-Send after
   *  the WS dropped between send and echo, double-click on the button,
   *  …) the gateway's `InboundDedup` rejects the retry inside its
   *  recency window instead of producing a second agent turn for the
   *  same user message. A fresh `uuid()` is generated
   *  per call when the caller doesn't supply one. */
  sendMessage(input: {
    sessionId: string;
    userId: string;
    content: string;
    channelType?: string;
    attachments?: WireAttachment[];
    clientMsgId?: string;
  }): void {
    const msgId = input.clientMsgId ?? uuid();
    this.sendFrame({
      kind: 'message',
      content: input.content,
      session_id: input.sessionId,
      user_id: input.userId,
      channel_type: input.channelType ?? 'owner',
      role: 'user',
      platform_msg_id: msgId,
      ...(input.attachments && input.attachments.length > 0
        ? { attachments: input.attachments }
        : {}),
    });
  }

  /** Send several user messages for one session as a single batch frame.
   *  The server runs them as one coalesced turn (one reply) while keeping
   *  each as its own transcript row — used by the web "fire every queued
   *  message at once" path so they merge deterministically instead of
   *  racing the per-message intake. Each entry's `clientMsgId` rides as
   *  `platform_msg_id` for the same optimistic-row reconciliation +
   *  dedup as {@link sendMessage}. */
  sendMessages(
    sessionId: string,
    messages: { content: string; clientMsgId: string; attachments?: WireAttachment[] }[],
    channelType = 'owner',
  ): void {
    if (messages.length === 0) return;
    this.sendFrame({
      kind: 'messages',
      messages: messages.map((m) => ({
        content: m.content,
        session_id: sessionId,
        user_id: 'web-operator',
        channel_type: channelType,
        role: 'user' as const,
        platform_msg_id: m.clientMsgId,
        ...(m.attachments && m.attachments.length > 0 ? { attachments: m.attachments } : {}),
      })),
    });
  }

  /** Echo a ResolveApproval back. */
  resolveApproval(callId: string, decision: 'approve' | 'approve_always' | 'deny'): void {
    this.sendFrame({ kind: 'resolve_approval', call_id: callId, decision });
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
    if (this.closed) return;
    this.notifyStatus({ state: 'connecting' });
    const url = buildWsUrl(this.opts.baseUrl, this.adminToken);
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
      channel_type: 'owner',
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
          this.detachAndCloseWs();
          this.notifyStatus({
            state: 'disconnected',
            retryInMs: 0,
            lastError: `register rejected: ${reason}`,
          });
          this.scheduleReconnect(reason);
          return;
        }
        this.retryAttempt = 0;
        this.notifyStatus({ state: 'connected' });
        // Replay every subscription so the live stream resumes; each
        // Subscribe is answered with a `subscribe_state` snapshot. The
        // caller recovers missed transcript rows via the REST sync
        // loop on the connected edge — the server replays nothing.
        for (const sid of this.subscriptions) {
          this.sendFrame({ kind: 'subscribe', session_id: sid });
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
    this.scheduleReconnect(reason);
  }

  private scheduleReconnect(reason: string): void {
    if (this.closed) return;
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
  // Browser WebSocket cannot set Authorization, so the admin auth
  // middleware accepts this query-param form and strips it before tracing.
  u.searchParams.set('token', token);
  return u.toString();
}
