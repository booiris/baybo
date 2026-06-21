import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from 'react';
import { useNavigate, useParams } from 'react-router-dom';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import {
  RiArrowDownLine,
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiAttachmentLine,
  RiCheckLine,
  RiClipboardLine,
  RiCloseLine,
  RiDeleteBin6Line,
  RiFileLine,
  RiLoader4Line,
  RiSendPlane2Line,
  RiStopFill,
} from 'react-icons/ri';

import { useAdminClient, useAuth } from '../api/auth';
import {
  ChatWs,
  EMPTY_VIEW,
  closeActiveWork,
  finalizeTrailingAnswer,
  formatWorkedLabel,
  historyRowToTranscript,
  isSlashText,
  isStopCancellationNotice,
  isStopCommand,
  mergeView,
  routeInboundFrame,
  settleActiveWork,
  workBlockDisplay,
  writeStreamingAnswer,
  type ConnectionStatus,
  type Frame,
  type PendingApproval,
  type ResourceAccess,
  type SessionPatch,
  type SessionView,
  type TranscriptRow,
  type WireAttachment,
  type WorkStep,
} from '@aura/chat-core';
import { CronInbox } from '../components/CronInbox';
import { TaskChecklist } from '../components/chat/TaskChecklist';
import { AttachmentImage } from './chat/AttachmentImage';
import { QueuePanel } from './chat/QueuePanel';
import { SessionSidebar } from './chat/SessionSidebar';
import { useQueueStore, useSessionQueue, type QueuedItem } from './chat/queueStore';
import { useFolderStore } from './chat/folderStore';
import type { SessionSummary } from './chat/types';

type ApprovalDecision = 'approve' | 'approve_always' | 'deny';

/** One selectable model in the header picker, projected from a
 *  `GET /v1/llm/models` entry. `name` is the `aura.json` entry name
 *  (the value `PUT …/model` expects); `provider`/`model` are shown as
 *  the secondary label so two entries on the same provider stay
 *  distinguishable. */
interface ModelOption {
  name: string;
  provider: string;
  model: string;
  isDefault: boolean;
}

/** Soft cap on `views` map size. Past this, the oldest non-active
 *  bucket (by frame recency) is evicted: transcript + pendingApproval
 *  freed, WS subscription dropped, recency entry cleared. Revisit
 *  re-subscribes and re-fetches via REST. Tuned high enough that
 *  casual session-switching stays free; bites only when the user has
 *  genuinely roamed across many conversations in one tab session. */
const VIEW_CACHE_LIMIT = 20;

/** A file the user picked in the composer. Uploaded to the blob store as
 *  soon as it's selected; `blobId` is filled once the upload lands, at which
 *  point it can be attached to the next outgoing message. */
interface PendingAttachment {
  localId: string;
  filename: string;
  mime: string;
  size: number;
  status: 'uploading' | 'ready' | 'error';
  blobId?: string;
  /** Local object URL for an instant composer thumbnail (images only).
   *  Revoked on remove / after send. */
  previewUrl?: string;
}

function attachmentKind(mime: string): WireAttachment['kind'] {
  if (mime.startsWith('image/')) return 'image';
  if (mime.startsWith('audio/')) return 'audio';
  return 'file';
}

export type ComposerAction = 'noop' | 'stop' | 'direct' | 'park';

/** Pure decision for what a composer submit should do, extracted from
 *  `handleSend` so the send-vs-park rule is unit-testable independent of the
 *  component. The rule is intentionally INDEPENDENT of how many items are
 *  already queued: an idle, unpaused send always goes direct (it starts a turn
 *  whose completion auto-drains the queue), so a non-empty queue can never
 *  stall the composer. Parking happens only while a turn is in flight (preserve
 *  order) or the pipeline is paused after a /stop or error. `/stop` always
 *  bypasses, even busy/paused. */
export function decideComposerAction(opts: {
  hasContent: boolean;
  isStop: boolean;
  busy: boolean;
  paused: boolean;
}): ComposerAction {
  if (!opts.hasContent) return 'noop';
  if (opts.isStop) return 'stop';
  return !opts.busy && !opts.paused ? 'direct' : 'park';
}

export function ChatPage() {
  const { sessionId } = useParams<{ sessionId?: string }>();
  const navigate = useNavigate();
  const client = useAdminClient();
  const { baseUrl } = useAuth();
  const queueStore = useQueueStore();
  const folderStore = useFolderStore();
  // Reactive interjection queue for the active session (drives the panel, the
  // park-vs-direct decision, and the Send-button affordance).
  const queue = useSessionQueue(sessionId);

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [sessionsLoading, setSessionsLoading] = useState(true);
  const [creating, setCreating] = useState(false);
  const [slashCommands, setSlashCommands] = useState<{ command: string; description: string }[]>([]);
  // Switchable models for the header picker + the name of the global
  // `default-llm`, both from `GET /v1/llm/models`. Fetched once on
  // mount; the picker only renders when more than one model exists.
  const [models, setModels] = useState<ModelOption[]>([]);
  const [defaultModelName, setDefaultModelName] = useState('');

  // Channel token + bootstrap state. The token is minted once per tab
  // lifetime; the WS reuses it across every session the user switches
  // through. The anchor session is the one whose POST .../{id}/token
  // call produced our token. If the server rejects our token later
  // (e.g. after a gateway restart wipes the in-memory token table),
  // ChatWs fires onTokenRejected and we mint a fresh one for the
  // same anchor — see handleTokenRejected below.
  const [channelToken, setChannelToken] = useState<string | null>(null);
  // Files picked in the composer, uploaded to the blob store on select and
  // attached to the next outgoing message once their upload lands.
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const anchorSessionIdRef = useRef<string | null>(null);

  // Per-session view buckets keyed by session_id. `currentView` is
  // the derived projection of the URL's sessionId.
  const [views, setViews] = useState<Record<string, SessionView>>({});
  const currentView = (sessionId && views[sessionId]) || EMPTY_VIEW;
  // A turn is "in flight" either optimistically (between send and the first
  // response) or per the server's authoritative TurnState. While busy the
  // composer's send button becomes a stop button and new sends are blocked.
  const busy = currentView.awaitingReply || (currentView.turn?.active ?? false);
  // Mirrors `useParams().sessionId` in a ref so the WS onFrame closure
  // (captured once at WS construction) can answer "is this frame for
  // the session the user is currently viewing?" without rebuilding the
  // WS on every nav. Kept in lockstep with the URL via the effect that
  // clears the current row's unread badge below.
  const currentSessionIdRef = useRef<string | undefined>(sessionId);
  // Wall-clock ms of the most recent "WS reached connected state". Bumped
  // each time the status flips to 'connected' (initial connect AND
  // every reconnect). The `pending_approvals_snapshot` reconciliation
  // consults this to decide whether a local card pre-dates the
  // reconnect (eligible for drop if missing from the snapshot) or
  // arrived after (must be kept — the server's queue read may have
  // missed it in the race window between subscribe registration and
  // snapshot send).
  const lastConnectedAtRef = useRef<number>(0);
  // Bumped when the server tells us our live stream is stale (Frame::
  // Reset — typically because the catch-up gap exceeded the gateway's
  // WS replay cap). The history-load effect picks the new value up
  // through its dep array and re-fetches the active session via REST;
  // background sessions get a cleared `historyLoaded` flag so they
  // refetch lazily on next visit.
  const [historyEpoch, setHistoryEpoch] = useState(0);

  // Bumped whenever the WS reports activity for a session_id we don't
  // track in the main `sessions` list — cron creates fresh sessions
  // server-side that the chat-list endpoint filters out, so the only
  // hint a tab has that a cron fire just happened is a
  // SessionActivity ping for an unknown id. Cascaded into the
  // CronInbox panel so it can refetch right when something new lands
  // instead of waiting on the next 30s poll.
  const [cronInboxRefresh, setCronInboxRefresh] = useState(0);

  const [status, setStatus] = useState<ConnectionStatus>({ state: 'connecting' });
  // Mirrors `status` in a ref so the captured-once `onFrame` closure and the
  // session-agnostic `sendToSession` read the live connection state without a
  // stale 'connecting' after a reconnect. Updated in the same `onStatus`
  // callback that calls `setStatus`.
  const statusRef = useRef<ConnectionStatus>({ state: 'connecting' });
  const [composer, setComposer] = useState('');
  const [showSlashHints, setShowSlashHints] = useState(false);

  const transcriptScrollRef = useRef<HTMLDivElement | null>(null);
  const transcriptEndRef = useRef<HTMLDivElement | null>(null);
  const composerRef = useRef<HTMLTextAreaElement | null>(null);
  // True when the user is parked within 64px of the latest message. When
  // false a new delta/message must *not* drag them back down — the user
  // is reading scroll-back. Re-asserts itself the moment they scroll back
  // to the bottom edge. Kept in a ref so the auto-scroll effect can
  // consult it without re-firing on scroll alone.
  const pinnedToBottomRef = useRef(true);
  const [hasNewBelow, setHasNewBelow] = useState(false);
  const wsRef = useRef<ChatWs | null>(null);
  // react-router v7's `useNavigate` (non-data routes) returns a fresh
  // function whenever the location pathname changes — its useCallback
  // depends on `locationPathname`. Capturing `navigate` directly in any
  // effect's dep array would re-run that effect on every URL change,
  // which for the WS effect means tearing down the live socket on every
  // session switch (the server revokes the channel-token on close, so
  // the next reconnect hits a dead-token 401 → reconnect loop). The ref
  // gives long-lived closures a stable handle to "whatever navigate is
  // right now".
  const navigateRef = useRef(navigate);
  useEffect(() => {
    navigateRef.current = navigate;
  });
  // Holds the latest token-rejected handler so the ChatWs callback
  // (captured at construction) always calls the current closure.
  const onTokenRejectedRef = useRef<((reason: string) => void) | null>(null);
  // Generation counter so a retry chain started by an older
  // rejection stops if a newer rejection (or unmount) supersedes it.
  const tokenRemintGenRef = useRef(0);
  // Last-touched wall-clock per session bucket — bumped on nav and on
  // every inbound view-mutating frame. Drives the LRU eviction effect
  // below. Lives in a ref so the WS onFrame closure can read/write it
  // without forcing the effect that constructs the WS to re-run.
  const recencyRef = useRef<Map<string, number>>(new Map());

  // Sessions the user just `/stop`'d locally. While a session is in here, a
  // late `answer_delta` (one the server had already put on the wire before it
  // saw the stop) is dropped instead of spilling a fresh bubble below the
  // now-collapsed work block. Cleared when a new turn starts (`turn_state`
  // active) or the user sends a non-stop message.
  const stoppedSessionsRef = useRef<Set<string>>(new Set());

  // Sessions whose agent is currently streaming its FINAL reply (an
  // `answer_delta` is in flight with no tool/reasoning frame since). Firing a
  // queued item here can't interject — there are no more tool boundaries — so
  // the row's send button arms the item to fire on turn completion (a persisted
  // "waiting" state) instead of sending immediately. Set on `answer_delta`,
  // cleared at turn start/end, on any non-answer progress frame, and on /stop.
  const streamingAnswerRef = useRef<Set<string>>(new Set());

  // ── Interjection queue: refs for the captured-once onFrame closure ──
  // Imperative queue handle (ref-backed live reads + mutators). The reactive
  // composer/panel read the queue through `useSessionQueue` instead.
  const queueStoreRef = useRef(queueStore);
  queueStoreRef.current = queueStore;
  // Auto-fire turn-dedup. A queued item fires at most once per turn. The token
  // is bumped only on a LIVE turn start (a user send into the session, or a
  // live `turn_state{active:true}`); a session with no token entry has had no
  // live turn this page-load, so reload catch-up replays — which arrive before
  // the turn_state snapshot — never spuriously drain the queue.
  const turnTokenRef = useRef<Map<string, number>>(new Map());
  const firedForTurnRef = useRef<Map<string, number>>(new Map());
  // Latest queue-frame handler (auto-fire + pause detection), kept current so
  // the WS onFrame closure can call it without rebuilding the socket.
  const queueFrameRef = useRef<((frame: Frame) => void) | null>(null);

  // Streaming pacer: decouples the visual reveal cadence from the wire
  // cadence. Servers tend to flush Delta frames in uneven bursts (a few
  // chars at a time during steady-state, then a 200-char chunk after a
  // network hiccup); writing each burst straight into the bubble looks
  // jittery. The pacer instead accumulates incoming text into a per-
  // session `target` and a rAF loop catches `rendered` up at a smooth,
  // adaptive rate — slow when the backlog is small (keeps a typewriter
  // feel), fast when the backlog grows (so we never visibly lag far
  // behind the server). State lives in a ref because the rAF callback
  // reads/writes between commits and we don't want to thrash React.
  const streamPacersRef = useRef<
    Record<string, { target: string; rendered: number; rafId: number | null }>
  >({});

  const cancelPacer = useCallback((sid: string) => {
    const pacer = streamPacersRef.current[sid];
    if (!pacer) return;
    if (pacer.rafId !== null) cancelAnimationFrame(pacer.rafId);
    delete streamPacersRef.current[sid];
  }, []);

  // Reveal the pacer's fully-buffered answer text at once and stop the
  // rAF loop. A progress frame (reasoning / tool / status) is interrupting
  // the answer stream, so flush the buffer into the standalone streaming
  // bubble via `writeStreamingAnswer`, left `streaming: true` so
  // `routeInboundFrame`'s `ensureWork` can fold it into the work block as an
  // intermediate `prose` step. The final `Message` path (which finalizes the
  // answer) goes through `cancelPacer` instead. No-op when no answer is
  // mid-stream.
  const flushPacerKeepStreaming = useCallback((sid: string) => {
    const pacer = streamPacersRef.current[sid];
    if (!pacer) return;
    if (pacer.rafId !== null) cancelAnimationFrame(pacer.rafId);
    const full = pacer.target;
    delete streamPacersRef.current[sid];
    setViews((prev) => {
      const view = prev[sid];
      if (!view) return prev;
      const nextTranscript = writeStreamingAnswer(view.transcript, full);
      if (nextTranscript === view.transcript) return prev;
      return { ...prev, [sid]: { ...view, transcript: nextTranscript } };
    });
  }, []);

  const pacerTick = useCallback((sid: string) => {
    const pacer = streamPacersRef.current[sid];
    if (!pacer) return;
    const backlog = pacer.target.length - pacer.rendered;
    if (backlog <= 0) {
      pacer.rafId = null;
      return;
    }
    // Adaptive reveal: a small backlog reveals slowly so the eye reads
    // a steady typewriter trickle; a big backlog (after a burst) is
    // drained proportionally so we close the gap within a handful of
    // frames instead of running visibly behind the server for seconds.
    const step =
      backlog > 400 ? Math.ceil(backlog / 6)
      : backlog > 120 ? 8
      : backlog > 40 ? 4
      : 2;
    pacer.rendered = Math.min(pacer.target.length, pacer.rendered + step);
    const visible = pacer.target.slice(0, pacer.rendered);
    setViews((prev) => {
      const view = prev[sid] ?? EMPTY_VIEW;
      const nextTranscript = writeStreamingAnswer(view.transcript, visible);
      if (nextTranscript === view.transcript && !view.awaitingReply) return prev;
      return {
        ...prev,
        [sid]: { ...view, transcript: nextTranscript, awaitingReply: false },
      };
    });
    pacer.rafId = requestAnimationFrame(() => pacerTick(sid));
  }, []);

  const enqueueDelta = useCallback(
    (sid: string, text: string) => {
      if (!text) return;
      let pacer = streamPacersRef.current[sid];
      if (!pacer) {
        pacer = { target: '', rendered: 0, rafId: null };
        streamPacersRef.current[sid] = pacer;
      }
      pacer.target += text;
      if (pacer.rafId === null) {
        pacer.rafId = requestAnimationFrame(() => pacerTick(sid));
      }
    },
    [pacerTick],
  );

  // Tear down everything we hold for a single session: WS subscription,
  // view bucket, recency entry. Used by local hide, cross-tab hide, and
  // LRU eviction. Stable identity via useCallback([]) so the WS effect's
  // dep array stays clean.
  const releaseSessionView = useCallback((sid: string) => {
    wsRef.current?.unsubscribe(sid);
    cancelPacer(sid);
    setViews((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    recencyRef.current.delete(sid);
  }, [cancelPacer]);

  // ── Bootstrap: load session list + slash manifest, mint a token ─────
  // Runs once on mount. Anchor selection priority:
  //   1. URL's `sessionId` if it names an existing http session — opening
  //      a tab to `/chat/<id>` should land on *that* session, not silently
  //      hop to "newest".
  //   2. Otherwise the most-recent existing http session (`existing[0]`).
  //   3. Otherwise mint a fresh one.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const [
        { data: list, error: listError },
        { data: manifest, error: manifestError },
        { data: modelList, error: modelError },
        { data: folderList, error: folderError },
      ] = await Promise.all([
        client.GET('/v1/chat/sessions'),
        client.GET('/v1/chat/slash-manifest'),
        client.GET('/v1/llm/models'),
        client.GET('/v1/chat/folders'),
      ]);
      if (cancelled) return;
      if (listError) {
        console.warn('chat bootstrap: list sessions failed', listError);
      }
      if (manifestError) {
        console.warn('chat bootstrap: slash-manifest failed', manifestError);
      }
      if (modelError) {
        console.warn('chat bootstrap: list models failed', modelError);
      }
      if (folderError) {
        console.warn('chat bootstrap: list folders failed', folderError);
      }
      folderStore.replaceFolders(
        (folderList?.items ?? []).map((f) => ({
          id: f.id,
          parent_id: f.parent_id ?? undefined,
          name: f.name,
          position: f.position,
          created_at: f.created_at,
        })),
      );
      setModels(
        (modelList?.items ?? []).map((m) => ({
          name: m.name,
          provider: m.provider,
          model: m.model,
          isDefault: m.is_default,
        })),
      );
      setDefaultModelName(modelList?.default_name ?? '');
      const existing: SessionSummary[] = (list?.items ?? []).map((s) => ({
        session_id: s.session_id,
        created_at: s.created_at,
        last_active: s.last_active,
        unread: 0,
        pinned: s.pinned,
        last_user_text: s.last_user_text ?? undefined,
        folder_id: s.folder_id ?? undefined,
      }));
      setSessions(existing);
      setSlashCommands(manifest?.items ?? []);
      setSessionsLoading(false);

      // Prefer the URL's session if it exists in the list — keeps
      // bookmark / copy-link semantics intact and avoids the
      // "every tab mints against existing[0]" thrash that revokes
      // sibling tabs' tokens.
      const preferred =
        sessionId && existing.some((s) => s.session_id === sessionId)
          ? sessionId
          : existing[0]?.session_id;

      let anchorId: string;
      let token: string;
      if (preferred) {
        const { data: refreshed, error } = await client.POST(
          '/v1/chat/sessions/{session_id}/token',
          { params: { path: { session_id: preferred } } },
        );
        if (cancelled) return;
        if (error || !refreshed?.channel_token) {
          // Anchor session vanished between list and refresh — fall
          // through to creating a new one.
          console.warn(
            'chat bootstrap: refresh token failed for',
            preferred,
            error,
          );
          const result = await createAnchorSession();
          if (!result) return;
          anchorId = result.sessionId;
          token = result.token;
        } else {
          anchorId = preferred;
          token = refreshed.channel_token;
        }
      } else {
        const result = await createAnchorSession();
        if (!result) return;
        anchorId = result.sessionId;
        token = result.token;
      }
      if (cancelled) return;
      anchorSessionIdRef.current = anchorId;
      setChannelToken(token);
      // Land on a session: if the URL has none, redirect to the
      // anchor; if it points at an unknown id, also redirect.
      if (!sessionId || sessionId !== anchorId) {
        navigateRef.current(`/chat/${anchorId}`, { replace: true });
      }

      async function createAnchorSession(): Promise<{ sessionId: string; token: string } | null> {
        const { data, error } = await client.POST('/v1/chat/sessions', {});
        if (cancelled) return null;
        if (error || !data?.channel_token) {
          console.warn('chat bootstrap: create session failed', error);
          return null;
        }
        setSessions((prev) =>
          prev.some((s) => s.session_id === data.session_id)
            ? prev
            : [
                {
                  session_id: data.session_id,
                  created_at: new Date().toISOString(),
                  last_active: new Date().toISOString(),
                  unread: 0,
                  pinned: false,
                },
                ...prev,
              ],
        );
        return { sessionId: data.session_id, token: data.channel_token };
      }
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, folderStore]); // intentionally NOT depending on sessionId — bootstrap is one-shot

  // Always-current handler for register_ack { ok: false }. Mints a
  // fresh token against the anchor session and feeds it back into
  // ChatWs via replaceToken. Backs off on POST failure, abandons the
  // chain if a newer rejection (or unmount) bumps the generation.
  useEffect(() => {
    onTokenRejectedRef.current = (_reason: string) => {
      const anchor = anchorSessionIdRef.current;
      if (!anchor) return;
      const myGen = ++tokenRemintGenRef.current;
      let attempt = 0;
      const tryMint = async (): Promise<void> => {
        if (tokenRemintGenRef.current !== myGen) return;
        const { data, error } = await client.POST(
          '/v1/chat/sessions/{session_id}/token',
          { params: { path: { session_id: anchor } } },
        );
        if (tokenRemintGenRef.current !== myGen) return;
        if (!error && data?.channel_token) {
          wsRef.current?.replaceToken(data.channel_token);
          return;
        }
        attempt += 1;
        const delay = Math.min(2_000 * 2 ** Math.min(attempt - 1, 4), 30_000);
        setTimeout(() => {
          void tryMint();
        }, delay);
      };
      void tryMint();
    };
  }, [client]);

  // ── WS lifecycle: tied to channelToken, not to sessionId ────────────
  // Opens once we have a token, lives until the component unmounts
  // (i.e. the user navigates away from /chat). Reconnect is internal
  // to ChatWs.
  useEffect(() => {
    if (!channelToken) return;
    const ws = new ChatWs({
      baseUrl,
      channelToken,
      initialSessionIds: [],
      onStatus: (s) => {
        statusRef.current = s;
        setStatus(s);
      },
      onFrame: (frame) => {
        if (frame.kind === 'folders_changed') {
          // Full-snapshot convergence — replace the local folder tree
          // wholesale (folders are few, no patch-merge needed).
          folderStore.replaceFolders(
            frame.folders.map((f) => ({
              id: f.id,
              parent_id: f.parent_id ?? undefined,
              name: f.name,
              position: f.position,
              created_at: f.created_at,
            })),
          );
          return;
        }
        if (frame.kind === 'session_updated') {
          setSessions((prev) => applySessionPatch(prev, frame.session_id, frame.patch));
          if (frame.patch.hidden === true) {
            // Sibling tab hid the session (or this tab via the local
            // DELETE path — both converge here). Drop the WS
            // subscription so Delta/Message frames stop streaming
            // into a soon-to-be-freed bucket, then free the bucket
            // and its recency entry. Idempotent with the local
            // handleHideSession cleanup.
            releaseSessionView(frame.session_id);
            if (currentSessionIdRef.current === frame.session_id) {
              navigateRef.current('/chat', { replace: true });
            }
          }
          return;
        }
        if (frame.kind === 'session_activity') {
          // Cheap unread / freshness signal for every http connection
          // regardless of subscription — the whole reason this frame
          // exists. Foreground sessions don't bump (the user can see
          // it), background sessions get a +1 badge.
          const isForeground = currentSessionIdRef.current === frame.session_id;
          let known = true;
          setSessions((prev) => {
            known = prev.some((s) => s.session_id === frame.session_id);
            return applySessionActivity(prev, frame.session_id, frame.at, isForeground);
          });
          if (!known) {
            // Probably a cron-spawned session — those are filtered out
            // of the main chat list server-side, so the only signal
            // the tab has is activity for an id it doesn't know.
            // Nudging the CronInbox panel triggers an immediate
            // refetch so the fire shows up without waiting on the
            // polling interval.
            setCronInboxRefresh((n) => n + 1);
          }
          return;
        }
        // Catch-up replays carry an explicit ordinal — advance the WS
        // cursor so a future reconnect doesn't ask for these rows
        // again. Live frames have ordinal === undefined and don't
        // shift the cursor.
        if (frame.kind === 'message' && frame.ordinal !== undefined) {
          wsRef.current?.recordOrdinal(frame.session_id, frame.ordinal);
        }
        // Protect actively-streamed buckets from LRU eviction. Only
        // frames that actually mutate `views` bump recency — sidebar-
        // only signals (session_updated) shouldn't bias retention of
        // a transcript the user isn't engaging with.
        switch (frame.kind) {
          case 'answer_delta':
          case 'reasoning':
          case 'tool_started':
          case 'tool_completed':
          case 'status':
          case 'message':
          case 'attachment':
          case 'notice':
          case 'approval_requested':
          case 'task_list':
          case 'turn_state':
            recencyRef.current.set(frame.session_id, Date.now());
            break;
          default:
            break;
        }
        // A fresh `turn_state{active}` means a new turn — the prior `/stop`
        // (if any) is over, so stop dropping this session's deltas.
        if (frame.kind === 'turn_state' && frame.active) {
          stoppedSessionsRef.current.delete(frame.session_id);
          // New turn — not in the final-reply phase yet.
          streamingAnswerRef.current.delete(frame.session_id);
          // A live turn started — arm auto-fire for this session's next
          // completion (re-arm drops any prior fired mark).
          turnTokenRef.current.set(
            frame.session_id,
            (turnTokenRef.current.get(frame.session_id) ?? 0) + 1,
          );
          firedForTurnRef.current.delete(frame.session_id);
        }
        if (frame.kind === 'turn_state' && !frame.active) {
          streamingAnswerRef.current.delete(frame.session_id);
        }
        // Route delta frames through the pacer instead of straight to
        // setViews — the pacer's rAF loop owns the bubble's text while
        // streaming is in flight. routeInboundFrame's delta case stays
        // as a defensive fallback but should not fire from this path.
        if (frame.kind === 'answer_delta') {
          // Drop a delta that raced in after a local `/stop` — the partial
          // answer is already settled inside the collapsed work block; a new
          // bubble here is the stray "message after /stop".
          if (stoppedSessionsRef.current.has(frame.session_id)) return;
          // Final answer is streaming — entering the final-reply phase.
          streamingAnswerRef.current.add(frame.session_id);
          enqueueDelta(frame.session_id, frame.text);
          return;
        }
        // Progress frames (reasoning / tool lifecycle / status) and a
        // terminal `notice` (e.g. the `/stop` confirmation) interrupt the
        // answer stream. Settle the paced answer bubble FIRST — its buffered
        // text is mid-turn prose, so it folds into the work block ahead of
        // this frame. Crucial for the notice: otherwise the pacer's rAF keeps
        // ticking and spills the buffered answer as a bubble *after* the
        // notice (the observed "reply after the stop notice" on a reloaded tab).
        if (
          frame.kind === 'reasoning' ||
          frame.kind === 'tool_started' ||
          frame.kind === 'tool_completed' ||
          frame.kind === 'status' ||
          frame.kind === 'notice'
        ) {
          flushPacerKeepStreaming(frame.session_id);
        }
        // Non-answer progress (the agent went back to tool/reasoning work, or a
        // notice interrupted) means we're no longer in the final-reply phase.
        if (
          frame.kind === 'reasoning' ||
          frame.kind === 'tool_started' ||
          frame.kind === 'tool_completed' ||
          frame.kind === 'status'
        ) {
          streamingAnswerRef.current.delete(frame.session_id);
        }
        // A broadcast `/stop` cancellation notice stops THIS tab's stream too
        // (the observer never ran the local `/stop`): settle the buffer above,
        // then drop any delta that races in afterwards so it can't spill a
        // bubble below the now-closed work block.
        if (frame.kind === 'notice' && !frame.transient && isStopCancellationNotice(frame.text)) {
          stoppedSessionsRef.current.add(frame.session_id);
          streamingAnswerRef.current.delete(frame.session_id);
        }
        // An assistant message frame is the authoritative final text
        // for the stream — drop any pacer state so its in-flight rAF
        // doesn't overwrite the finalized bubble a tick later.
        if (frame.kind === 'message' && frame.role !== 'user') {
          cancelPacer(frame.session_id);
          // Turn's answer is settled — leave the final-reply phase.
          streamingAnswerRef.current.delete(frame.session_id);
        }
        routeInboundFrame(
          frame,
          setViews,
          (sid, preview) => setSessions((prev) => applySessionUserText(prev, sid, preview)),
          lastConnectedAtRef.current,
        );
        // Interjection queue: auto-fire the next parked item on a live normal
        // completion, and pause the pipeline on a /stop-cancel or error notice.
        queueFrameRef.current?.(frame);
      },
      onTokenRejected: (reason) => onTokenRejectedRef.current?.(reason),
      onReset: (reason) => {
        // Stream is stale (per Frame::Reset contract — see chatWs.ts
        // onReset docs). The chatWs has already cleared its cursors;
        // we clear each view's history flags so the load effect re-
        // fetches via REST. Transcript / oldestOrdinal are wiped too:
        // some rows in the slow-consumer gap may already be in the
        // current transcript (echoed live before the disconnect) and
        // we can't tell which, so the safe thing is to refill from
        // the authoritative REST source. `pendingApproval` is left
        // alone — it's a separate transport-independent state that
        // the server's ApprovalResolved fan-out will clear when /
        // if the call resolves.
        console.warn('chat WS reset; refetching history via REST', reason);
        // The REST refill will rebuild the streaming bubble's final
        // text — any pacer state we hold is stale relative to that
        // and must not flush after the wipe.
        for (const sid of Object.keys(streamPacersRef.current)) {
          cancelPacer(sid);
        }
        setViews((prev) => {
          const next: Record<string, SessionView> = {};
          for (const [sid, view] of Object.entries(prev)) {
            next[sid] = { ...EMPTY_VIEW, pendingApproval: view.pendingApproval };
          }
          return next;
        });
        setHistoryEpoch((e) => e + 1);
      },
    });
    wsRef.current = ws;
    return () => {
      // Bump the generation so any in-flight retry chain stops.
      tokenRemintGenRef.current += 1;
      ws.close();
      wsRef.current = null;
    };
  }, [
    baseUrl,
    channelToken,
    releaseSessionView,
    enqueueDelta,
    cancelPacer,
    flushPacerKeepStreaming,
    folderStore,
  ]);

  // ── Active session: subscribe + lazy-load history ───────────────────
  // Subscribe stays sticky once added: when the user switches away,
  // we keep the subscription so background sessions still accumulate
  // Delta/Message frames into their view bucket. The LRU eviction
  // effect above caps the per-tab bucket count at `VIEW_CACHE_LIMIT`
  // and drops the WS subscription alongside the freed transcript.
  useEffect(() => {
    if (!sessionId || !wsRef.current) return;
    wsRef.current.subscribe(sessionId);
    const existing = views[sessionId];
    if (existing && existing.historyLoaded) return;
    if (existing && existing.historyLoading) return;
    let cancelled = false;
    setViews((prev) => mergeView(prev, sessionId, { historyLoading: true }));
    void (async () => {
      const failNotice = (reason: string): TranscriptRow => ({
        key: `hist-err-${sessionId}-${Date.now()}`,
        role: 'system',
        text: '',
        notice: {
          level: 'warn',
          text: `Couldn't load conversation history: ${reason}. New messages will still arrive live.`,
        },
      });
      try {
        const { data, error } = await client.GET('/v1/chat/sessions/{session_id}', {
          params: { path: { session_id: sessionId } },
        });
        if (cancelled) return;
        if (error) {
          console.warn('chat history load failed', sessionId, error);
          setViews((prev) =>
            mergeView(prev, sessionId, {
              transcript: [failNotice(formatHttpError(error))],
              historyLoaded: true,
              historyLoading: false,
            }),
          );
          return;
        }
        if (data) {
          const rows = data.transcript.map(historyRowToTranscript.bind(null, sessionId));
          // Use the server's real message-ordinal bounds, NOT the transcript
          // items: control-event items carry synthetic negative ordinals, so
          // inferring from `transcript[0]` / `transcript[last]` would seed a
          // bogus cursor (a trailing `/stop` / `/compact` notice is the common
          // case) — forcing a full replay and duplicating rows.
          const oldestOrdinal = data.oldest_ordinal ?? null;
          // Seed the WS cursor so a reconnect after a network dip asks
          // the server for anything newer rather than dropping it on
          // the floor. The `-1` sentinel handles a brand-new session
          // whose transcript is empty: without it the cursor would
          // stay `undefined`, the next Subscribe would omit
          // `since_ordinal`, and the server would skip replay
          // entirely — any messages persisted during the disconnect
          // would be lost. `recordOrdinal` ignores backwards moves so
          // the non-empty branch's higher ordinal still wins.
          wsRef.current?.recordOrdinal(sessionId, -1);
          if (data.newest_ordinal != null) {
            wsRef.current?.recordOrdinal(sessionId, data.newest_ordinal);
          }
          setViews((prev) =>
            // The reload replaces the transcript wholesale. Do NOT fold the
            // cached `turn` back in here: the freshly-fetched REST history
            // is the server's authoritative state (a finished/cancelled
            // turn comes back collapsed), and the WS (re)subscribe that
            // accompanies a reload always delivers a fresh `TurnState`
            // snapshot which re-opens a genuinely in-flight turn's block
            // (matched by start). Folding a possibly-stale cached `turn`
            // over the reload could resurrect a finished turn as a phantom
            // "Working" box, so the authoritative snapshot drives it.
            mergeView(prev, sessionId, {
              transcript: rows,
              historyLoaded: true,
              historyLoading: false,
              oldestOrdinal,
              hasMore: data.has_more,
              model: data.last_llm ?? null,
            }),
          );
        } else {
          setViews((prev) =>
            mergeView(prev, sessionId, { historyLoaded: true, historyLoading: false }),
          );
        }
      } catch (e) {
        if (!cancelled) {
          console.warn('chat history load threw', sessionId, e);
          setViews((prev) =>
            mergeView(prev, sessionId, {
              transcript: [failNotice(String(e))],
              historyLoaded: true,
              historyLoading: false,
            }),
          );
        }
      }
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, channelToken, historyEpoch]); // views intentionally excluded — we react to nav, WS readiness, and Reset-driven refetch via the epoch

  // Auto-scroll on transcript append — but only if the user is already
  // parked at the bottom. Otherwise raise the "new messages" pill so
  // they can opt back in. useLayoutEffect runs before paint so we read
  // fresh scrollHeight.
  //
  // `instant` (the default behavior) not `smooth` — when first landing
  // on a session, the history fetch resolves and React commits the
  // populated transcript, and we want the first paint to already be at
  // the bottom rather than scrolling past every message on the way
  // down. Same for streaming deltas: each delta would otherwise queue
  // another smooth animation, which compounds into visible jitter as
  // the bubble grows. The user-initiated `jumpToLatest` (below) keeps
  // smooth — that one IS a discrete "take me there" gesture.
  useLayoutEffect(() => {
    const scroller = transcriptScrollRef.current;
    if (!scroller) return;
    if (pinnedToBottomRef.current) {
      scroller.scrollTop = scroller.scrollHeight;
      setHasNewBelow(false);
    } else {
      setHasNewBelow(true);
    }
  }, [
    currentView.transcript,
    currentView.pendingApproval,
    currentView.awaitingReply,
    // A newly deferred bubble grows the scroller without touching the
    // transcript — re-pin to the bottom when the user is already there.
    queue.deferred.length,
  ]);

  // Reset pin state when switching sessions — a fresh view should jump
  // to its tail, not inherit the previous view's scroll posture. Also
  // clear the just-entered session's unread badge and update the ref
  // that routeInboundFrame consults to decide whether an inbound
  // frame's session is in the foreground.
  useEffect(() => {
    pinnedToBottomRef.current = true;
    setHasNewBelow(false);
    currentSessionIdRef.current = sessionId;
    if (sessionId) {
      recencyRef.current.set(sessionId, Date.now());
      setSessions((prev) => {
        const idx = prev.findIndex((s) => s.session_id === sessionId);
        if (idx === -1 || prev[idx].unread === 0) return prev;
        const next = prev.slice();
        next[idx] = { ...prev[idx], unread: 0 };
        return next;
      });
    }
  }, [sessionId]);

  // LRU eviction. Active session is protected — yanking its bucket
  // mid-render would flash an empty transcript. Eviction drops the
  // WS subscription too; revisit re-subscribes via the
  // foreground-session effect and re-fetches transcript via REST.
  useEffect(() => {
    const keys = Object.keys(views);
    if (keys.length <= VIEW_CACHE_LIMIT) return;
    const activeSid = currentSessionIdRef.current;
    const candidates = keys
      .filter((sid) => sid !== activeSid)
      .sort(
        (a, b) =>
          (recencyRef.current.get(a) ?? 0) - (recencyRef.current.get(b) ?? 0),
      );
    const toEvict = candidates.slice(0, keys.length - VIEW_CACHE_LIMIT);
    if (toEvict.length === 0) return;
    for (const sid of toEvict) releaseSessionView(sid);
  }, [views, releaseSessionView]);

  // Bump the reconnect cutoff each time we land on 'connected'. The
  // snapshot reconciliation uses this as the "anything older than this
  // is fair game to drop if the server says it's gone" boundary. Initial
  // connect also bumps; no local cards exist that early so it's a no-op
  // in practice.
  useEffect(() => {
    if (status.state === 'connected') {
      lastConnectedAtRef.current = Date.now();
    }
  }, [status.state]);

  // Scroll-up pagination: when the user is within `topThresholdPx`
  // of the top *and* the current view still has older rows on the
  // server, fetch one more slice and prepend it. Scroll position is
  // pinned to the same logical row across the prepend by recording
  // `scrollHeight - scrollTop` before the state update and restoring
  // it after — otherwise the new top of the list would yank the
  // viewport out from under the user.
  const loadOlder = useCallback(async () => {
    if (!sessionId) return;
    const view = views[sessionId];
    if (!view || !view.hasMore || view.olderLoading || view.oldestOrdinal === null) return;
    const scroller = transcriptScrollRef.current;
    const anchorFromBottom = scroller
      ? scroller.scrollHeight - scroller.scrollTop
      : null;
    setViews((prev) => mergeView(prev, sessionId, { olderLoading: true }));
    try {
      const { data, error } = await client.GET('/v1/chat/sessions/{session_id}', {
        params: {
          path: { session_id: sessionId },
          query: { before_ordinal: view.oldestOrdinal },
        },
      });
      if (error || !data) {
        console.warn('chat history older-page load failed', sessionId, error);
        setViews((prev) => mergeView(prev, sessionId, { olderLoading: false }));
        return;
      }
      const newRows = data.transcript.map(historyRowToTranscript.bind(null, sessionId));
      // Real message-ordinal bound from the server (not the transcript items,
      // which may include synthetic-ordinal control events).
      const newOldest = data.oldest_ordinal ?? view.oldestOrdinal;
      setViews((prev) => {
        const cur = prev[sessionId] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sessionId]: {
            ...cur,
            transcript: [...newRows, ...cur.transcript],
            oldestOrdinal: newOldest,
            hasMore: data.has_more,
            olderLoading: false,
          },
        };
      });
      // Restore scroll position so the previously-visible top row stays
      // visible. Run after the next paint so the prepended rows have
      // their measured height.
      if (scroller && anchorFromBottom !== null) {
        requestAnimationFrame(() => {
          scroller.scrollTop = scroller.scrollHeight - anchorFromBottom;
        });
      }
    } catch (e) {
      console.warn('chat history older-page load threw', sessionId, e);
      setViews((prev) => mergeView(prev, sessionId, { olderLoading: false }));
    }
  }, [client, sessionId, views]);

  const handleTranscriptScroll = useCallback(() => {
    const scroller = transcriptScrollRef.current;
    if (!scroller) return;
    const slackPx = 64;
    const atBottom =
      scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight <= slackPx;
    pinnedToBottomRef.current = atBottom;
    if (atBottom) setHasNewBelow(false);
    // Trigger older-page fetch when the user is within 200px of the
    // top. The `loadOlder` callback no-ops if a request is already
    // in flight or `hasMore === false`, so emitting this on every
    // scroll event is safe.
    if (scroller.scrollTop <= 200) {
      void loadOlder();
    }
  }, [loadOlder]);

  const jumpToLatest = useCallback(() => {
    transcriptEndRef.current?.scrollIntoView({ behavior: 'smooth' });
    pinnedToBottomRef.current = true;
    setHasNewBelow(false);
  }, []);

  // Auto-grow the composer up to a cap. Keeps single-line idle state
  // compact while still allowing multi-paragraph drafting.
  useLayoutEffect(() => {
    const ta = composerRef.current;
    if (!ta) return;
    ta.style.height = 'auto';
    const max = 200;
    ta.style.height = `${Math.min(ta.scrollHeight, max)}px`;
  }, [composer]);

  // Session-agnostic send used by the composer (active session), the queue
  // auto-fire pipeline, manual per-item fire, and resume — so a message can be
  // fired into ANY tracked session, not just the one on screen. Returns true
  // iff the WS accepted it (connected); false => the caller leaves the item
  // queued. Unlike `/stop`, this never closes the live work block, so a
  // mid-turn interjection keeps the in-flight block open.
  const sendToSession = useCallback(
    (
      targetSessionId: string,
      raw: string,
      wireAttachments: WireAttachment[] = [],
      opts?: { foreground?: boolean },
    ): boolean => {
      const trimmed = raw.trim();
      if (!trimmed && wireAttachments.length === 0) return false;
      if (!wsRef.current || statusRef.current.state !== 'connected') return false;
      // Same UUID is the WS frame's dedup key AND the optimistic row's
      // reconciliation key (the inbound echo replaces this row in place).
      const clientMsgId = crypto.randomUUID();
      stoppedSessionsRef.current.delete(targetSessionId);
      setViews((prev) => {
        const view = prev[targetSessionId] ?? EMPTY_VIEW;
        // A mid-turn interjection splits the agent's work in two. Relabel the
        // open work block to "Worked Xs" (so it isn't a live "Working…" next to
        // the NEW block the agent opens after the interjection — two "Working"
        // boxes read as two concurrent runs), but KEEP it expanded: its
        // split-off work stays visible until the turn fully ends (then
        // `closeActiveWork` collapses it). No-op when idle (no open block). The
        // working indicator below bridges until the next progress frame.
        const base = view.turn?.active ? settleActiveWork(view.transcript) : view.transcript;
        return {
          ...prev,
          [targetSessionId]: {
            ...view,
            transcript: [
              ...base,
              {
                key: `pending-${clientMsgId}`,
                role: 'user',
                text: trimmed,
                hasAttachments: wireAttachments.length > 0,
                attachments: wireAttachments.length > 0 ? wireAttachments : undefined,
                pending: true,
                clientMsgId,
                createdAt: new Date().toISOString(),
              },
            ],
            awaitingReply: true,
          },
        };
      });
      setSessions((prev) =>
        applySessionUserText(prev, targetSessionId, trimmed || '[attachment]'),
      );
      // A user send starts (or extends) a turn in this session — arm auto-fire
      // for its completion and protect its bucket from LRU eviction.
      turnTokenRef.current.set(
        targetSessionId,
        (turnTokenRef.current.get(targetSessionId) ?? 0) + 1,
      );
      firedForTurnRef.current.delete(targetSessionId);
      recencyRef.current.set(targetSessionId, Date.now());
      if (opts?.foreground ?? targetSessionId === currentSessionIdRef.current) {
        pinnedToBottomRef.current = true;
        setHasNewBelow(false);
      }
      wsRef.current.sendMessage({
        sessionId: targetSessionId,
        userId: 'web-operator',
        content: trimmed,
        clientMsgId,
        attachments: wireAttachments,
      });
      return true;
    },
    [],
  );

  // Fire several queued messages into a session as ONE batch frame — the
  // server runs them as a single coalesced turn (one reply) while keeping each
  // as its own row, so they merge deterministically instead of racing the
  // per-message intake. Appends N optimistic rows + one turn-arm, mirroring
  // sendToSession's bookkeeping. Returns false (nothing sent) if disconnected.
  const sendBatchToSession = useCallback(
    (targetSessionId: string, items: QueuedItem[]): boolean => {
      if (!wsRef.current || statusRef.current.state !== 'connected') return false;
      const prepared = items
        .map((it) => ({
          clientMsgId: crypto.randomUUID(),
          text: it.text.trim(),
          attachments: it.attachments,
        }))
        .filter((m) => m.text.length > 0 || m.attachments.length > 0);
      if (prepared.length === 0) return false;
      stoppedSessionsRef.current.delete(targetSessionId);
      setViews((prev) => {
        const view = prev[targetSessionId] ?? EMPTY_VIEW;
        // Collapse any open work block so it reads "Worked Xs" rather than a
        // live "Working…" next to the block the batch's turn opens (same as the
        // single interjection). No-op when idle / already closed.
        const base = view.turn?.active ? closeActiveWork(view.transcript) : view.transcript;
        return {
          ...prev,
          [targetSessionId]: {
            ...view,
            transcript: [
              ...base,
              ...prepared.map((m) => ({
                key: `pending-${m.clientMsgId}`,
                role: 'user' as const,
                text: m.text,
                hasAttachments: m.attachments.length > 0,
                attachments: m.attachments.length > 0 ? m.attachments : undefined,
                pending: true,
                clientMsgId: m.clientMsgId,
                createdAt: new Date().toISOString(),
              })),
            ],
            awaitingReply: true,
          },
        };
      });
      const lastText = prepared[prepared.length - 1].text;
      setSessions((prev) =>
        applySessionUserText(prev, targetSessionId, lastText || '[attachment]'),
      );
      turnTokenRef.current.set(
        targetSessionId,
        (turnTokenRef.current.get(targetSessionId) ?? 0) + 1,
      );
      firedForTurnRef.current.delete(targetSessionId);
      recencyRef.current.set(targetSessionId, Date.now());
      if (targetSessionId === currentSessionIdRef.current) {
        pinnedToBottomRef.current = true;
        setHasNewBelow(false);
      }
      wsRef.current.sendMessages(
        targetSessionId,
        prepared.map((m) => ({
          content: m.text,
          clientMsgId: m.clientMsgId,
          attachments: m.attachments,
        })),
      );
      return true;
    },
    [],
  );

  const sendText = useCallback(
    (raw: string, wireAttachments: WireAttachment[] = []) => {
      const trimmed = raw.trim();
      if ((!trimmed && wireAttachments.length === 0) || !sessionId || !wsRef.current)
        return;
      if (status.state !== 'connected') return;
      // Non-stop sends go through the shared session-agnostic path.
      if (!isStopCommand(trimmed)) {
        sendToSession(sessionId, raw, wireAttachments, { foreground: true });
        return;
      }
      // `/stop` is the one command we can reflect without the backend: the
      // user's own action means "cancel", so collapse the live work block to
      // "Cancelled" and end the turn locally right away. Don't show the
      // awaiting-reply spinner (we're stopping, not starting a turn); the
      // server's TurnState/notice frames then reconcile idempotently. Settle
      // any buffered answer first, keep the partial reply as its own bubble
      // below the collapsed block, and mark the session stopped so a delta
      // that raced in just after is dropped rather than spilling a bubble.
      const clientMsgId = crypto.randomUUID();
      flushPacerKeepStreaming(sessionId);
      stoppedSessionsRef.current.add(sessionId);
      streamingAnswerRef.current.delete(sessionId);
      // Pause the interjection queue at once. The cancelled turn salvages its
      // partial reply as an assistant `message` that races ahead of the cancel
      // notice; without this it would auto-fire a queued item before the notice
      // sets the pause. Move any deferred items back to the parked queue first
      // (synchronously, before any frame) so the salvaged-message drain finds an
      // empty deferred list and the banner covers them too. Setting the pause
      // here also surfaces the banner immediately.
      queueStoreRef.current.restoreDeferred(sessionId);
      if (queueStoreRef.current.queue(sessionId).items.length > 0) {
        queueStoreRef.current.setPause(sessionId, 'cancelled');
      }
      setViews((prev) => {
        const view = prev[sessionId] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sessionId]: {
            ...view,
            transcript: [
              ...closeActiveWork(finalizeTrailingAnswer(view.transcript), true),
              {
                key: `pending-${clientMsgId}`,
                role: 'user',
                text: trimmed,
                hasAttachments: wireAttachments.length > 0,
                attachments: wireAttachments.length > 0 ? wireAttachments : undefined,
                pending: true,
                clientMsgId,
                createdAt: new Date().toISOString(),
              },
            ],
            awaitingReply: false,
            turn: { active: false, startedAt: null },
          },
        };
      });
      setSessions((prev) => applySessionUserText(prev, sessionId, trimmed));
      pinnedToBottomRef.current = true;
      setHasNewBelow(false);
      wsRef.current.sendMessage({
        sessionId,
        userId: 'web-operator',
        content: trimmed,
        clientMsgId,
        attachments: wireAttachments,
      });
    },
    [sessionId, status.state, sendToSession, flushPacerKeepStreaming],
  );

  // Non-empty draft OR at least one ready attachment — drives the Send/Stop
  // matrix and whether a submit does anything.
  const hasContent =
    composer.trim().length > 0 || attachments.some((a) => a.status === 'ready');

  const handleSend = useCallback(
    (e: FormEvent) => {
      e.preventDefault();
      // No busy early-out: while a turn is in flight (or the queue is non-empty
      // / paused) a submit PARKS the message instead of being blocked.
      // Don't act until every picked file has finished uploading, so an
      // in-flight attachment isn't silently dropped from the message.
      if (attachments.some((a) => a.status === 'uploading')) return;
      const trimmed = composer.trim();
      const wire: WireAttachment[] = attachments
        .filter((a) => a.status === 'ready' && a.blobId)
        .map((a) => ({
          kind: attachmentKind(a.mime),
          blob_id: a.blobId as string,
          mime_type: a.mime,
          size: a.size,
          filename: a.filename,
        }));
      const action = decideComposerAction({
        hasContent: trimmed.length > 0 || wire.length > 0,
        isStop: isStopCommand(trimmed),
        busy,
        paused: queue.pauseReason !== null,
      });
      if (action === 'noop') return;
      if (action === 'stop') {
        sendText('/stop');
      } else if (action === 'direct') {
        sendText(composer, wire);
      } else {
        queue.enqueue({ id: crypto.randomUUID(), text: trimmed, attachments: wire });
      }
      setComposer('');
      attachments.forEach((a) => {
        if (a.previewUrl) URL.revokeObjectURL(a.previewUrl);
      });
      setAttachments([]);
      setShowSlashHints(false);
    },
    [composer, busy, attachments, sendText, queue],
  );

  const handleStop = useCallback(
    (e: ReactMouseEvent<HTMLButtonElement>) => {
      // Clicking stop flips `busy` false (the turn is cancelled optimistically),
      // which React flushes synchronously — mid-click — re-typing THIS button
      // from `type="button"` to the Send button's `type="submit"`. The browser
      // then runs the click's default action on the now-submit button and
      // submits the form, firing `handleSend` (whose `if (busy) return` guard
      // no longer holds) and sending the composer draft. Prevent that default
      // submit; the distinct button `key`s below stop the node reuse at the
      // source too.
      e.preventDefault();
      // The stop button issues `/stop` through the same path as typing it,
      // ignoring any draft already in the composer.
      sendText('/stop');
    },
    [sendText],
  );

  // Interjection queue: react to inbound frames. On a LIVE normal completion
  // (an assistant `message` for a turn we armed) fire the top parked item for
  // that session; on a terminal /stop-cancel or error notice pause the
  // pipeline so it stops draining until the user resumes via the banner.
  const drainQueueOnFrame = useCallback(
    (frame: Frame) => {
      const store = queueStoreRef.current;
      if (frame.kind === 'message' && frame.role !== 'user') {
        const sid = frame.session_id;
        // A just-/stop'd session salvages its partial reply as an assistant
        // message — that is NOT a normal completion and must never drain the
        // queue. Cleared when a new turn starts or a non-stop message is sent.
        if (stoppedSessionsRef.current.has(sid)) return;
        const token = turnTokenRef.current.get(sid);
        // Fire only when a live turn armed this session (token set) — skips
        // reload catch-up replays — and not already fired for this turn.
        if (token !== undefined && firedForTurnRef.current.get(sid) !== token) {
          const snap = store.queue(sid);
          if (snap.pauseReason !== null) return;
          // Deferred ("waiting in the thread") messages — the ones the operator
          // clicked send on mid-final-reply — ALL go out together as soon as the
          // reply completes, so the agent answers them as one merged turn.
          // Parked items stay one-per-completion. Sharing the parked-queue token
          // gate keeps this single-fire-per-live-completion.
          if (snap.deferred.length > 0) {
            // Drop any content-less junk (can only arise from an out-of-band
            // localStorage write — the composer/edit paths refuse blank items)
            // so it can't wedge the queue or skew the batch threshold/removal.
            const sendable = snap.deferred.filter(
              (i) => i.text.trim().length > 0 || i.attachments.length > 0,
            );
            for (const item of snap.deferred) {
              if (!sendable.includes(item)) store.removeDeferred(sid, item.id);
            }
            if (sendable.length === 0) return;
            firedForTurnRef.current.set(sid, token);
            // 2+ plain messages go as ONE batch frame so the server coalesces
            // them deterministically (no per-message intake race). A slash
            // command is a coalescing barrier — those, and the lone-item case,
            // fall back to individual sends.
            const canBatch = sendable.length >= 2 && sendable.every((i) => !isSlashText(i.text));
            if (canBatch) {
              if (sendBatchToSession(sid, sendable)) {
                for (const item of sendable) store.removeDeferred(sid, item.id);
              } else {
                firedForTurnRef.current.delete(sid);
              }
              return;
            }
            for (const item of sendable) {
              if (sendToSession(sid, item.text, item.attachments)) {
                store.removeDeferred(sid, item.id);
              } else {
                // Disconnected — stop here and leave the rest deferred to retry.
                firedForTurnRef.current.delete(sid);
                break;
              }
            }
            return;
          }
          const top = snap.items[0];
          if (!top) return;
          firedForTurnRef.current.set(sid, token);
          if (sendToSession(sid, top.text, top.attachments)) {
            store.removeItem(sid, top.id);
          } else {
            // Disconnected — leave it queued and allow a later retry.
            firedForTurnRef.current.delete(sid);
          }
        }
        return;
      }
      if (frame.kind === 'turn_state' && !frame.active) {
        const sid = frame.session_id;
        const token = turnTokenRef.current.get(sid);
        // `turn_state{active:false}` is the one turn-end signal that ALWAYS
        // fires; the assistant `message` does not (a blank/tool-only final
        // emits none, an errored/cancelled turn emits none, a reload/Reset
        // never re-delivers the prior completion). So if the message branch did
        // NOT already dispatch a deferred item this turn (firedForTurn === token
        // means it did, and the rest keep waiting for the turn that dispatch
        // started), the still-pending deferred items can't ride this completion.
        // Move them back to the parked queue — visible/editable and drained on
        // the next completion — rather than leaving them stranded as a
        // read-only thread bubble. Restoring (not sending) here also means a
        // turn that ended via /stop or error can't auto-fire a deferred item
        // ahead of the pause-setting notice.
        if (token === undefined || firedForTurnRef.current.get(sid) !== token) {
          const snap = store.queue(sid);
          if (snap.deferred.length > 0 && snap.pauseReason === null) {
            store.restoreDeferred(sid);
          }
        }
        return;
      }
      if (frame.kind === 'notice' && !frame.transient) {
        const sid = frame.session_id;
        const q = store.queue(sid);
        if (q.items.length === 0 && q.deferred.length === 0) return;
        // The reply a deferred message was waiting on was cancelled/failed —
        // move it back to the parked queue and pause so it isn't auto-sent; the
        // banner's "Send remaining" is the explicit resume.
        if (isStopCancellationNotice(frame.text)) {
          store.restoreDeferred(sid);
          store.setPause(sid, 'cancelled');
        } else if (frame.level === 'error') {
          store.restoreDeferred(sid);
          store.setPause(sid, 'error');
        }
      }
    },
    [sendToSession],
  );

  useEffect(() => {
    queueFrameRef.current = drainQueueOnFrame;
  }, [drainQueueOnFrame]);

  const uploadAttachment = useCallback(
    async (file: File) => {
      const localId = crypto.randomUUID();
      const mime = file.type || 'application/octet-stream';
      // Instant composer thumbnail for images, straight from the local file
      // (no upload round-trip needed to preview it).
      const previewUrl = mime.startsWith('image/') ? URL.createObjectURL(file) : undefined;
      setAttachments((prev) => [
        ...prev,
        { localId, filename: file.name, mime, size: file.size, status: 'uploading', previewUrl },
      ]);
      try {
        // The web operator's channel token authorises `/v1/blobs` (it
        // resolves to `AuthedClient::Web`, which bypasses pairing); the
        // returned content-addressed blob id is what the message references.
        const base = (baseUrl || '').replace(/\/+$/, '');
        const res = await fetch(`${base}/v1/blobs`, {
          method: 'POST',
          headers: {
            'x-aura-channel-token': channelToken ?? '',
            'content-type': mime,
          },
          body: file,
        });
        if (!res.ok) throw new Error(`upload failed: ${res.status}`);
        const data = (await res.json()) as { blob_id: string };
        setAttachments((prev) =>
          prev.map((a) =>
            a.localId === localId ? { ...a, status: 'ready', blobId: data.blob_id } : a,
          ),
        );
      } catch {
        setAttachments((prev) =>
          prev.map((a) => (a.localId === localId ? { ...a, status: 'error' } : a)),
        );
      }
    },
    [baseUrl, channelToken],
  );

  const handleFilePick = useCallback(
    (e: ChangeEvent<HTMLInputElement>) => {
      const files = e.target.files;
      if (files) {
        for (const file of Array.from(files)) void uploadAttachment(file);
      }
      // Reset so picking the same file again still fires `change`.
      e.target.value = '';
    },
    [uploadAttachment],
  );

  const removeAttachment = useCallback((localId: string) => {
    setAttachments((prev) => {
      const target = prev.find((a) => a.localId === localId);
      if (target?.previewUrl) URL.revokeObjectURL(target.previewUrl);
      return prev.filter((a) => a.localId !== localId);
    });
  }, []);

  const handleComposerKey = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        const hasReady = attachments.some((a) => a.status === 'ready');
        // Enter submits whether idle or busy — handleSend decides send vs park.
        if (composer.trim().length > 0 || hasReady) {
          const form = e.currentTarget.form;
          form?.requestSubmit();
        }
      }
    },
    [composer, attachments],
  );

  const handleComposerChange = useCallback(
    (value: string) => {
      setComposer(value);
      setShowSlashHints(value.startsWith('/') && slashCommands.length > 0);
    },
    [slashCommands.length],
  );

  // ── Interjection queue: composer/panel callbacks ────────────────────
  // Per-item send-icon: fire this item now (jumps the queue); mid-turn it lands
  // as an interjection, idle it starts a turn. Keeps any pauseReason.
  const fireQueuedItem = useCallback(
    (item: QueuedItem) => {
      if (!sessionId) return;
      // Mid-final-reply there are no tool boundaries left to interject at, and
      // sending now would either race the turn's end (double-fire risk) or split
      // the streaming answer into two bubbles. Instead defer the item: it leaves
      // the queue panel and renders in the thread below the agent's output,
      // dispatched on the turn's completion by the queue drain (or moved back to
      // the parked queue if the turn ends without a normal reply). Outside the
      // final-reply phase (tool work → interject; idle → start a turn) fire now.
      if (streamingAnswerRef.current.has(sessionId)) {
        queue.deferItem(item.id);
        return;
      }
      if (sendToSession(sessionId, item.text, item.attachments, { foreground: true })) {
        queue.removeItem(item.id);
      }
    },
    [sessionId, sendToSession, queue],
  );

  // Banner "Send remaining": clear the pause, fire the top item now, and let
  // the pipeline resume one-per-completion. clearPause then popTop compose in
  // one tick because the store updates its ref synchronously.
  const resumeQueue = useCallback(() => {
    if (!sessionId) return;
    const top = queue.items[0];
    if (!top) return;
    queue.clearPause();
    if (sendToSession(sessionId, top.text, top.attachments, { foreground: true })) {
      queue.popTop();
    }
  }, [sessionId, queue, sendToSession]);

  const handleApprovalDecision = useCallback(
    (decision: ApprovalDecision) => {
      if (!sessionId || !wsRef.current) return;
      const current = views[sessionId]?.pendingApproval;
      if (!current) return;
      wsRef.current.resolveApproval(current.callId, decision);
      // Optimistic dismissal — the server's ApprovalResolved fan-out
      // will also clear it via routeInboundFrame, harmless if it
      // arrives second.
      setViews((prev) => mergeView(prev, sessionId, { pendingApproval: null }));
    },
    [sessionId, views],
  );

  // Hiding a conversation is confirmed through an in-app dialog (not the
  // browser's native `confirm`). `hidePrompt` holds the row awaiting
  // confirmation; the delete only fires from `confirmHideSession`.
  const [hidePrompt, setHidePrompt] = useState<string | null>(null);
  const [hideSubmitting, setHideSubmitting] = useState(false);
  const [hideError, setHideError] = useState<string | null>(null);

  const handleHideSession = useCallback((id: string) => {
    setHideError(null);
    setHidePrompt(id);
  }, []);

  // Pin / unpin a conversation. Optimistic: flip the local row right
  // away so the sidebar reshuffles instantly, then PUT. The server's
  // SessionPatch broadcast converges every tab; on failure we revert
  // the optimistic flip (the row falls back to its prior block).
  const handleTogglePin = useCallback(
    async (id: string, pinned: boolean) => {
      setSessions((prev) => {
        const idx = prev.findIndex((s) => s.session_id === id);
        if (idx === -1 || prev[idx].pinned === pinned) return prev;
        const next = prev.slice();
        next[idx] = { ...prev[idx], pinned };
        return next;
      });
      const { error, response } = await client.PUT('/v1/chat/sessions/{session_id}/pin', {
        params: { path: { session_id: id } },
        body: { pinned },
      });
      if (error || !response.ok) {
        console.warn('toggle session pin failed', id, error);
        setSessions((prev) => {
          const idx = prev.findIndex((s) => s.session_id === id);
          if (idx === -1 || prev[idx].pinned !== pinned) return prev;
          const next = prev.slice();
          next[idx] = { ...prev[idx], pinned: !pinned };
          return next;
        });
      }
    },
    [client],
  );

  // ── Folder handlers ────────────────────────────────────────────────
  // Assign (or clear, with null) a session's folder. Optimistic; the
  // server's SessionPatch broadcast converges every tab. A pinned row
  // stays pinned (no auto-unpin) — the assignment just takes effect when
  // it's unpinned. On failure we revert to the prior folder.
  const handleAssignFolder = useCallback(
    async (id: string, folderId: string | null) => {
      let prevFolder: string | undefined;
      setSessions((prev) => {
        const idx = prev.findIndex((s) => s.session_id === id);
        if (idx === -1) return prev;
        prevFolder = prev[idx].folder_id;
        const next = prev.slice();
        next[idx] = { ...prev[idx], folder_id: folderId ?? undefined };
        return next;
      });
      const { error, response } = await client.PUT('/v1/chat/sessions/{session_id}/folder', {
        params: { path: { session_id: id } },
        body: { folder_id: folderId },
      });
      if (error || !response.ok) {
        console.warn('assign folder failed', id, error);
        setSessions((prev) => {
          const idx = prev.findIndex((s) => s.session_id === id);
          if (idx === -1) return prev;
          const next = prev.slice();
          next[idx] = { ...prev[idx], folder_id: prevFolder };
          return next;
        });
      }
    },
    [client],
  );

  // Folder CRUD — fire-and-converge: the server broadcasts a
  // Frame::FoldersChanged snapshot that the folderStore swaps in, so we
  // don't optimistically mutate the (store-owned) folder list here.
  const handleCreateFolder = useCallback(
    async (name: string, parentId?: string) => {
      const { error } = await client.POST('/v1/chat/folders', {
        body: { name, parent_id: parentId ?? null },
      });
      if (error) console.warn('create folder failed', error);
    },
    [client],
  );
  const handleRenameFolder = useCallback(
    async (id: string, name: string) => {
      const { error } = await client.PATCH('/v1/chat/folders/{folder_id}', {
        params: { path: { folder_id: id } },
        body: { name },
      });
      if (error) console.warn('rename folder failed', error);
    },
    [client],
  );
  const handleMoveFolder = useCallback(
    async (id: string, parentId: string | null) => {
      const { error } = await client.POST('/v1/chat/folders/{folder_id}/move', {
        params: { path: { folder_id: id } },
        body: { parent_id: parentId },
      });
      if (error) console.warn('move folder failed', error);
    },
    [client],
  );
  const handleReorderFolders = useCallback(
    async (parentId: string | null, orderedIds: string[]) => {
      const { error } = await client.POST('/v1/chat/folders/reorder', {
        body: { parent_id: parentId, ordered_ids: orderedIds },
      });
      if (error) console.warn('reorder folders failed', error);
    },
    [client],
  );
  const handleDeleteFolder = useCallback(
    async (id: string) => {
      const { error } = await client.DELETE('/v1/chat/folders/{folder_id}', {
        params: { path: { folder_id: id } },
      });
      if (error) console.warn('delete folder failed', error);
    },
    [client],
  );
  const handleNewChatInFolder = useCallback(
    async (folderId: string) => {
      setCreating(true);
      try {
        const { data } = await client.POST('/v1/chat/sessions', {});
        if (data?.session_id) {
          const sid = data.session_id;
          setSessions((prev) =>
            prev.some((s) => s.session_id === sid)
              ? prev
              : [
                  {
                    session_id: sid,
                    created_at: new Date().toISOString(),
                    last_active: new Date().toISOString(),
                    unread: 0,
                    pinned: false,
                    folder_id: folderId,
                  },
                  ...prev,
                ],
          );
          await client.PUT('/v1/chat/sessions/{session_id}/folder', {
            params: { path: { session_id: sid } },
            body: { folder_id: folderId },
          });
          navigateRef.current(`/chat/${sid}`);
        }
      } finally {
        setCreating(false);
      }
    },
    [client],
  );

  const cancelHideSession = useCallback(() => {
    if (hideSubmitting) return;
    setHidePrompt(null);
    setHideError(null);
  }, [hideSubmitting]);

  const confirmHideSession = useCallback(async () => {
    const id = hidePrompt;
    if (!id) return;
    setHideSubmitting(true);
    setHideError(null);
    const { error, response } = await client.DELETE('/v1/chat/sessions/{session_id}', {
      params: { path: { session_id: id } },
    });
    if (error || !response.ok) {
      // Surface server-side failure (404, etc.) in the dialog without
      // nuking the sidebar. The hide is server-authoritative; if the
      // call fails the row stays visible and the dialog stays open.
      setHideSubmitting(false);
      setHideError(error?.error ?? `HTTP ${response.status}`);
      return;
    }
    setSessions((prev) => prev.filter((s) => s.session_id !== id));
    releaseSessionView(id);
    if (sessionId === id) {
      const fallback =
        sessions.find((s) => s.session_id !== id)?.session_id ??
        (anchorSessionIdRef.current && anchorSessionIdRef.current !== id
          ? anchorSessionIdRef.current
          : null);
      if (fallback) {
        navigateRef.current(`/chat/${fallback}`, { replace: true });
      } else {
        navigateRef.current('/chat', { replace: true });
      }
    }
    setHideSubmitting(false);
    setHidePrompt(null);
  }, [client, hidePrompt, releaseSessionView, sessionId, sessions]);

  // Re-pin the active session's model. The PUT is authoritative — its
  // `last_llm` echo drives the local update, and a live actor (if any)
  // is re-pinned server-side to take effect on the session's next turn.
  // `name === null` clears the pin back to `default-llm`. Only a failure
  // surfaces a transcript notice; a successful switch is silent.
  const handleSelectModel = useCallback(
    async (name: string | null) => {
      if (!sessionId) return;
      const { response, data, error } = await client.PUT(
        '/v1/chat/sessions/{session_id}/model',
        {
          params: { path: { session_id: sessionId } },
          body: { llm: name },
        },
      );
      if (error || !response.ok) {
        console.warn('set session model failed', error);
        setViews((prev) => {
          const view = prev[sessionId];
          if (!view) return prev;
          return {
            ...prev,
            [sessionId]: {
              ...view,
              transcript: [
                ...view.transcript,
                {
                  key: `model-err-${Date.now()}`,
                  role: 'system',
                  text: '',
                  notice: {
                    level: 'error',
                    text: `Couldn't switch model: ${
                      error ? formatHttpError(error) : `HTTP ${response.status}`
                    }.`,
                  },
                },
              ],
            },
          };
        });
        return;
      }
      const applied = data?.last_llm ?? null;
      setViews((prev) => {
        const view = prev[sessionId] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sessionId]: { ...view, model: applied },
        };
      });
    },
    [client, sessionId],
  );

  const handleNewChat = useCallback(async () => {
    setCreating(true);
    try {
      const { data } = await client.POST('/v1/chat/sessions', {});
      if (data?.session_id) {
        // The server's Created broadcast (Frame::SessionUpdated, full
        // patch) reaches this tab too and `applySessionPatch` adds the
        // row for an unknown session_id. If that frame lands before the
        // POST response returns, an unconditional prepend here would
        // double the row. Guard with the same check createAnchorSession
        // uses — whichever path runs first wins; the second is a no-op.
        setSessions((prev) =>
          prev.some((s) => s.session_id === data.session_id)
            ? prev
            : [
                {
                  session_id: data.session_id,
                  created_at: new Date().toISOString(),
                  last_active: new Date().toISOString(),
                  unread: 0,
                  pinned: false,
                },
                ...prev,
              ],
        );
        navigateRef.current(`/chat/${data.session_id}`);
      }
    } finally {
      setCreating(false);
    }
  }, [client]);

  const filteredSlash = useMemo(() => {
    if (!showSlashHints) return [];
    const query = composer.slice(1).split(/\s/)[0]?.toLowerCase() ?? '';
    return slashCommands.filter(
      (s) => query.length === 0 || s.command.toLowerCase().startsWith(query),
    );
  }, [showSlashHints, composer, slashCommands]);

  const pendingApprovalIds = useMemo(
    () =>
      new Set(
        Object.entries(views)
          .filter(([, v]) => v.pendingApproval)
          .map(([id]) => id),
      ),
    [views],
  );

  return (
    <div className="flex flex-1 overflow-hidden bg-surface min-h-0">
      {/* Session list sidebar (zone 2; the global icon rail is zone 1) */}
      <SessionSidebar
        sessions={sessions}
        activeSessionId={sessionId}
        pendingIds={pendingApprovalIds}
        creating={creating}
        loading={sessionsLoading}
        onNewChat={handleNewChat}
        onHide={handleHideSession}
        onTogglePin={handleTogglePin}
        onAssignFolder={handleAssignFolder}
        onCreateFolder={handleCreateFolder}
        onRenameFolder={handleRenameFolder}
        onMoveFolder={handleMoveFolder}
        onReorderFolders={handleReorderFolders}
        onDeleteFolder={handleDeleteFolder}
        onNewChatInFolder={handleNewChatInFolder}
      />

      {/* Main column */}
      <main className="flex-1 flex flex-col overflow-hidden relative">
        <CronInbox refreshSignal={cronInboxRefresh} />
        <header className="h-12 px-4 border-b-2 border-black flex items-center justify-between gap-3 bg-canvas">
          <div className="flex items-baseline gap-2 min-w-0 flex-1">
            {sessionId ? (
              <span
                className="font-mono text-xs text-ink select-all break-all"
                title={sessionId}
              >
                <span className="text-ink-soft select-none mr-1">session id:</span>
                {sessionId}
              </span>
            ) : (
              <span className="font-bold text-sm text-ink-soft">No session</span>
            )}
          </div>
          <div className="flex items-center gap-3 shrink-0">
            <ConnectionBadge status={status} />
          </div>
        </header>

        <div className="flex-1 flex flex-col overflow-hidden relative xl:pr-[260px]">
        <div className="flex-1 flex justify-center min-h-0 relative">
        <div
          ref={transcriptScrollRef}
          onScroll={handleTranscriptScroll}
          className="chat-scroll chat-scroll-centered relative w-full overflow-y-auto overflow-x-hidden px-6 pt-4 pb-40"
        >
          <TaskChecklist tasks={currentView.tasks} />
          {currentView.historyLoading ? (
            <div className="flex justify-center py-12 text-ink-soft">
              <RiLoader4Line className="text-3xl animate-spin" />
            </div>
          ) : currentView.transcript.length === 0 && !currentView.pendingApproval ? (
            <WelcomeEmpty slashCommands={slashCommands} onPick={handleComposerChange} />
          ) : (
            <div className="flex flex-col gap-3 w-full max-w-4xl mx-auto">
              {currentView.olderLoading ? (
                <div className="flex justify-center py-2 text-ink-soft">
                  <RiLoader4Line className="text-xl animate-spin" />
                </div>
              ) : currentView.hasMore ? (
                <div className="flex justify-center py-1 text-[0.7rem] font-mono text-ink-soft uppercase tracking-wider">
                  scroll up to load older messages
                </div>
              ) : null}
              {currentView.transcript.flatMap((row, i, arr) => {
                const nodes: React.ReactNode[] = [
                  <MessageBubble
                    key={row.key}
                    row={row}
                    channelToken={channelToken}
                    baseUrl={baseUrl}
                  />,
                ];
                if (isCancelledWorkAt(arr, i, currentView.turn)) {
                  nodes.push(
                    <CancelledTurnIndicator key={`${row.key}-cancelled`} />,
                  );
                }
                return nodes;
              })}
              {/* Deferred ("send after the reply") messages render as dimmed
                  user bubbles pinned below the agent's output — never woven into
                  the streaming transcript array, so they can't split the answer
                  bubble. They dispatch (and become real transcript rows) once the
                  turn completes. */}
              {queue.deferred.map((item) => (
                <MessageBubble
                  key={`deferred-${item.id}`}
                  row={{
                    key: `deferred-${item.id}`,
                    role: 'user',
                    text: item.text,
                    attachments: item.attachments,
                    hasAttachments: item.attachments.length > 0,
                    pending: true,
                  }}
                  channelToken={channelToken}
                  baseUrl={baseUrl}
                />
              ))}
              {currentView.awaitingReply ? <WorkingIndicator /> : null}
              {currentView.pendingApproval ? (
                <ApprovalCard
                  approval={currentView.pendingApproval}
                  onDecide={handleApprovalDecision}
                  connected={status.state === 'connected'}
                />
              ) : null}
              <div ref={transcriptEndRef} />
            </div>
          )}
        </div>
        </div>

        {hasNewBelow ? (
          <button
            type="button"
            onClick={jumpToLatest}
            className="absolute bottom-32 left-1/2 -translate-x-1/2 z-30 flex items-center gap-1.5 px-3 py-1.5 bg-white border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:bg-gray-100 cursor-pointer"
            title="Jump to latest"
          >
            <RiArrowDownLine className="text-base" />
            New messages
          </button>
        ) : null}

        {/* Floating composer pill (app/mac-style): hovers over the thread
            bottom, centered on the reading column. */}
        <div className="pointer-events-none absolute inset-x-0 bottom-0 z-20 flex justify-center px-6 pb-6 xl:pr-[284px]">
          <form
            onSubmit={handleSend}
            className="pointer-events-auto relative w-full max-w-4xl"
          >
            {/* The thread scrolls *behind* the floating composer. A page-colour
                gradient (transparent at the top → opaque canvas) makes bubbles
                fade out as they slide into the composer — fully gone by roughly
                the pill's middle — while keeping the area below the input clear.
                Scoped to the form (the band width) so it never paints over the
                right-hand panel/divider; `-bottom-6` reaches the viewport edge
                under the pill and `-top-20` lifts the fade-in into the thread. */}
            <div
              aria-hidden
              className="pointer-events-none absolute inset-x-0 -bottom-6 -top-20 bg-linear-to-t from-surface from-40% to-transparent"
            />
            {sessionId ? (
              <QueuePanel
                sessionId={sessionId}
                baseUrl={baseUrl}
                channelToken={channelToken}
                onFire={fireQueuedItem}
                onResume={resumeQueue}
              />
            ) : null}

            <div className="relative border-2 border-black rounded-2xl bg-white shadow-brutal focus-within:shadow-brutal transition-shadow">
              {/* Slash-command autocomplete floats directly over the input box.
                  Anchored to the pill (not the form) so the queue panel sitting
                  above the pill can't push it up. */}
              {filteredSlash.length > 0 ? (
                <div className="absolute bottom-full left-0 right-0 mb-2 z-30 border-2 border-black bg-white rounded-2xl shadow-brutal px-2 py-2 flex flex-col gap-0.5">
                  <div className="px-2 pb-1 text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
                    Slash commands
                  </div>
                  {filteredSlash.map((s) => (
                    <button
                      key={s.command}
                      type="button"
                      onClick={() => {
                        handleComposerChange(`/${s.command} `);
                      }}
                      className="text-left px-2 py-1.5 border-2 border-transparent hover:border-black hover:bg-canvas rounded font-mono text-sm flex items-center gap-2 cursor-pointer"
                    >
                      <span className="font-bold shrink-0">/{s.command}</span>
                      <span className="text-ink-soft truncate">{s.description}</span>
                    </button>
                  ))}
                </div>
              ) : null}
              {attachments.length > 0 ? (
                <div className="flex flex-wrap gap-1.5 px-3 pt-2.5">
                  {attachments.map((a) =>
                    a.previewUrl ? (
                      <span key={a.localId} className="relative inline-flex shrink-0">
                        <img
                          src={a.previewUrl}
                          alt={a.filename}
                          title={a.filename}
                          className={`h-14 w-14 object-cover rounded-md border-2 border-black ${
                            a.status === 'error' ? 'opacity-40' : ''
                          }`}
                        />
                        {a.status === 'uploading' ? (
                          <span className="absolute inset-0 flex items-center justify-center bg-white/60 rounded-md">
                            <RiLoader4Line className="text-base animate-spin" />
                          </span>
                        ) : null}
                        <button
                          type="button"
                          onClick={() => removeAttachment(a.localId)}
                          className="absolute -top-1.5 -right-1.5 h-4 w-4 flex items-center justify-center bg-white border-2 border-black rounded-full text-ink hover:bg-err hover:text-white cursor-pointer"
                          aria-label="Remove attachment"
                        >
                          <RiCloseLine className="text-[0.6rem]" />
                        </button>
                      </span>
                    ) : (
                      <span
                        key={a.localId}
                        className={`flex items-center gap-1 max-w-[200px] px-2 py-0.5 border-2 border-black rounded-md font-mono text-[0.7rem] ${
                          a.status === 'error' ? 'bg-err/10 text-err' : 'bg-canvas text-ink'
                        }`}
                      >
                        {a.status === 'uploading' ? (
                          <RiLoader4Line className="text-xs animate-spin shrink-0" />
                        ) : a.status === 'error' ? (
                          <RiCloseLine className="text-xs shrink-0" />
                        ) : (
                          <RiAttachmentLine className="text-xs shrink-0" />
                        )}
                        <span className="truncate" title={a.filename}>
                          {a.filename}
                        </span>
                        <button
                          type="button"
                          onClick={() => removeAttachment(a.localId)}
                          className="shrink-0 text-ink-soft hover:text-err cursor-pointer"
                          aria-label="Remove attachment"
                        >
                          <RiCloseLine className="text-xs" />
                        </button>
                      </span>
                    ),
                  )}
                </div>
              ) : null}
              <textarea
                ref={composerRef}
                value={composer}
                onChange={(e) => handleComposerChange(e.target.value)}
                onKeyDown={handleComposerKey}
                placeholder={
                  status.state === 'connected'
                    ? 'Message Aura…  (Shift+Enter for newline)'
                    : 'Waiting for connection…'
                }
                rows={1}
                className="w-full px-4 pt-3 pb-1.5 font-sans text-sm bg-transparent resize-none focus:outline-none leading-relaxed placeholder:text-ink-soft/70"
              />

              <div className="flex items-center justify-between gap-2 px-2.5 pb-2 pt-0.5">
                <div className="flex items-center gap-2 min-w-0 flex-1">
                  <input
                    ref={fileInputRef}
                    type="file"
                    multiple
                    className="hidden"
                    onChange={handleFilePick}
                  />
                  <button
                    type="button"
                    onClick={() => fileInputRef.current?.click()}
                    disabled={!channelToken || status.state !== 'connected'}
                    className="group shrink-0 h-7 w-7 flex items-center justify-center bg-surface text-ink-soft hover:text-ink border-2 border-black rounded-md shadow-brutal-xs hover:bg-canvas hover:-translate-y-px active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer transition-[transform,box-shadow,background-color,color] duration-150"
                    title="Attach image or file"
                    aria-label="Attach image or file"
                  >
                    <RiAttachmentLine className="text-base transition-transform duration-200 group-hover:-rotate-[18deg] group-hover:scale-110" />
                  </button>
                </div>
                {sessionId && models.length > 1 ? (
                  <ModelPicker
                    models={models}
                    defaultName={defaultModelName}
                    current={currentView.model}
                    onSelect={handleSelectModel}
                  />
                ) : null}
                {busy && !hasContent ? (
                  <button
                    key="composer-stop"
                    type="button"
                    onClick={handleStop}
                    disabled={status.state !== 'connected'}
                    className="shrink-0 h-8 w-8 flex items-center justify-center bg-err text-white border-2 border-black rounded-full shadow-brutal-xs hover:opacity-90 active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                    title="Stop the current turn (/stop). Type /stop to stop while drafting."
                    aria-label="Stop the current turn"
                  >
                    <RiStopFill className="text-base" />
                  </button>
                ) : (
                  <button
                    key="composer-send"
                    type="submit"
                    disabled={
                      !sessionId ||
                      status.state !== 'connected' ||
                      attachments.some((a) => a.status === 'uploading') ||
                      !hasContent
                    }
                    className="shrink-0 h-8 w-8 flex items-center justify-center bg-brand text-ink border-2 border-black rounded-full shadow-brutal-xs hover:bg-brand-hover active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
                    title={
                      busy || queue.pauseReason !== null
                        ? 'Queue message (Enter)'
                        : 'Send (Enter)'
                    }
                  >
                    <RiSendPlane2Line className="text-base" />
                  </button>
                )}
              </div>
            </div>
          </form>
        </div>
        </div>
      </main>

      {hidePrompt ? (
        <HideSessionModal
          submitting={hideSubmitting}
          error={hideError}
          onCancel={cancelHideSession}
          onConfirm={confirmHideSession}
        />
      ) : null}
    </div>
  );
}

/** In-app confirmation for hiding a conversation, replacing the browser's
 *  native `confirm`. Hiding only filters the row from the user's list —
 *  the session row, transcript, and binding stay live on the server — so
 *  the copy frames it as a recoverable "remove from list", not a delete.
 *  Backdrop click and the Escape key both cancel (while idle). */
function HideSessionModal({
  submitting,
  error,
  onCancel,
  onConfirm,
}: {
  submitting: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') onCancel();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onCancel]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onCancel}
    >
      <div
        className="max-w-md w-full bg-surface border-[3px] border-black rounded-md shadow-brutal overflow-hidden"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="px-6 py-4 border-b-2 border-black">
          <h3 className="font-bold uppercase tracking-wider">Remove conversation</h3>
        </header>
        <div className="px-6 py-4 space-y-3">
          <p className="text-[0.95rem] leading-relaxed">
            Remove this conversation from your list?
          </p>
          {error ? (
            <div className="bg-surface border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.85rem]">
              {error}
            </div>
          ) : null}
        </div>
        <footer className="flex justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
          <button
            type="button"
            onClick={onCancel}
            disabled={submitting}
            className="h-9 px-3 border-2 border-black rounded-md bg-surface font-bold uppercase tracking-wider text-[0.85rem] shadow-brutal-xs hover:bg-canvas active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={onConfirm}
            disabled={submitting}
            className="h-9 px-3 inline-flex items-center gap-1.5 border-2 border-err rounded-md bg-err text-white font-bold uppercase tracking-wider text-[0.85rem] shadow-brutal-xs hover:bg-err/90 active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
          >
            {submitting ? (
              <RiLoader4Line className="animate-spin text-base shrink-0" />
            ) : (
              <RiDeleteBin6Line className="text-base shrink-0" />
            )}
            Remove
          </button>
        </footer>
      </div>
    </div>
  );
}

/** Pull a one-line, human-readable reason from an openapi-fetch error
 *  body. Falls back to JSON-stringifying it so the user at least sees
 *  *something* instead of `[object Object]`. */
function formatHttpError(err: unknown): string {
  if (err && typeof err === 'object' && 'message' in err) {
    const msg = (err as { message?: unknown }).message;
    if (typeof msg === 'string' && msg.length > 0) return msg;
  }
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return 'unknown error';
  }
}

/** Merge a `SessionUpdated` patch onto the sidebar's session list.
 *
 *  Rules:
 *  * `hidden: true` removes the row (sidebar never shows hidden);
 *  * a patch for an unknown session_id constructs a row iff it
 *    carries enough fields (currently `created_at` + `last_active`);
 *    a sparse `last_active`-only patch for an unknown session is
 *    dropped on the floor (Created arrives separately and adds it).
 *  * Otherwise present fields are merged in place — absent fields
 *    keep their previous values.
 *
 *  Row order is never touched: fields merge in place, so a live update
 *  (a session bumping its activity) cannot reposition the row. This is
 *  deliberate — concurrent replies must not reshuffle the list under the
 *  user. A genuinely new session is prepended (newest first). */
function applySessionPatch(
  prev: SessionSummary[],
  sessionId: string,
  patch: SessionPatch,
): SessionSummary[] {
  if (patch.hidden === true) {
    return prev.filter((s) => s.session_id !== sessionId);
  }
  // Resolve the three-state folder change: absent ⇒ keep current,
  // `'uncategorized'` ⇒ clear, `{ set: { id } }` ⇒ that folder.
  const patchedFolder =
    patch.folder_id === undefined
      ? undefined
      : patch.folder_id === 'uncategorized'
        ? undefined
        : patch.folder_id.set.id;
  const idx = prev.findIndex((s) => s.session_id === sessionId);
  if (idx === -1) {
    if (patch.created_at == null || patch.last_active == null) return prev;
    return [
      {
        session_id: sessionId,
        created_at: patch.created_at,
        last_active: patch.last_active,
        unread: 0,
        pinned: patch.pinned ?? false,
        folder_id: patchedFolder,
      },
      ...prev,
    ];
  }
  const current = prev[idx];
  const nextFolderId =
    patch.folder_id === undefined ? current.folder_id : patchedFolder;
  const merged: SessionSummary = {
    session_id: current.session_id,
    created_at: patch.created_at ?? current.created_at,
    last_active: patch.last_active ?? current.last_active,
    unread: current.unread,
    pinned: patch.pinned ?? current.pinned,
    last_user_text: current.last_user_text,
    folder_id: nextFolderId,
  };
  if (
    merged.created_at === current.created_at &&
    merged.last_active === current.last_active &&
    merged.pinned === current.pinned &&
    merged.folder_id === current.folder_id
  ) {
    return prev;
  }
  const next = prev.slice();
  next[idx] = merged;
  return next;
}

/** Merge a `SessionActivity` ping onto the sidebar list. Projects
 *  `at` onto the row's local `last_active` (so the age string stays
 *  current without a list refetch) and bumps `unread` iff the activity
 *  isn't on the currently-foregrounded session. The row is updated in
 *  place — never repositioned — so concurrent replies don't reshuffle
 *  the list. Activity for sessions we don't know about (raced ahead of
 *  Created, or hidden in this tab) is dropped on the floor — Created
 *  arrives separately, and rehydration after a hide isn't worth
 *  optimising. */
function applySessionActivity(
  prev: SessionSummary[],
  sessionId: string,
  at: string,
  isForeground: boolean,
): SessionSummary[] {
  const idx = prev.findIndex((s) => s.session_id === sessionId);
  if (idx === -1) return prev;
  const current = prev[idx];
  const nextLastActive =
    Date.parse(at) > Date.parse(current.last_active) ? at : current.last_active;
  const nextUnread = isForeground ? current.unread : current.unread + 1;
  if (nextLastActive === current.last_active && nextUnread === current.unread) {
    return prev;
  }
  const next = prev.slice();
  next[idx] = { ...current, last_active: nextLastActive, unread: nextUnread };
  return next;
}

/** Soft cap on the sidebar preview length. Mirrors `PREVIEW_MAX_CHARS`
 *  on the gateway side — server-supplied previews already arrive
 *  pre-truncated, but local updates (this tab's send, sibling tab's
 *  UserEcho) go through this so the row stays at the same width
 *  regardless of which path filled it. */
const PREVIEW_MAX_CHARS = 120;

/** Replace `session_id`'s preview text with the freshest user turn.
 *  Collapses whitespace + truncates to mirror the server's
 *  `truncate_preview`. Returns `prev` unchanged when the target row
 *  isn't in the list (sidebar dropped it via hide, or the activity
 *  raced ahead of Created) — Created arrives separately and seeds the
 *  row, and the next list refresh will reseed the preview. */
function applySessionUserText(
  prev: SessionSummary[],
  sessionId: string,
  text: string,
): SessionSummary[] {
  const idx = prev.findIndex((s) => s.session_id === sessionId);
  if (idx === -1) return prev;
  const collapsed = text.replace(/\s+/g, ' ').trim();
  if (!collapsed) return prev;
  const truncated =
    collapsed.length > PREVIEW_MAX_CHARS
      ? `${collapsed.slice(0, PREVIEW_MAX_CHARS)}…`
      : collapsed;
  if (prev[idx].last_user_text === truncated) return prev;
  const next = prev.slice();
  next[idx] = { ...prev[idx], last_user_text: truncated };
  return next;
}

/** `HH:MM` for messages from today, `MM-DD HH:MM` for older rows.
 *  Keeps each bubble's timestamp short enough to sit next to a
 *  280px-wide bubble without wrapping, while still disambiguating
 *  long sessions that span multiple days. */
function formatTimestampShort(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const hm = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  if (sameDay) return hm;
  return `${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${hm}`;
}

/** Full date + time for the bubble's hover tooltip — locale-formatted
 *  so users in different time zones see something they recognize
 *  rather than the wire ISO. */
function formatTimestampTooltip(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString();
}

function pad2(n: number): string {
  return n < 10 ? `0${n}` : `${n}`;
}

// ── visual components ───────────────────────────────────────────────

// Brutalist override map for ReactMarkdown. Keeps the bubble feeling
// like a chat bubble (tight spacing, no doc-style top margins) while
// still giving lists / code / quotes / tables a recognizable shape.
// `first:mt-0 last:mb-0` on the block elements keeps the bubble's top
// and bottom edges from gaining extra padding from leading/trailing
// markdown blocks.
const MARKDOWN_COMPONENTS: Components = {
  p: ({ children }) => (
    <p className="my-2 first:mt-0 last:mb-0 leading-relaxed">{children}</p>
  ),
  h1: ({ children }) => (
    <h1 className="my-2 first:mt-0 last:mb-0 text-base font-bold uppercase tracking-wider">
      {children}
    </h1>
  ),
  h2: ({ children }) => (
    <h2 className="my-2 first:mt-0 last:mb-0 text-base font-bold">{children}</h2>
  ),
  h3: ({ children }) => (
    <h3 className="my-2 first:mt-0 last:mb-0 text-sm font-bold">{children}</h3>
  ),
  h4: ({ children }) => (
    <h4 className="my-2 first:mt-0 last:mb-0 text-sm font-bold">{children}</h4>
  ),
  h5: ({ children }) => (
    <h5 className="my-2 first:mt-0 last:mb-0 text-sm font-bold">{children}</h5>
  ),
  h6: ({ children }) => (
    <h6 className="my-2 first:mt-0 last:mb-0 text-sm font-bold">{children}</h6>
  ),
  // Marker geometry (fixed left-edge bullets / numbers) lives in the
  // `.md-list` rules in `index.css` — native `outside` markers right-align
  // to the text, which left a numbered list hanging further left than a
  // bulleted one. Keep vertical rhythm here in utilities.
  ul: ({ children }) => (
    <ul className="md-list my-2 first:mt-0 last:mb-0 space-y-1">{children}</ul>
  ),
  ol: ({ children }) => (
    <ol className="md-list my-2 first:mt-0 last:mb-0 space-y-1">{children}</ol>
  ),
  // `leading-relaxed` (not snug) so a tight list's text line-height matches the
  // loose list's paragraph and the `.md-list` marker (which inherits it), keeping
  // the marker on the first text line in both. See `.md-list` in index.css.
  li: ({ children }) => <li className="leading-relaxed">{children}</li>,
  a: ({ href, children }) => (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className="text-info underline underline-offset-2 hover:opacity-80 break-words"
    >
      {children}
    </a>
  ),
  strong: ({ children }) => <strong className="font-bold">{children}</strong>,
  em: ({ children }) => <em className="italic">{children}</em>,
  blockquote: ({ children }) => (
    <blockquote className="my-2 first:mt-0 last:mb-0 border-l-4 border-black pl-3 italic text-ink-soft">
      {children}
    </blockquote>
  ),
  hr: () => <hr className="my-3 border-t-2 border-black" />,
  // `inline` is false for fenced code blocks; ReactMarkdown wraps those
  // in `<pre><code>…</code></pre>`, so the inline branch handles the
  // `\`foo\`` case and the block branch is rendered via `pre`.
  code: ({ className, children, ...rest }) => {
    const isInline = !/^language-/.test(className ?? '');
    if (isInline) {
      return (
        <code
          className="font-mono bg-canvas border border-black/30 rounded px-1 py-[1px] text-[0.85em]"
          {...rest}
        >
          {children}
        </code>
      );
    }
    return (
      <code className={`${className ?? ''} block`} {...rest}>
        {children}
      </code>
    );
  },
  pre: ({ children }) => (
    <pre className="font-mono my-2 first:mt-0 last:mb-0 bg-canvas border-2 border-black rounded-md p-2 overflow-x-auto text-xs leading-snug">
      {children}
    </pre>
  ),
  table: ({ children }) => (
    <div className="my-2 first:mt-0 last:mb-0 overflow-x-auto">
      <table className="border-2 border-black border-collapse text-xs">{children}</table>
    </div>
  ),
  thead: ({ children }) => <thead className="bg-canvas">{children}</thead>,
  th: ({ children }) => (
    <th className="border border-black px-2 py-1 font-bold uppercase tracking-wider text-[0.65rem] text-left">
      {children}
    </th>
  ),
  td: ({ children }) => <td className="border border-black px-2 py-1">{children}</td>,
};

const REMARK_PLUGINS = [remarkGfm];

function MarkdownBody({ text }: { text: string }) {
  return (
    <ReactMarkdown components={MARKDOWN_COMPONENTS} remarkPlugins={REMARK_PLUGINS}>
      {text}
    </ReactMarkdown>
  );
}

function AttachmentList({
  attachments,
  baseUrl,
  channelToken,
}: {
  attachments: WireAttachment[];
  baseUrl: string;
  channelToken: string | null;
}) {
  return (
    <div className="flex flex-wrap gap-2">
      {attachments.map((a, i) =>
        a.kind === 'image' ? (
          <AttachmentImage
            key={`${a.blob_id}-${i}`}
            blobId={a.blob_id}
            alt={a.filename ?? 'image'}
            baseUrl={baseUrl}
            channelToken={channelToken}
          />
        ) : (
          <span
            key={`${a.blob_id}-${i}`}
            className="flex items-center gap-1.5 px-2 py-1 bg-canvas border-2 border-black rounded-md font-mono text-[0.7rem] max-w-full"
            title={a.filename ?? a.mime_type}
          >
            <RiFileLine className="text-sm shrink-0" />
            <span className="truncate">{a.filename ?? a.mime_type}</span>
          </span>
        ),
      )}
    </div>
  );
}

function MessageBubble({
  row,
  channelToken,
  baseUrl,
}: {
  row: TranscriptRow;
  channelToken: string | null;
  baseUrl: string;
}) {
  if (row.kind === 'work') {
    return <WorkBlock row={row} />;
  }
  if (row.notice) {
    const palette =
      row.notice.level === 'error'
        ? 'bg-err/10 border-err text-err'
        : row.notice.level === 'warn'
          ? 'bg-warn/10 border-warn text-warn'
          : 'bg-info/10 border-info text-info';
    return (
      <div className="flex flex-col items-start min-w-0">
        <div className="flex flex-col w-fit min-w-0">
          <div
            className={`border-2 rounded-md px-3 py-2 font-mono text-sm whitespace-pre-wrap ${palette}`}
          >
            {row.notice.text}
          </div>
          {row.createdAt ? (
            <span
              className="mt-1 px-1 self-start font-mono text-[0.65rem] text-ink-soft tabular-nums"
              title={formatTimestampTooltip(row.createdAt)}
            >
              {formatTimestampShort(row.createdAt)}
            </span>
          ) : null}
        </div>
      </div>
    );
  }
  const isUser = row.role === 'user';
  // Live rows carry full attachment details (render thumbnails/filenames);
  // history rows carry only `hasAttachments`, so those still fall back to
  // the `[attachment]` placeholder.
  const attachmentDetails = row.attachments ?? [];
  const body =
    row.text ||
    (row.hasAttachments && attachmentDetails.length === 0 ? '[attachment]' : '');
  // Markdown rendering is reserved for assistant output — user input
  // is left as plain pre-wrap so markdown-looking syntax (e.g. paths
  // with underscores, leading hashes in shell logs, raw HTML tags)
  // shows up verbatim instead of being silently reinterpreted. The
  // streaming caret is also dropped on the markdown side: the pacer's
  // character-by-character reveal already conveys "in progress", and
  // a caret pinned to the bubble's tail would land below a block
  // element when the last token is a code fence or list, looking off.
  const showMarkdown = !isUser && !row.notice && body.length > 0;
  return (
    <div className={`group flex flex-col min-w-0 ${isUser ? 'items-end' : 'items-start'}`}>
      <div className={`flex flex-col w-fit min-w-0 ${isUser ? 'max-w-2xl' : 'max-w-4xl'}`}>
        <div className="relative min-w-0">
          <div
            className={`rounded-md py-2 text-sm text-ink transition-opacity break-words [overflow-wrap:anywhere] ${
              showMarkdown ? 'chat-prose' : 'font-mono whitespace-pre-wrap'
            } ${isUser ? 'border-2 border-black px-3 bg-brand/60 shadow-brutal-sm' : ''} ${
              row.pending ? 'opacity-60' : ''
            }`}
          >
            {attachmentDetails.length > 0 ? (
              <div className={body ? 'mb-1.5' : ''}>
                <AttachmentList
                  attachments={attachmentDetails}
                  baseUrl={baseUrl}
                  channelToken={channelToken}
                />
              </div>
            ) : null}
            {showMarkdown ? (
              <MarkdownBody text={body} />
            ) : (
              <>
                {body}
                {row.streaming ? (
                  <span className="inline-block w-1.5 h-3 ml-0.5 align-baseline bg-current animate-pulse" />
                ) : null}
              </>
            )}
          </div>
          {row.pending ? (
            <RiLoader4Line
              className="absolute -bottom-1.5 -right-1.5 text-sm bg-white text-ink rounded-full border-2 border-black animate-spin"
              title="Sending…"
            />
          ) : null}
        </div>
        {row.createdAt || (!isUser && !row.streaming && body) ? (
          <div className="mt-1 flex items-center gap-1.5 self-start">
            {row.createdAt ? (
              <span
                className="font-mono text-[0.65rem] text-ink-soft tabular-nums"
                title={formatTimestampTooltip(row.createdAt)}
              >
                {formatTimestampShort(row.createdAt)}
              </span>
            ) : null}
            {!isUser && !row.streaming && body ? <CopyButton text={body} /> : null}
          </div>
        ) : null}
      </div>
    </div>
  );
}

// One rendered step inside a work block — mirrors the old inline
// reasoning / tool / status visuals, plus `prose` for folded mid-turn
// answer text. Reused by both the live (active) panel and the expanded
// collapsed view.
function WorkStepView({ step }: { step: WorkStep }) {
  if (step.kind === 'reasoning') {
    return (
      <div className="flex items-start gap-2 font-mono text-xs text-ink-soft whitespace-pre-wrap">
        <span className="select-none">✻</span>
        <span className="italic">{step.text}</span>
      </div>
    );
  }
  if (step.kind === 'status') {
    return (
      <div className="flex items-center gap-2 font-mono text-xs text-ink-soft">
        <span className="select-none">⟳</span>
        <span>{step.text}</span>
      </div>
    );
  }
  if (step.kind === 'prose') {
    // Intermediate reply text the model emitted between tool calls (the final
    // answer streams in its own bubble, not here). Render it with the answer
    // bubble's prose styling — not the dim mono of the reasoning/tool steps —
    // so it reads as reply text when the block is expanded.
    return (
      <div className="chat-prose text-ink">
        <MarkdownBody text={step.text ?? ''} />
      </div>
    );
  }
  const statusColor =
    step.toolStatus === 'error'
      ? 'text-err'
      : step.toolStatus === 'denied'
        ? 'text-warn'
        : 'text-ink-soft';
  return (
    <div className="flex flex-col gap-0.5 font-mono text-xs">
      <div className="flex items-center gap-1.5">
        <span className="text-info">⏺</span>
        <span className="font-bold text-ink">{step.tool}</span>
        {step.toolLabel ? <span className="text-ink-soft">({step.toolLabel})</span> : null}
        {step.toolStatus === 'running' ? (
          <RiLoader4Line className="text-ink-soft animate-spin" title="Running…" />
        ) : null}
      </div>
      {step.toolSummary ? (
        <div className={`flex items-start gap-1.5 pl-1 ${statusColor}`}>
          <span className="select-none">⎿</span>
          <span className="whitespace-pre-wrap">{step.toolSummary}</span>
        </div>
      ) : null}
    </div>
  );
}

// The turn's aggregated progress. A live turn that hasn't produced a step
// yet is just the compact "Working" spinner (matching the initial
// WorkingIndicator); the bordered bubble grows its steps panel in only once
// work actually lands. On completion it collapses to a dim `Worked Xs ›`
// line (click to re-expand) that sits above the final answer bubble. A turn
// that produced no steps is dropped on close (see `closeActiveWork`), so a
// collapsed block always has work to show and the arrow is always meaningful.
function WorkBlock({ row }: { row: TranscriptRow }) {
  const active = !!row.workActive;
  const steps = row.steps ?? [];
  // `expanded` is the user's explicit toggle for the *finished* block and
  // defaults closed. Two derived flags drive the look — deriving them rather
  // than flipping state in an effect keeps every transition a single render
  // (the body animates 0fr↔1fr cleanly, no flash):
  //  • `boxed`     — show the bordered card. True while the turn is live (so
  //    it reads as one element with the initial WorkingIndicator) or when the
  //    user re-expanded a finished block.
  //  • `panelOpen` — reveal the steps panel. Held shut until the turn has
  //    actually produced a step, so a live-but-stepless turn is just the
  //    compact spinner and the panel grows in when the first step lands.
  const [expanded, setExpanded] = useState(false);
  const hasSteps = steps.length > 0;
  // A block closed mid-turn by an interjection: keep it open ("Worked Xs",
  // steps shown) until the turn ends and `closeActiveWork` clears the flag.
  const settling = !!row.workSettling;
  const { boxed, panelOpen } = workBlockDisplay(active, hasSteps, expanded, settling);
  // The spinner-first state hugs its content instead of stretching the full
  // card width; the panel/steps states take the full width so work has room.
  const compact = boxed && !panelOpen;

  // Pin the steps panel to its tail while the turn is producing so a long
  // tool loop reveals the newest reasoning/tool line at the bottom instead
  // of stranding the user at the top — but ONLY while the user is parked at
  // (or near) the bottom. Once they scroll up to read an earlier line we stop
  // yanking them back down, and re-engage when they scroll back to the end.
  // Layout-effect-scoped so the catch-up happens pre-paint, no visible flash.
  const stepsContainerRef = useRef<HTMLDivElement | null>(null);
  const stepsPinnedRef = useRef(true);
  const handleStepsScroll = useCallback(() => {
    const el = stepsContainerRef.current;
    if (!el) return;
    const slackPx = 48;
    stepsPinnedRef.current =
      el.scrollHeight - el.scrollTop - el.clientHeight <= slackPx;
  }, []);
  useLayoutEffect(() => {
    if (!active || !stepsPinnedRef.current) return;
    const el = stepsContainerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [active, steps]);

  if (!active && steps.length === 0) return null;

  const secs =
    row.workEndedAt && row.workStartedAt
      ? Math.max(0, Math.round((row.workEndedAt - row.workStartedAt) / 1000))
      : 0;
  // Never surface a "0s" duration; a cancelled (`/stop`) turn reads
  // "Cancelled · Worked Xs" instead of a plain completion summary.
  const cancelled = !!row.workCancelled;
  const workedLabel = formatWorkedLabel(secs, cancelled);

  // One persistent element tree across active / collapsed / expanded so
  // the transitions actually animate (a branch swap would just hard-cut).
  // The chrome (border + bg + shadow) fades via `transition-all`; the
  // steps panel grows/shrinks via the grid-rows 0fr↔1fr trick — the
  // dependable way to animate to/from content height. Border *width*
  // stays 2px in every state (only its color fades) so nothing reflows.
  return (
    <div className="group flex flex-col items-start w-full">
      <div
        className={`${
          compact ? 'w-fit max-w-4xl' : 'w-full max-w-4xl'
        } rounded-md overflow-hidden border-2 transition-all duration-300 ease-out ${
          boxed
            ? 'border-black bg-white shadow-brutal-sm'
            : // Collapsed: pull left by the 2px transparent border so the
              // `Worked Xs ›` line sits flush with the answer bubble's edge.
              'border-transparent bg-transparent shadow-none -ml-0.5'
        }`}
      >
        <button
          type="button"
          onClick={() => {
            // Live and settling blocks are non-toggleable: a live turn owns its
            // expansion, and a settling block stays open until the turn ends.
            if (!active && !settling) setExpanded((e) => !e);
          }}
          className={`w-full flex items-center gap-2 py-2 font-mono text-xs text-left border-b-2 transition-all duration-300 ease-out ${
            // Drop the horizontal padding when collapsed so the summary
            // aligns to the bubble's left edge, not its (indented) text.
            boxed ? 'px-3' : 'px-0'
          } ${
            // The header divider + tint only read once the steps panel is
            // open; the compact spinner card and the collapsed summary keep
            // a seamless, divider-less header.
            panelOpen ? 'border-black bg-canvas' : 'border-transparent bg-transparent'
          } ${active || settling ? 'cursor-default' : 'cursor-pointer'}`}
        >
          {active ? (
            <>
              <RiLoader4Line className="text-sm text-brand animate-spin shrink-0" />
              <span className="font-bold uppercase tracking-wider text-ink">Working</span>
              {row.workStartedAt ? (
                <span className="text-ink-soft tabular-nums">
                  <LiveElapsed startedAt={row.workStartedAt} />
                </span>
              ) : null}
            </>
          ) : (
            <>
              <span className={cancelled ? 'text-error' : 'text-ink-soft'}>{workedLabel}</span>
              <RiArrowRightSLine
                className={`text-sm text-ink-soft shrink-0 transition-transform duration-300 ease-out ${
                  panelOpen ? 'rotate-90' : ''
                }`}
              />
            </>
          )}
        </button>
        <div
          className={`grid transition-[grid-template-rows] duration-300 ease-out ${
            panelOpen ? 'grid-rows-[1fr]' : 'grid-rows-[0fr]'
          }`}
        >
          <div
            ref={stepsContainerRef}
            onScroll={handleStepsScroll}
            className={`min-h-0 ${
              panelOpen
                ? 'chat-scroll max-h-[calc((100vh-12rem)*3/5)] overflow-y-auto'
                : 'overflow-hidden'
            }`}
          >
            <div className="flex flex-col gap-1.5 px-3 py-2">
              {steps.map((s) => (
                <WorkStepView key={s.key} step={s} />
              ))}
            </div>
          </div>
        </div>
      </div>
      {!active && !panelOpen ? (
        <div aria-hidden className="w-full border-t border-black/20" />
      ) : null}
    </div>
  );
}

// Live-ticking elapsed seconds for the active work header. Self-contained
// 1s interval so the rest of the transcript doesn't re-render on the tick.
function LiveElapsed({ startedAt }: { startedAt: number }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, []);
  const secs = Math.max(0, Math.floor((now - startedAt) / 1000));
  // Hold the counter back for the first second so a just-started turn reads
  // "Working", never "Working 0s".
  return secs < 1 ? null : <>{secs}s</>;
}

// True when the trailing closed `work` block at position `i` represents
// a turn that ended without producing the final assistant reply —
// a cancellation the session never got a notice for (agent-loop abort,
// gateway shutdown mid-turn; a user `/stop` leaves its own notice as the
// trailing row, so it never reaches this). Keyed on the server's
// `TurnState`: the indicator renders only once this connection has been
// told, definitively, that no turn is in flight — a turn the server says
// is still running renders as the live work block instead, and with no
// signal at all (`turn === null`, e.g. the Subscribe snapshot hasn't
// landed yet) we stay quiet rather than mis-label a working turn.
// Only the very last transcript row is considered: a mid-transcript work
// block followed by a user message is an answered-elsewhere ambiguity we
// don't flag.
function isCancelledWorkAt(
  transcript: TranscriptRow[],
  i: number,
  turn: SessionView['turn'],
): boolean {
  if (i !== transcript.length - 1) return false;
  if (turn === null || turn.active) return false;
  const row = transcript[i];
  return row.kind === 'work' && !row.workActive;
}

function CancelledTurnIndicator() {
  return (
    <div className="flex">
      <span
        className="inline-flex items-center gap-1 px-2 py-0.5 border-2 border-warn/40 bg-warn/10 text-warn rounded-md font-mono text-[0.7rem] font-bold uppercase tracking-wider"
        role="status"
        title="The turn stopped before a reply landed."
      >
        <RiCloseLine className="text-xs shrink-0" />
        Cancelled
      </span>
    </div>
  );
}

// The initial "working" affordance, shown between sending a turn and the
// agent's first output frame (`SessionView.awaitingReply`). Replaces the
// old typing-dots bubble; its header matches the active work block so the
// two read as one continuous element once steps start landing.
function WorkingIndicator() {
  return (
    <div className="group flex flex-col items-start">
      <div
        className="border-2 border-black rounded-md bg-white px-3 py-2 shadow-brutal-sm flex items-center gap-2 font-mono text-xs"
        aria-label="Assistant is working"
        role="status"
      >
        <RiLoader4Line className="text-sm text-brand animate-spin" />
        <span className="font-bold uppercase tracking-wider text-ink-soft">Working…</span>
      </div>
    </div>
  );
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const handle = useCallback(() => {
    void navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    });
  }, [text]);
  return (
    <button
      type="button"
      onClick={handle}
      className="opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity text-ink-soft hover:text-ink cursor-pointer"
      title={copied ? 'Copied' : 'Copy message'}
      aria-label="Copy message"
    >
      {copied ? <RiCheckLine className="text-xs" /> : <RiClipboardLine className="text-xs" />}
    </button>
  );
}

function WelcomeEmpty({
  slashCommands,
  onPick,
}: {
  slashCommands: { command: string; description: string }[];
  onPick: (value: string) => void;
}) {
  return (
    <div className="max-w-3xl mx-auto pt-12 pb-6 flex flex-col gap-4">
      <div className="border-2 border-black bg-white rounded-md shadow-brutal-sm p-4 flex flex-col gap-2">
        <span className="text-2xl font-bold uppercase -tracking-[0.04em]">Start chatting</span>
        <p className="text-sm font-mono text-ink-soft">
          Send a message below. Shift+Enter inserts a newline; <code>/</code> opens the
          command palette.
        </p>
      </div>
      {slashCommands.length > 0 ? (
        <div className="border-2 border-black bg-white rounded-md p-3 flex flex-col gap-2">
          <span className="text-[0.7rem] font-bold uppercase tracking-wider text-ink-soft">
            Slash commands
          </span>
          <ul className="flex flex-col gap-1">
            {slashCommands.map((s) => (
              <li key={s.command}>
                <button
                  type="button"
                  onClick={() => onPick(`/${s.command} `)}
                  className="w-full text-left px-2 py-1.5 hover:bg-gray-100 rounded font-mono text-sm flex items-center gap-3 cursor-pointer"
                >
                  <span className="font-bold shrink-0">/{s.command}</span>
                  <span className="text-ink-soft truncate">{s.description}</span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

function ApprovalCard({
  approval,
  onDecide,
  connected,
}: {
  approval: PendingApproval;
  onDecide: (decision: ApprovalDecision) => void;
  connected: boolean;
}) {
  // Local `submitting` guard. The optimistic-dismiss in the parent
  // unmounts this card on the first click, so submitting state never
  // needs to survive remount — but until React paints that update,
  // a synchronous double-click in the same tick would see the same
  // captured `onDecide` closure and fire twice. The ref check below
  // wins synchronously; React state is only there to update the
  // visual disabled state for the brief window before unmount.
  const [submitting, setSubmitting] = useState(false);
  const submittingRef = useRef(false);
  const buttonsDisabled = submitting || !connected;
  const decide = (decision: ApprovalDecision) => {
    if (submittingRef.current) return;
    if (!connected) return;
    submittingRef.current = true;
    setSubmitting(true);
    onDecide(decision);
  };
  return (
    <div className="border-2 border-black bg-white rounded-md shadow-brutal-sm p-3 flex flex-col gap-2">
      <div className="flex items-baseline justify-between gap-2">
        <span className="font-bold uppercase tracking-wider text-sm">
          Approval needed: {approval.tool}
        </span>
        <span className="text-ink-soft font-mono text-xs">{approval.callId.slice(0, 8)}</span>
      </div>
      {approval.description ? (
        <div className="text-sm font-mono text-ink-soft">{approval.description}</div>
      ) : null}
      <ul className="text-sm font-mono flex flex-col gap-0.5">
        {approval.accesses.map((acc, i) => (
          <li key={i} className="text-ink-soft">
            • {formatAccess(acc)}
          </li>
        ))}
      </ul>
      <details className="text-xs font-mono">
        <summary className="cursor-pointer text-ink-soft hover:text-ink">
          parameters
        </summary>
        <pre className="mt-1 p-2 bg-canvas border-2 border-black rounded text-[11px] overflow-auto">
          {approval.paramsPreview}
        </pre>
      </details>
      {!connected ? (
        <div className="text-[0.7rem] font-mono uppercase tracking-wider text-warn">
          Waiting for connection — buttons disabled until the WS is back.
        </div>
      ) : null}
      <div className="flex gap-2 flex-wrap pt-1">
        <button
          type="button"
          onClick={() => decide('approve')}
          disabled={buttonsDisabled}
          className="px-3 py-1.5 bg-ok text-white border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:opacity-90 active:translate-x-[2px] active:translate-y-[2px] active:shadow-none cursor-pointer flex items-center gap-1 disabled:opacity-50 disabled:cursor-not-allowed disabled:active:translate-x-0 disabled:active:translate-y-0 disabled:active:shadow-brutal-sm"
        >
          <RiCheckLine /> Approve
        </button>
        <button
          type="button"
          onClick={() => decide('approve_always')}
          disabled={buttonsDisabled}
          className="px-3 py-1.5 bg-white text-ink border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:bg-gray-100 active:translate-x-[2px] active:translate-y-[2px] active:shadow-none cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed disabled:active:translate-x-0 disabled:active:translate-y-0 disabled:active:shadow-brutal-sm"
        >
          Approve always
        </button>
        <button
          type="button"
          onClick={() => decide('deny')}
          disabled={buttonsDisabled}
          className="px-3 py-1.5 bg-err text-white border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:opacity-90 active:translate-x-[2px] active:translate-y-[2px] active:shadow-none cursor-pointer flex items-center gap-1 disabled:opacity-50 disabled:cursor-not-allowed disabled:active:translate-x-0 disabled:active:translate-y-0 disabled:active:shadow-brutal-sm"
        >
          <RiCloseLine /> Deny
        </button>
      </div>
    </div>
  );
}

const CONNECTION_BADGE_COLOR: Record<ConnectionStatus['state'], string> = {
  connected: 'bg-ok',
  connecting: 'bg-warn',
  disconnected: 'bg-err',
};

function connectionBadgeLabel(status: ConnectionStatus): string {
  switch (status.state) {
    case 'connected':
      return 'connected';
    case 'connecting':
      return 'connecting…';
    case 'disconnected':
      return `reconnecting in ${Math.round(status.retryInMs / 1000)}s`;
  }
}

/** Composer-footer dropdown for switching the active conversation's
 *  model. `current` is the session's pin (`null` ⇒ following
 *  `default-llm`); selecting an entry (or "Default") calls `onSelect`,
 *  which PUTs the change. Opens upward since it sits at the bottom of
 *  the viewport. Rendered only when more than one model is configured. */
function ModelPicker({
  models,
  defaultName,
  current,
  onSelect,
}: {
  models: ModelOption[];
  defaultName: string;
  current: string | null | undefined;
  onSelect: (name: string | null) => void | Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const rootRef = useRef<HTMLDivElement | null>(null);

  // Dismiss on outside click or Escape while the menu is open.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [open]);

  const pinned = current ?? null;
  const label = pinned ?? (defaultName ? `Default · ${defaultName}` : 'Default');

  const pick = async (name: string | null) => {
    setOpen(false);
    // Re-selecting the active pin is a no-op — skip the round-trip.
    if ((name ?? null) === pinned) return;
    setBusy(true);
    try {
      await onSelect(name);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div ref={rootRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        disabled={busy}
        className="flex items-center gap-1.5 px-2 py-1 bg-white border-2 border-black rounded-md shadow-brutal-xs font-mono text-xs hover:bg-gray-100 active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer max-w-[240px]"
        title="Switch the model for this conversation"
      >
        {busy ? <RiLoader4Line className="text-sm animate-spin shrink-0" /> : null}
        <span className="text-ink-soft uppercase tracking-wider text-[0.6rem] shrink-0">
          model
        </span>
        <span className="font-bold truncate">{label}</span>
        <RiArrowDownSLine className="text-sm shrink-0" />
      </button>
      {open ? (
        <div className="absolute left-0 bottom-full mb-1 z-20 w-[260px] max-h-[60vh] overflow-auto bg-white border-2 border-black rounded-md shadow-brutal py-1">
          <ModelPickerRow
            label={defaultName ? `Default · ${defaultName}` : 'Default (default-llm)'}
            sublabel="follow the global default"
            selected={pinned === null}
            onClick={() => void pick(null)}
          />
          <div className="my-1 border-t-2 border-black/10" />
          {models.map((m) => (
            <ModelPickerRow
              key={m.name}
              label={m.name}
              sublabel={`${m.provider} · ${m.model}`}
              selected={pinned === m.name}
              onClick={() => void pick(m.name)}
            />
          ))}
        </div>
      ) : null}
    </div>
  );
}

function ModelPickerRow({
  label,
  sublabel,
  selected,
  onClick,
}: {
  label: string;
  sublabel: string;
  selected: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="w-full text-left px-3 py-1.5 flex items-center gap-2 hover:bg-canvas cursor-pointer"
    >
      <RiCheckLine className={`text-sm shrink-0 ${selected ? 'text-brand' : 'invisible'}`} />
      <span className="min-w-0">
        <span className="block font-mono text-xs font-bold truncate">{label}</span>
        <span className="block font-mono text-[0.65rem] text-ink-soft truncate">{sublabel}</span>
      </span>
    </button>
  );
}

function ConnectionBadge({ status }: { status: ConnectionStatus }) {
  return (
    <span className="flex items-center gap-1.5 text-xs font-mono uppercase tracking-wider text-ink-soft">
      <span className={`w-2 h-2 rounded-full ${CONNECTION_BADGE_COLOR[status.state]}`} />
      {connectionBadgeLabel(status)}
    </span>
  );
}

function formatAccess(acc: ResourceAccess): string {
  switch (acc.kind) {
    case 'read_file':
      return `read ${acc.path}`;
    case 'write_file':
      return `write ${acc.path}`;
    case 'http':
      return acc.host === '*' ? 'network access' : `reach ${acc.host}`;
    case 'exec_command':
      return `run: ${acc.command}`;
    case 'env':
      return (acc.vars?.length ?? 0) === 0
        ? 'read environment'
        : `read env: ${acc.vars?.join(', ') ?? ''}`;
    default:
      return `${(acc as { kind: string }).kind}`;
  }
}
