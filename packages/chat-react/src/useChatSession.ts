// React glue over @aura/chat-core: owns one chat session's live lifecycle —
// the ChatWs connection, history hydration, server-authoritative frame routing
// (routeInboundFrame), and optimistic send / stop / approval — and hands the
// rendering app a ready-to-render SessionView plus action callbacks. Keeps
// chat-core itself framework-agnostic; this is the only React-dependent layer.
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  ChatWs,
  EMPTY_VIEW,
  closeActiveWork,
  historyRowToTranscript,
  isStopCancellationNotice,
  isStopCommand,
  routeInboundFrame,
  type ConnectionStatus,
  type HistoryRowDto,
  type SessionView,
  type WireAttachment,
} from '@aura/chat-core';

/** A session's REST history, normalized for the hook. The app supplies this
 *  (the openapi client + DTO shape differ per app), so the hook stays free of
 *  any specific API client. */
export interface ChatHistory {
  transcript: HistoryRowDto[];
  oldestOrdinal: number | null;
  newestOrdinal: number | null;
  hasMore: boolean;
  /** Per-session pinned model (`last_llm`), or null to follow `default-llm`. */
  model: string | null;
}

export interface UseChatSessionOptions {
  /** Channel-listener base URL (same origin as the admin listener). */
  baseUrl: string;
  /** The session this hook instance drives. Changing it rebuilds the socket. */
  sessionId: string;
  /** `user_id` stamped on outgoing user messages. */
  userId: string;
  /** Mint/refresh a channel token for `sessionId`. Called once to connect and
   *  again whenever the server rejects the current token. */
  mintToken: () => Promise<string>;
  /** Fetch REST history for `sessionId`, or null when there's nothing yet. */
  fetchHistory: () => Promise<ChatHistory | null>;
  /** Called for each user message so the app can refresh its sidebar preview. */
  onUserPreview?: (sessionId: string, preview: string) => void;
  /** Called on `Frame::SessionActivity` so the app can refresh its session list. */
  onActivity?: () => void;
}

export interface ChatSession {
  /** The session's live view-model — render this. */
  view: SessionView;
  /** WS connection state for a header badge. */
  status: ConnectionStatus['state'];
  /** A turn is in flight (optimistically or per server `turn_state`): the
   *  composer should show Stop, and block new sends. */
  busy: boolean;
  /** Pinned model (from history / picker), null = follow default-llm. */
  model: string | null;
  /** Optimistically set the pinned model (the app still persists it). */
  setModel: (model: string) => void;
  /** Current channel token — needed for blob uploads. Stable identity. */
  getToken: () => string;
  /** Send a user message (or a `/stop`, which is routed to {@link stop}).
   *  Inserts an optimistic, `clientMsgId`-keyed bubble reconciled on echo. */
  send: (text: string, attachments?: WireAttachment[]) => void;
  /** Optimistically cancel the in-flight turn and tell the server (`/stop`). */
  stop: () => void;
  resolveApproval: (callId: string, decision: 'approve' | 'approve_always' | 'deny') => void;
}

export function useChatSession({
  baseUrl,
  sessionId,
  userId,
  mintToken,
  fetchHistory,
  onUserPreview,
  onActivity,
}: UseChatSessionOptions): ChatSession {
  // One per-session bucket map (shared shape with web). This hook instance
  // drives a single session, but routing through routeInboundFrame keeps the
  // reducer identical to web's.
  const [views, setViews] = useState<Record<string, SessionView>>({});
  const view = views[sessionId] ?? EMPTY_VIEW;
  const busy = view.awaitingReply || (view.turn?.active ?? false);
  const [status, setStatus] = useState<ConnectionStatus['state']>('connecting');
  const [model, setModel] = useState<string | null>(null);

  const wsRef = useRef<ChatWs | null>(null);
  const tokenRef = useRef<string>('');
  const newestOrdinalRef = useRef<number>(-1);
  const lastConnectedAtRef = useRef<number>(0);
  // Sessions just `/stop`'d locally: a late `answer_delta` the server had
  // already put on the wire is dropped instead of spilling a stray bubble
  // below the now-collapsed work block. Cleared when a new turn starts.
  const stoppedRef = useRef<Set<string>>(new Set());

  // Latest callbacks in refs so the WS effect doesn't tear the socket down when
  // a parent re-render hands fresh callback identities.
  const mintTokenRef = useRef(mintToken);
  const fetchHistoryRef = useRef(fetchHistory);
  const onUserPreviewRef = useRef(onUserPreview);
  const onActivityRef = useRef(onActivity);
  useEffect(() => {
    mintTokenRef.current = mintToken;
    fetchHistoryRef.current = fetchHistory;
    onUserPreviewRef.current = onUserPreview;
    onActivityRef.current = onActivity;
  });

  // Load history, seed the model + cursor, then open the live stream.
  useEffect(() => {
    let ws: ChatWs | null = null;
    let cancelled = false;

    void (async () => {
      const hist = await fetchHistoryRef.current();
      if (cancelled) return;
      if (hist) {
        setModel(hist.model);
        const rows = hist.transcript.map((r) => historyRowToTranscript(sessionId, r));
        setViews((prev) => ({
          ...prev,
          [sessionId]: {
            ...EMPTY_VIEW,
            transcript: rows,
            historyLoaded: true,
            oldestOrdinal: hist.oldestOrdinal,
            hasMore: hist.hasMore,
            model: hist.model,
          },
        }));
        newestOrdinalRef.current = hist.newestOrdinal ?? -1;
      }
      const token = await mintTokenRef.current();
      if (cancelled) return;
      tokenRef.current = token;
      ws = new ChatWs({
        baseUrl,
        channelToken: token,
        initialSessionIds: [sessionId],
        onStatus: (s) => {
          setStatus(s.state);
          // The pending-approval snapshot reconciliation uses this cutoff to
          // tell a card from before the reconnect (droppable) from one that
          // raced in after it (kept).
          if (s.state === 'connected') lastConnectedAtRef.current = Date.now();
        },
        onFrame: (frame) => {
          if (frame.kind === 'session_activity') onActivityRef.current?.();
          // A fresh turn supersedes any prior local `/stop`.
          if (frame.kind === 'turn_state' && frame.active) {
            stoppedRef.current.delete(frame.session_id);
          }
          // Drop a delta that raced in after a local `/stop` — the partial
          // answer is already settled inside the collapsed work block.
          if (frame.kind === 'answer_delta' && stoppedRef.current.has(frame.session_id)) {
            return;
          }
          // A broadcast `/stop` cancellation notice stops this tab's stream too
          // (an observer never ran the local `/stop`).
          if (frame.kind === 'notice' && !frame.transient && isStopCancellationNotice(frame.text)) {
            stoppedRef.current.add(frame.session_id);
          }
          routeInboundFrame(
            frame,
            setViews,
            (sid, preview) => onUserPreviewRef.current?.(sid, preview),
            lastConnectedAtRef.current,
          );
        },
        onTokenRejected: () => {
          // Token expired/revoked (commonly a gateway restart on config
          // reload). Mint a fresh one and resume instead of sitting dead.
          void (async () => {
            try {
              const fresh = await mintTokenRef.current();
              if (cancelled) return;
              tokenRef.current = fresh;
              wsRef.current?.replaceToken(fresh);
            } catch {
              /* stay disconnected; the status badge surfaces it */
            }
          })();
        },
      });
      if (newestOrdinalRef.current >= 0) ws.recordOrdinal(sessionId, newestOrdinalRef.current);
      wsRef.current = ws;
    })();

    return () => {
      cancelled = true;
      ws?.close();
      wsRef.current = null;
    };
  }, [baseUrl, sessionId]);

  const getToken = useCallback(() => tokenRef.current, []);

  // Optimistically close the live work block as Cancelled and tell the server.
  const stop = useCallback(() => {
    const ws = wsRef.current;
    if (!ws) return;
    ws.sendMessage({ sessionId, userId, content: '/stop' });
    stoppedRef.current.add(sessionId);
    setViews((prev) => {
      const v = prev[sessionId] ?? EMPTY_VIEW;
      return {
        ...prev,
        [sessionId]: {
          ...v,
          transcript: closeActiveWork(v.transcript, true),
          awaitingReply: false,
          turn: { active: false, startedAt: null },
        },
      };
    });
  }, [sessionId, userId]);

  const send = useCallback(
    (text: string, attachments?: WireAttachment[]) => {
      const ws = wsRef.current;
      if (!ws) return;
      // A `/stop` is a control command, not a chat turn — no optimistic bubble.
      if (isStopCommand(text)) {
        stop();
        return;
      }
      // The agent is already working — block new turns; only `/stop` gets through.
      if (view.awaitingReply || (view.turn?.active ?? false)) return;
      if (!text && (!attachments || attachments.length === 0)) return;
      const clientMsgId = crypto.randomUUID();
      const atts = attachments && attachments.length > 0 ? attachments : undefined;
      // Optimistic user bubble keyed by `clientMsgId`; the server's echo
      // reconciles against it in routeInboundFrame (clears `pending`, keeps the
      // row identity) instead of appending a duplicate.
      setViews((prev) => {
        const v = prev[sessionId] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sessionId]: {
            ...v,
            transcript: [
              ...v.transcript,
              {
                key: `local-${clientMsgId}`,
                role: 'user',
                text,
                pending: true,
                clientMsgId,
                hasAttachments: atts ? true : undefined,
                attachments: atts,
                createdAt: new Date().toISOString(),
              },
            ],
            awaitingReply: true,
          },
        };
      });
      ws.sendMessage({ sessionId, userId, content: text, attachments: atts, clientMsgId });
    },
    [sessionId, userId, view.awaitingReply, view.turn, stop],
  );

  const resolveApproval = useCallback(
    (callId: string, decision: 'approve' | 'approve_always' | 'deny') => {
      wsRef.current?.resolveApproval(callId, decision);
      setViews((prev) => {
        const v = prev[sessionId];
        if (!v?.pendingApproval) return prev;
        return { ...prev, [sessionId]: { ...v, pendingApproval: null } };
      });
    },
    [sessionId],
  );

  return { view, status, busy, model, setModel, getToken, send, stop, resolveApproval };
}
