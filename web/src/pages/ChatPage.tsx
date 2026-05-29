import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type KeyboardEvent,
} from 'react';
import { Link, useNavigate, useParams } from 'react-router-dom';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import {
  RiAddLine,
  RiArrowDownLine,
  RiArrowDownSLine,
  RiArrowLeftLine,
  RiArrowRightSLine,
  RiCheckLine,
  RiClipboardLine,
  RiCloseLine,
  RiDeleteBin6Line,
  RiLoader4Line,
  RiSendPlane2Line,
} from 'react-icons/ri';

import { useAdminClient, useAuth } from '../api/auth';
import {
  ChatWs,
  type ConnectionStatus,
  type Frame,
  type ResourceAccess,
  type SessionPatch,
} from '../api/chatWs';
import { CronInbox } from '../components/CronInbox';

/** One progress entry inside a turn's work block. `reasoning`, `status`
 *  and `prose` carry `text`; `tool` carries the tool-call fields and is
 *  keyed by `toolCallId` so the completion frame resolves the step its
 *  start created. `prose` is mid-turn answer text the model emitted
 *  before its final reply — folded in here rather than left as its own
 *  bubble. */
interface WorkStep {
  key: string;
  kind: 'reasoning' | 'tool' | 'status' | 'prose';
  text?: string;
  toolCallId?: string;
  tool?: string;
  toolLabel?: string | null;
  toolStatus?: 'running' | 'ok' | 'error' | 'denied';
  toolSummary?: string;
}

interface TranscriptRow {
  /** Stable key for React. Synthetic; not part of any server schema. */
  key: string;
  role: 'user' | 'assistant' | 'system';
  text: string;
  /** Streaming text appended via Frame::AnswerDelta until the final
   *  Frame::Message arrives. */
  streaming?: boolean;
  notice?: { level: 'info' | 'warn' | 'error'; text: string };
  hasAttachments?: boolean;
  /** True while a user-authored row is on screen optimistically,
   *  waiting for the server's UserEcho. Cleared when the echo arrives
   *  carrying the same `clientMsgId` in its `platform_msg_id`. */
  pending?: boolean;
  /** Client-generated UUID for outbound user rows; doubles as both
   *  the WS frame's `platform_msg_id` (idempotency key against the
   *  gateway's InboundDedup) and the reconciliation key the inbound
   *  echo matches against. Unset on rows that didn't originate from
   *  this tab's composer. */
  clientMsgId?: string;
  /** ISO timestamp the bubble renders next to the message. For
   *  REST-loaded history rows this is the persisted
   *  `session_messages.created_at`; for live WS frames (the wire
   *  shape doesn't carry it) it's the receive time, which is close
   *  enough for genuine live emissions and drifts only on catch-up
   *  replays — those rows are also reachable via the REST history
   *  surface with the real value once the page refetches. */
  createdAt?: string;
  /** Set on the single live-only "work" row that aggregates a turn's
   *  intermediate progress — reasoning, tool calls, status, and mid-turn
   *  prose — into one collapsible block. Live-only: never persisted, so a
   *  REST history reload drops it. See `docs/turn-progress-events.md`. */
  kind?: 'work';
  /** Ordered progress steps inside a `kind === 'work'` row. */
  steps?: WorkStep[];
  /** True while the turn is still producing the block (animated
   *  "Working…" header, steps shown). Cleared when the turn's final
   *  message / notice lands, at which point the block collapses to a
   *  `Worked Xs ›` summary above the answer. */
  workActive?: boolean;
  /** Epoch ms when the block opened / closed — drives the live elapsed
   *  timer and the collapsed `Worked Xs` label. */
  workStartedAt?: number;
  workEndedAt?: number;
}

interface PendingApproval {
  callId: string;
  sessionId: string;
  tool: string;
  description: string | null;
  paramsPreview: string;
  accesses: ResourceAccess[];
  /** Wall-clock ms when this card was set. Used by the
   *  `pending_approvals_snapshot` reconciliation to tell apart "stale
   *  card from before reconnect" (older than the last reconnect, eligible
   *  for drop if absent from the snapshot) from "live card that arrived
   *  in the race window between subscribe and snapshot" (newer than the
   *  last reconnect, must be preserved even if the snapshot's queue read
   *  didn't observe it yet). */
  receivedAt: number;
}

type ApprovalDecision = 'approve' | 'approve_always' | 'deny';

interface SessionSummary {
  session_id: string;
  created_at: string;
  last_active: string;
  /** Local-only unread counter. Server doesn't surface this — the
   *  sidebar derives it from incoming `Frame::SessionActivity`. Cleared
   *  on navigation to the session. Always 0 on the row the user is
   *  currently viewing because activity for foreground sessions
   *  doesn't bump. */
  unread: number;
  /** Preview text the sidebar row renders — the session's most-recent
   *  user-authored message, truncated server-side. `undefined` for
   *  brand-new sessions with no user turn yet, and for sessions whose
   *  preview fetch failed on the list call. Updated locally on send
   *  and on inbound `Frame::UserEcho` so the row reflects the latest
   *  prompt without a list refetch. */
  last_user_text?: string;
}

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

/**
 * State for one session in the tab's view. The tab keeps one of these
 * per session it has visited — switching sessions doesn't drop the
 * prior session's transcript, and a streaming Delta arriving for a
 * background session reaches the right bucket without racing the
 * active view.
 */
interface SessionView {
  transcript: TranscriptRow[];
  pendingApproval: PendingApproval | null;
  historyLoaded: boolean;
  historyLoading: boolean;
  /** True while a scroll-up "load older" request is in flight; used
   *  to gate further triggers and surface a spinner above the list. */
  olderLoading: boolean;
  /** Lowest `ordinal` of any persisted row currently in `transcript`,
   *  or `null` when no persisted rows have loaded yet. Live Delta /
   *  Message rows arriving over the WS don't update it — they have
   *  no server ordinal until the agent persists, by which point the
   *  user already sees them. */
  oldestOrdinal: number | null;
  /** Mirrors the server's `has_more`: false once the slice reaches
   *  the session's first message and there's nothing left to page
   *  back to. */
  hasMore: boolean;
  /** True between `handleSend` and the assistant's first response
   *  (delta or message). Drives the left-aligned typing indicator
   *  bubble so the user gets immediate visual feedback that the agent
   *  is working, instead of staring at a frozen transcript. Cleared
   *  on first `Frame::AnswerDelta` (the streaming bubble itself takes over
   *  as the activity signal) or on a non-streaming assistant
   *  `Frame::Message`, and on Reset / session switch. */
  awaitingReply: boolean;
  /** Per-session LLM pin (`session.state.last_llm`) for the header
   *  model picker. `null` = follow `default-llm`; a string is the
   *  pinned `aura.json` entry name. Seeded from the GET-session
   *  detail's `last_llm` on history load and updated on a successful
   *  `PUT …/model`. */
  model?: string | null;
}

const EMPTY_VIEW: SessionView = {
  transcript: [],
  pendingApproval: null,
  historyLoaded: false,
  historyLoading: false,
  olderLoading: false,
  oldestOrdinal: null,
  hasMore: false,
  awaitingReply: false,
  model: null,
};

/** Soft cap on `views` map size. Past this, the oldest non-active
 *  bucket (by frame recency) is evicted: transcript + pendingApproval
 *  freed, WS subscription dropped, recency entry cleared. Revisit
 *  re-subscribes and re-fetches via REST. Tuned high enough that
 *  casual session-switching stays free; bites only when the user has
 *  genuinely roamed across many conversations in one tab session. */
const VIEW_CACHE_LIMIT = 20;

export function ChatPage() {
  const { sessionId } = useParams<{ sessionId?: string }>();
  const navigate = useNavigate();
  const client = useAdminClient();
  const { baseUrl } = useAuth();

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
  const anchorSessionIdRef = useRef<string | null>(null);

  // Per-session view buckets keyed by session_id. `currentView` is
  // the derived projection of the URL's sessionId.
  const [views, setViews] = useState<Record<string, SessionView>>({});
  const currentView = (sessionId && views[sessionId]) || EMPTY_VIEW;
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
  // the answer stream, so flush the buffer to its destination via
  // `writeStreamingAnswer`: into the open work block's trailing `prose`
  // step when a block is active (it stays put as an earlier mid-turn line
  // once the progress step lands after it), or into the standalone
  // streaming bubble otherwise — left `streaming: true` so
  // `routeInboundFrame`'s `ensureWork` can fold it into a fresh block. The
  // final `Message` path (which finalizes the answer) goes through
  // `cancelPacer` instead. No-op when no answer is mid-stream.
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
      ] = await Promise.all([
        client.GET('/v1/chat/sessions'),
        client.GET('/v1/chat/slash-manifest'),
        client.GET('/v1/llm/models'),
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
        last_user_text: s.last_user_text ?? undefined,
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
  }, [client]); // intentionally NOT depending on sessionId — bootstrap is one-shot

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
      onStatus: setStatus,
      onFrame: (frame) => {
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
          case 'notice':
          case 'approval_requested':
            recencyRef.current.set(frame.session_id, Date.now());
            break;
          default:
            break;
        }
        // Route delta frames through the pacer instead of straight to
        // setViews — the pacer's rAF loop owns the bubble's text while
        // streaming is in flight. routeInboundFrame's delta case stays
        // as a defensive fallback but should not fire from this path.
        if (frame.kind === 'answer_delta') {
          enqueueDelta(frame.session_id, frame.text);
          return;
        }
        // Progress frames (reasoning / tool lifecycle / status) interrupt
        // the answer stream. Settle the paced answer bubble first — its
        // buffered text was mid-turn prose, so `routeInboundFrame` folds
        // it into the turn's work block ahead of this progress step.
        if (
          frame.kind === 'reasoning' ||
          frame.kind === 'tool_started' ||
          frame.kind === 'tool_completed' ||
          frame.kind === 'status'
        ) {
          flushPacerKeepStreaming(frame.session_id);
        }
        // An assistant message frame is the authoritative final text
        // for the stream — drop any pacer state so its in-flight rAF
        // doesn't overwrite the finalized bubble a tick later.
        if (frame.kind === 'message' && frame.role !== 'user') {
          cancelPacer(frame.session_id);
        }
        routeInboundFrame(frame, setViews, setSessions, lastConnectedAtRef.current);
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
  }, [baseUrl, channelToken, releaseSessionView, enqueueDelta, cancelPacer, flushPacerKeepStreaming]);

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
          const oldestOrdinal = data.transcript.length > 0
            ? data.transcript[0].ordinal
            : null;
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
          if (data.transcript.length > 0) {
            const newest = data.transcript[data.transcript.length - 1].ordinal;
            wsRef.current?.recordOrdinal(sessionId, newest);
          }
          setViews((prev) =>
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
  }, [currentView.transcript, currentView.pendingApproval, currentView.awaitingReply]);

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
      const newOldest = data.transcript.length > 0
        ? data.transcript[0].ordinal
        : view.oldestOrdinal;
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

  const handleSend = useCallback(
    (e: FormEvent) => {
      e.preventDefault();
      const trimmed = composer.trim();
      if (!trimmed || !sessionId || !wsRef.current) return;
      if (status.state !== 'connected') return;
      // Same UUID flows two ways: as the WS frame's `platform_msg_id`
      // (server-side dedup key — a retry between send and echo doesn't
      // produce a second agent turn) AND as the optimistic row's
      // `clientMsgId` (reconciliation key — the inbound echo carrying
      // that same id replaces this row in place rather than appending
      // a duplicate). Generated once here so both sides agree.
      const clientMsgId = crypto.randomUUID();
      setViews((prev) => {
        const view = prev[sessionId] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sessionId]: {
            ...view,
            transcript: [
              ...closeActiveWork(view.transcript),
              {
                key: `pending-${clientMsgId}`,
                role: 'user',
                text: trimmed,
                pending: true,
                clientMsgId,
                createdAt: new Date().toISOString(),
              },
            ],
            awaitingReply: true,
          },
        };
      });
      // Update the sidebar row's preview optimistically — the server
      // will repeat the truth in the UserEcho frame (which also flows
      // into setSessions via applySessionUserText), but reflecting it
      // here keeps the sidebar in lockstep with the bubble the user
      // just dropped.
      setSessions((prev) => applySessionUserText(prev, sessionId, trimmed));
      // The user just hit send — anchor them to the tail regardless of
      // where they had scrolled to, so the optimistic row + incoming
      // reply land in view. The transcript-change effect above reads
      // this ref and jumps to scrollHeight on the next layout pass.
      pinnedToBottomRef.current = true;
      setHasNewBelow(false);
      wsRef.current.sendMessage({
        sessionId,
        userId: 'web-operator',
        content: trimmed,
        clientMsgId,
      });
      setComposer('');
      setShowSlashHints(false);
    },
    [composer, sessionId, status.state],
  );

  const handleComposerKey = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        if (composer.trim().length > 0) {
          const form = e.currentTarget.form;
          form?.requestSubmit();
        }
      }
    },
    [composer],
  );

  const handleComposerChange = useCallback(
    (value: string) => {
      setComposer(value);
      setShowSlashHints(value.startsWith('/') && slashCommands.length > 0);
    },
    [slashCommands.length],
  );

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

  const handleHideSession = useCallback(
    async (id: string) => {
      if (
        !window.confirm(
          'Hide this conversation from your list? It stays on the server — only your view is filtered.',
        )
      ) {
        return;
      }
      const { error } = await client.DELETE('/v1/chat/sessions/{session_id}', {
        params: { path: { session_id: id } },
      });
      if (error) {
        // Surface server-side failure (404, etc.) without nuking the
        // sidebar. The hide is server-authoritative; if the call
        // fails the row stays visible.
        console.warn('hide session failed:', error);
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
    },
    [client, releaseSessionView, sessionId, sessions],
  );

  // Re-pin the active session's model. The PUT is authoritative — its
  // `last_llm` echo drives the local update, and a live actor (if any)
  // is re-pinned server-side to take effect on the session's next turn.
  // `name === null` clears the pin back to `default-llm`. A concise
  // system notice records the switch inline so the transcript shows
  // which turns ran on which model.
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
          [sessionId]: {
            ...view,
            model: applied,
            transcript: [
              ...view.transcript,
              {
                key: `model-set-${Date.now()}`,
                role: 'system',
                text: '',
                notice: {
                  level: 'info',
                  text: applied
                    ? `Model set to ${applied}. Applies from your next message.`
                    : `Model reset to default${
                        defaultModelName ? ` (${defaultModelName})` : ''
                      }. Applies from your next message.`,
                },
              },
            ],
          },
        };
      });
    },
    [client, sessionId, defaultModelName],
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

  return (
    <div className="flex h-screen bg-canvas">
      {/* Session list rail */}
      <aside className="w-[260px] border-r-2 border-black flex flex-col bg-white shrink-0">
        <div className="px-4 py-4 border-b-2 border-black flex items-center justify-between">
          <Link
            to="/logs"
            className="flex items-center gap-1.5 text-ink-soft hover:text-ink text-[0.85rem] font-bold uppercase tracking-wider"
            title="Back to admin"
          >
            <RiArrowLeftLine className="text-base" />
            Admin
          </Link>
          <span className="text-xl font-bold uppercase -tracking-[0.05em]">CHAT</span>
        </div>
        <div className="px-3 py-3 border-b-2 border-black">
          <button
            type="button"
            onClick={handleNewChat}
            disabled={creating}
            className="w-full flex items-center justify-center gap-2 px-3 py-2 bg-brand text-white border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.85rem] hover:bg-brand-hover active:translate-x-[2px] active:translate-y-[2px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
          >
            <RiAddLine className="text-lg" />
            New chat
          </button>
        </div>
        <nav className="flex-1 overflow-auto px-2 py-2 flex flex-col gap-1">
          {sessionsLoading ? (
            <div className="flex justify-center py-6 text-ink-soft">
              <RiLoader4Line className="text-2xl animate-spin" />
            </div>
          ) : sessions.length === 0 ? (
            <div className="text-center text-ink-soft text-sm py-6 font-mono">
              No conversations yet.
            </div>
          ) : (
            sessions.map((s) => (
              <SessionRow
                key={s.session_id}
                session={s}
                active={s.session_id === sessionId}
                hasPending={Boolean(views[s.session_id]?.pendingApproval)}
                unreadCount={s.unread}
                onHide={handleHideSession}
              />
            ))
          )}
        </nav>
      </aside>

      {/* Main column */}
      <main className="flex-1 flex flex-col overflow-hidden relative">
        <CronInbox refreshSignal={cronInboxRefresh} />
        <header className="h-12 px-4 border-b-2 border-black flex items-center justify-between gap-3 bg-white">
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
            {sessionId && models.length > 1 ? (
              <ModelPicker
                models={models}
                defaultName={defaultModelName}
                current={currentView.model}
                onSelect={handleSelectModel}
              />
            ) : null}
            <ConnectionBadge status={status} />
          </div>
        </header>

        <div className="flex-1 flex flex-col overflow-hidden relative xl:pr-[260px]">
        <div className="flex-1 flex justify-center min-h-0 relative">
        <div
          ref={transcriptScrollRef}
          onScroll={handleTranscriptScroll}
          className="chat-scroll relative w-full max-w-6xl overflow-y-auto overflow-x-hidden px-6 py-4"
        >
          {currentView.historyLoading ? (
            <div className="flex justify-center py-12 text-ink-soft">
              <RiLoader4Line className="text-3xl animate-spin" />
            </div>
          ) : currentView.transcript.length === 0 && !currentView.pendingApproval ? (
            <WelcomeEmpty slashCommands={slashCommands} onPick={handleComposerChange} />
          ) : (
            <div className="flex flex-col gap-3">
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
                  <MessageBubble key={row.key} row={row} />,
                ];
                if (
                  isCancelledWorkAt(
                    arr,
                    i,
                    currentView.awaitingReply,
                    currentView.pendingApproval,
                  )
                ) {
                  nodes.push(
                    <CancelledTurnIndicator key={`${row.key}-cancelled`} />,
                  );
                }
                return nodes;
              })}
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
            className="absolute bottom-[calc(18vh+12px)] left-1/2 -translate-x-1/2 flex items-center gap-1.5 px-3 py-1.5 bg-white border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.75rem] hover:bg-gray-100 cursor-pointer"
            title="Jump to latest"
          >
            <RiArrowDownLine className="text-base" />
            New messages
          </button>
        ) : null}

        <div className="bg-canvas border-t-2 border-black max-w-6xl mx-auto w-full">
        <form
          onSubmit={handleSend}
          className="bg-canvas px-4 pt-3 pb-6 mb-[calc(14vh-131px)] max-w-3xl mx-auto w-full"
        >
          <div className="relative border-2 border-black rounded-md bg-white shadow-brutal-sm focus-within:shadow-brutal transition-shadow">
            {filteredSlash.length > 0 ? (
              <div className="border-b-2 border-black bg-canvas px-2 py-2 flex flex-col gap-0.5 rounded-t-[4px]">
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
                    className="text-left px-2 py-1.5 border-2 border-transparent hover:border-black hover:bg-white rounded font-mono text-sm flex items-center gap-2 cursor-pointer"
                  >
                    <span className="font-bold shrink-0">/{s.command}</span>
                    <span className="text-ink-soft truncate">{s.description}</span>
                  </button>
                ))}
              </div>
            ) : null}

            <textarea
              ref={composerRef}
              value={composer}
              onChange={(e) => handleComposerChange(e.target.value)}
              onKeyDown={handleComposerKey}
              placeholder={
                status.state === 'connected'
                  ? 'Message Aura…'
                  : 'Waiting for connection…'
              }
              rows={1}
              className="w-full px-3.5 py-3 font-mono text-sm bg-transparent resize-none focus:outline-none leading-relaxed placeholder:text-ink-soft/70"
            />

            <div className="flex items-center justify-between gap-2 px-2.5 py-1.5 border-t-2 border-black bg-canvas rounded-b-[4px]">
              <span className="hidden md:flex items-center gap-1 text-[0.6rem] font-mono text-ink-soft/80 min-w-0 flex-1">
                <kbd className="px-1.5 py-0.5 border border-black/40 rounded bg-white font-bold">
                  Enter
                </kbd>
                send
                <kbd className="ml-1 px-1.5 py-0.5 border border-black/40 rounded bg-white font-bold">
                  ⇧Enter
                </kbd>
                newline
                <kbd className="ml-1 px-1.5 py-0.5 border border-black/40 rounded bg-white font-bold">
                  /
                </kbd>
                commands
              </span>
              <button
                type="submit"
                disabled={!sessionId || composer.trim().length === 0 || status.state !== 'connected'}
                className="shrink-0 px-3 py-1.5 bg-brand text-white border-2 border-black rounded-md shadow-brutal-xs font-bold uppercase tracking-wider text-[0.7rem] hover:bg-brand-hover active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer flex items-center gap-1.5"
                title="Send (Enter)"
              >
                <RiSendPlane2Line className="text-sm" />
                Send
              </button>
            </div>
          </div>
        </form>
        </div>
        </div>
      </main>
    </div>
  );
}

// ── frame routing ───────────────────────────────────────────────────

/** Update the right per-session bucket based on a frame's session_id.
 *  Always operates on the views map via setViews so background
 *  sessions accumulate frames even when not currently viewed. Unread
 *  accounting lives elsewhere — `Frame::SessionActivity` is the single
 *  source of truth for sidebar badges, fired by the gateway's
 *  dispatch observer regardless of subscription state. */
function routeInboundFrame(
  frame: Frame,
  setViews: React.Dispatch<React.SetStateAction<Record<string, SessionView>>>,
  setSessions: React.Dispatch<React.SetStateAction<SessionSummary[]>>,
  lastConnectedAt: number,
): void {
  switch (frame.kind) {
    case 'answer_delta': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: appendStreamingDelta(view.transcript, frame.text),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'reasoning': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: appendReasoningStep(view.transcript, frame.text),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'tool_started': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: pushToolStartedStep(
              view.transcript,
              frame.call_id,
              frame.tool,
              frame.label ?? null,
            ),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'tool_completed': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: applyToolCompletedStep(
              view.transcript,
              frame.call_id,
              frame.status,
              frame.summary,
            ),
          },
        };
      });
      return;
    }
    case 'status': {
      const sid = frame.session_id;
      const text =
        frame.phase === 'compacting'
          ? 'Compacting context…'
          : frame.phase === 'compacted'
            ? 'Context compacted'
            : frame.phase;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: pushStatusStep(view.transcript, text),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'message': {
      const sid = frame.session_id;
      const role: 'user' | 'assistant' = frame.role === 'user' ? 'user' : 'assistant';
      // Any assistant message — replay or live, with or without prior
      // streaming deltas — ends the "awaiting reply" window AND closes the
      // turn's work block (collapsing it to its `Worked Xs` summary). Both
      // are done here as their own setViews rather than threading them
      // through every replay-merge branch below. The close must fire on
      // the replay path too: the gateway stamps the persisted `ordinal`
      // onto the LIVE final reply (see `OutgoingMessage::ordinal`), so it
      // routes through the ordinal branch, not just the live fall-through.
      // Turns are sequential per session, so the open block is always the
      // one this reply ends.
      if (role === 'assistant') {
        setViews((prev) => {
          const view = prev[sid];
          if (!view) return prev;
          const transcript = closeWorkForFinalReply(view.transcript);
          if (transcript === view.transcript && !view.awaitingReply) return prev;
          return { ...prev, [sid]: { ...view, transcript, awaitingReply: false } };
        });
      }
      // Catch-up replay (ordinal set): key by ordinal so React
      // reconciles against rows the REST history fetch already laid
      // down with the same shape, and a duplicate replay is a no-op.
      // Reconciles against locally-leftover rows from a WS drop in
      // the window between local emit and the live frame:
      // * a `streaming` assistant row (drop mid-Delta-stream) is
      //   swallowed by the replay's finalized Message;
      // * a `pending` user row (drop between handleSend and the
      //   live UserEcho) is matched by text;
      // * a *finalized* `msg-*` row (drop after the live frame was
      //   already rendered — this is the common case when only the
      //   assistant `Frame::Message` carries an ordinal and reconnect
      //   replays the user echo that landed during the previous
      //   session) is also matched by role+text within the recent
      //   tail. The gateway zeros `platform_msg_id` on replay (see
      //   `crates/gateway/src/channel/route.rs`), so text is the best
      //   discriminator we have client-side — sending the same text
      //   twice within the drop window would mis-match, but the
      //   failure mode (one duplicate row) is no worse than the
      //   pre-fix baseline. Without these paths the leftover row
      //   would sit alongside the replay forever.
      if (frame.ordinal !== undefined) {
        const replayKey = `hist-${sid}-${frame.ordinal}`;
        // Replays arrive ascending so iteratively overwriting the
        // sidebar preview converges on the freshest user turn — the
        // disconnect window may have hidden a sibling-tab send that
        // the bootstrap snapshot also missed.
        if (role === 'user') {
          const preview = frame.content.trim().length > 0
            ? frame.content
            : ((frame.attachments?.length ?? 0) > 0 ? '[attachment]' : '');
          if (preview) {
            setSessions((prev) => applySessionUserText(prev, sid, preview));
          }
        }
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          if (view.transcript.some((r) => r.key === replayKey)) return prev;
          if (role === 'assistant') {
            const lastIdx = view.transcript.length - 1;
            const last = view.transcript[lastIdx];
            if (last?.streaming && last.role === 'assistant') {
              const next = view.transcript.slice();
              // Preserve the streaming row's `createdAt` (stamped at
              // first Delta) — the persisted Message replay is the
              // same logical bubble, the user just saw it earlier.
              next[lastIdx] = {
                ...last,
                key: replayKey,
                role: 'assistant',
                text: frame.content,
                streaming: false,
              };
              return { ...prev, [sid]: { ...view, transcript: next } };
            }
          }
          if (role === 'user') {
            const matchIdx = view.transcript.findIndex(
              (r) => r.pending && r.role === 'user' && r.text === frame.content,
            );
            if (matchIdx >= 0) {
              const next = view.transcript.slice();
              next[matchIdx] = {
                ...view.transcript[matchIdx],
                key: replayKey,
                role: 'user',
                text: frame.content,
                pending: false,
              };
              return { ...prev, [sid]: { ...view, transcript: next } };
            }
          }
          // Finalized live row from a prior connection: scan the
          // tail for a non-keyed (`msg-*` / no `hist-` prefix), non-
          // streaming, non-pending row of the same role+text. Window
          // capped at the last 16 rows so we don't replay-walk a
          // 10k-message scrollback.
          //
          // Iterate oldest→newest within the window. Replays arrive
          // in ascending ordinal order, so the first un-claimed
          // matching row is the one this replay belongs to. The
          // newest-first walk we used before inverted the pairing
          // when the same text appeared twice: replay N would claim
          // the *later* row, then replay N+1 would claim the earlier
          // one, leaving the earlier text rendered with the newer
          // ordinal. Rows already re-keyed by a prior replay carry
          // the `hist-` prefix and are skipped, so iterating forward
          // can't re-claim them.
          //
          // `hasAttachments` is also part of the discriminator so an
          // attachment-only row doesn't get re-keyed onto a text-only
          // replay (and vice-versa) when their text happens to be
          // empty for the attachment side.
          const TAIL_WINDOW = 16;
          const start = Math.max(0, view.transcript.length - TAIL_WINDOW);
          const replayHasAttachments = (frame.attachments?.length ?? 0) > 0;
          for (let i = start; i < view.transcript.length; i++) {
            const row = view.transcript[i];
            if (row.streaming || row.pending) continue;
            if (row.key.startsWith('hist-')) continue;
            if (row.role !== role) continue;
            if (row.text !== frame.content) continue;
            if (Boolean(row.hasAttachments) !== replayHasAttachments) continue;
            const next = view.transcript.slice();
            next[i] = { ...row, key: replayKey, text: frame.content };
            return { ...prev, [sid]: { ...view, transcript: next } };
          }
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: [
                ...view.transcript,
                {
                  key: replayKey,
                  role,
                  text: frame.content,
                  createdAt: new Date().toISOString(),
                },
              ],
            },
          };
        });
        return;
      }
      // Live user echo. If this tab sent the message and is still
      // showing the optimistic placeholder, clear `pending` in place
      // (server text wins — sanitization may have rewritten it) and
      // keep the row's React key so the bubble doesn't unmount/
      // remount. Echoes without a matching placeholder (other tab,
      // pre-optimistic bundle, race after Reset wipe) fall through
      // to the normal append path. Decision is made inside the
      // updater because state setters are batched — checking outside
      // can't observe whether the updater found a match.
      const hasAttachments = (frame.attachments?.length ?? 0) > 0;
      // Sidebar preview tracks the freshest user-authored text, so
      // every live user echo (whether this tab sent it or a sibling
      // did) feeds the sidebar — including the attachment-only case,
      // where the placeholder string mirrors the bubble's "[attachment]"
      // fallback so the row doesn't go blank on a media-only send.
      if (role === 'user') {
        const preview = frame.content.trim().length > 0
          ? frame.content
          : (hasAttachments ? '[attachment]' : '');
        if (preview) {
          setSessions((prev) => applySessionUserText(prev, sid, preview));
        }
      }
      if (role === 'user' && frame.platform_msg_id) {
        const clientMsgId = frame.platform_msg_id;
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          const idx = view.transcript.findIndex(
            (r) => r.pending && r.clientMsgId === clientMsgId,
          );
          if (idx >= 0) {
            const next = view.transcript.slice();
            next[idx] = {
              ...view.transcript[idx],
              text: frame.content,
              pending: false,
              hasAttachments: hasAttachments || next[idx].hasAttachments,
            };
            return { ...prev, [sid]: { ...view, transcript: next } };
          }
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: finalizeMessage(view.transcript, role, frame.content, hasAttachments),
            },
          };
        });
        return;
      }
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: finalizeMessage(view.transcript, role, frame.content, hasAttachments),
          },
        };
      });
      return;
    }
    case 'notice': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        // A notice is terminal for the turn (slash-command reply,
        // refusal, compaction confirmation, …) — close any open work
        // block so it collapses above the notice instead of dangling.
        const base = closeActiveWork(view.transcript);
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: [
              ...base,
              {
                key: `notice-${sid}-${base.length}-${Date.now()}`,
                role: 'system',
                text: '',
                notice: { level: noticeLevel(frame.level), text: frame.text },
              },
            ],
            // Some turns reply with `AgentOutput::Notice` and never
            // emit a Delta/Message — slash commands like `/compact`,
            // refusal / error paths, etc. Without this, the working
            // indicator would hang forever for those sends. The notice
            // itself is now the reply, so awaitingReply ends here.
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'approval_requested': {
      const sid = frame.session_id;
      const receivedAt = Date.now();
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            pendingApproval: {
              callId: frame.call_id,
              sessionId: sid,
              tool: frame.tool,
              description: frame.description ?? null,
              paramsPreview: frame.params_preview,
              accesses: frame.accesses,
              receivedAt,
            },
            // Agent has stopped to ask the user something — it's no
            // longer composing. The approval card is the activity
            // signal now; suppress the typing dots so the two don't
            // stack and contradict each other.
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'pending_approvals_snapshot': {
      // Server's authoritative list of pending approval call_ids for
      // this session, sent once per Subscribe. Reconcile against our
      // local card: drop if it (a) pre-dates the most recent
      // reconnect — i.e. could plausibly be stale from before — AND
      // (b) is missing from the snapshot. Cards stamped after the
      // last reconnect came in through live broadcast and are
      // protected from the race where a fresh approval arrives in
      // the microsecond gap between the server's subscribe
      // registration and snapshot send.
      const sid = frame.session_id;
      const callIds = new Set(frame.call_ids);
      setViews((prev) => {
        const view = prev[sid];
        if (!view?.pendingApproval) return prev;
        const pa = view.pendingApproval;
        if (pa.receivedAt >= lastConnectedAt) return prev;
        if (callIds.has(pa.callId)) return prev;
        return { ...prev, [sid]: { ...view, pendingApproval: null } };
      });
      return;
    }
    case 'approval_resolved': {
      setViews((prev) => {
        // Walk every session bucket since we don't know which one
        // the call_id belongs to. Map is small (~tabs visited), so
        // this is cheap. Return `prev` unchanged when no card matches
        // — the call_id may belong to an already-resolved session, and
        // a fresh object would force every SessionRow to re-render.
        let next: Record<string, SessionView> | null = null;
        for (const [sid, view] of Object.entries(prev)) {
          if (view.pendingApproval?.callId === frame.call_id) {
            next ??= { ...prev };
            next[sid] = { ...view, pendingApproval: null };
          }
        }
        return next ?? prev;
      });
      return;
    }
    default:
      // history_snapshot / start_bot / stop_bot / slash_manifest /
      // subscribe / unsubscribe / register / register_ack / reset are
      // not expected on the web client (the SDK strips most of them
      // before they reach onFrame; the rest are debug noise).
      return;
  }
}

function mergeView(
  prev: Record<string, SessionView>,
  sessionId: string,
  patch: Partial<SessionView>,
): Record<string, SessionView> {
  const view = prev[sessionId] ?? EMPTY_VIEW;
  return { ...prev, [sessionId]: { ...view, ...patch } };
}

// Write the pacer's cumulative visible answer slice into the right place.
// The second arg is the *full* visible text (not an increment), so each
// rAF tick replaces rather than appends.
//
// While a turn's work block is open, the answer streams **inside** it as
// its trailing `prose` step — that's what keeps mid-turn output (and the
// final answer as it types out) from popping out below the working bubble.
// When the terminal `Frame::Message` lands, `closeWorkForFinalReply` peels
// that prose step back off into the standalone answer bubble. With no open
// block (a direct answer, no reasoning/tools) it streams straight into a
// standalone bubble, finalized by the message path as today.
function writeStreamingAnswer(prev: TranscriptRow[], text: string): TranscriptRow[] {
  const last = prev[prev.length - 1];
  if (last?.kind === 'work' && last.workActive) {
    const steps = last.steps ?? [];
    const lastStep = steps[steps.length - 1];
    if (lastStep?.kind === 'prose') {
      if (lastStep.text === text) return prev;
      const next = prev.slice();
      next[next.length - 1] = {
        ...last,
        steps: [...steps.slice(0, -1), { ...lastStep, text }],
      };
      return next;
    }
    const next = prev.slice();
    next[next.length - 1] = {
      ...last,
      steps: [...steps, { key: `prose-${steps.length}-${Date.now()}`, kind: 'prose', text }],
    };
    return next;
  }
  if (last?.streaming && last.role === 'assistant') {
    if (last.text === text) return prev;
    return [...prev.slice(0, -1), { ...last, text }];
  }
  return [
    ...prev,
    {
      key: `stream-${prev.length}-${Date.now()}`,
      role: 'assistant',
      text,
      streaming: true,
      createdAt: new Date().toISOString(),
    },
  ];
}

function appendStreamingDelta(prev: TranscriptRow[], text: string): TranscriptRow[] {
  const last = prev[prev.length - 1];
  if (last?.streaming && last.role === 'assistant') {
    return [...prev.slice(0, -1), { ...last, text: last.text + text }];
  }
  // Stamp `createdAt` at the moment the assistant starts streaming —
  // not when the final Message lands — so the timestamp the user sees
  // matches when the bubble actually appeared in their view rather
  // than the persistence-time clock skew of "Message arrives a few
  // hundred ms after the last delta".
  return [
    ...prev,
    {
      key: `stream-${prev.length}-${Date.now()}`,
      role: 'assistant',
      text,
      streaming: true,
      createdAt: new Date().toISOString(),
    },
  ];
}

/** Locate the turn's open work block — creating one at the tail if
 *  absent — and fold any trailing streaming answer bubble into it as a
 *  `prose` step first. Returns the next rows plus the block's index.
 *
 *  The fold is what makes "mid-turn prose lives in the work block" work:
 *  a progress frame interrupting the answer stream means the text
 *  streamed so far was intermediate, not the final reply, so it moves
 *  inside the block ahead of the step the caller is about to push. The
 *  final answer never reaches here — it's capped by `Frame::Message`,
 *  which finalizes its bubble through `finalizeMessage` instead. Callers
 *  own the returned `rows` (they slice before writing), so `prev` is
 *  never mutated. The work block is `role: 'system'` so it never collides
 *  with the assistant-streaming / replay reconciliation paths. */
function ensureWork(prev: TranscriptRow[]): { rows: TranscriptRow[]; idx: number } {
  let rows = prev;
  let proseStep: WorkStep | null = null;
  const last = rows[rows.length - 1];
  if (last && last.kind !== 'work' && last.role === 'assistant' && last.streaming) {
    if (last.text.trim().length > 0) {
      proseStep = { key: `prose-${last.key}`, kind: 'prose', text: last.text };
    }
    rows = rows.slice(0, -1);
  }
  const tail = rows[rows.length - 1];
  let idx: number;
  if (tail?.kind === 'work' && tail.workActive) {
    idx = rows.length - 1;
  } else {
    const now = Date.now();
    rows = [
      ...rows,
      {
        key: `work-${now}-${rows.length}`,
        role: 'system',
        text: '',
        kind: 'work',
        steps: [],
        workActive: true,
        workStartedAt: now,
      },
    ];
    idx = rows.length - 1;
  }
  if (proseStep) {
    const block = rows[idx];
    rows = rows.slice();
    rows[idx] = { ...block, steps: [...(block.steps ?? []), proseStep] };
  }
  return { rows, idx };
}

/** Append one step to the turn's open work block. */
function pushWorkStep(prev: TranscriptRow[], step: WorkStep): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev);
  const block = rows[idx];
  const next = rows.slice();
  next[idx] = { ...block, steps: [...(block.steps ?? []), step] };
  return next;
}

/** Append a reasoning chunk, merging into a trailing reasoning step so
 *  the streamed thinking reads as one paragraph. */
function appendReasoningStep(prev: TranscriptRow[], text: string): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev);
  const block = rows[idx];
  const steps = block.steps ?? [];
  const lastStep = steps[steps.length - 1];
  const next = rows.slice();
  if (lastStep?.kind === 'reasoning') {
    next[idx] = {
      ...block,
      steps: [...steps.slice(0, -1), { ...lastStep, text: (lastStep.text ?? '') + text }],
    };
  } else {
    next[idx] = {
      ...block,
      steps: [...steps, { key: `reason-${steps.length}-${Date.now()}`, kind: 'reasoning', text }],
    };
  }
  return next;
}

/** Push a running tool step, keyed by `callId`. Idempotent on a
 *  re-delivered start within the open block. */
function pushToolStartedStep(
  prev: TranscriptRow[],
  callId: string,
  tool: string,
  label: string | null,
): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev);
  const block = rows[idx];
  const steps = block.steps ?? [];
  if (steps.some((s) => s.kind === 'tool' && s.toolCallId === callId)) return rows;
  const next = rows.slice();
  next[idx] = {
    ...block,
    steps: [
      ...steps,
      { key: `tool-${callId}`, kind: 'tool', toolCallId: callId, tool, toolLabel: label, toolStatus: 'running' },
    ],
  };
  return next;
}

/** Resolve a tool step by `callId` with its final status + summary. If
 *  the start was never seen (dropped), synthesize a completed step so the
 *  result still shows. */
function applyToolCompletedStep(
  prev: TranscriptRow[],
  callId: string,
  status: string,
  summary: string,
): TranscriptRow[] {
  const toolStatus: WorkStep['toolStatus'] =
    status === 'error' ? 'error' : status === 'denied' ? 'denied' : 'ok';
  for (let i = prev.length - 1; i >= 0; i--) {
    const row = prev[i];
    if (row.kind !== 'work') continue;
    const steps = row.steps ?? [];
    const sIdx = steps.findIndex((s) => s.kind === 'tool' && s.toolCallId === callId);
    if (sIdx < 0) continue;
    const nextSteps = steps.slice();
    nextSteps[sIdx] = { ...nextSteps[sIdx], toolStatus, toolSummary: summary };
    const next = prev.slice();
    next[i] = { ...row, steps: nextSteps };
    return next;
  }
  return pushWorkStep(prev, {
    key: `tool-${callId}`,
    kind: 'tool',
    toolCallId: callId,
    tool: 'tool',
    toolStatus,
    toolSummary: summary,
  });
}

/** Push a status step (compaction, …) into the turn's open work block. */
function pushStatusStep(prev: TranscriptRow[], text: string): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev);
  const block = rows[idx];
  const steps = block.steps ?? [];
  const next = rows.slice();
  next[idx] = {
    ...block,
    steps: [...steps, { key: `status-${steps.length}-${Date.now()}`, kind: 'status', text }],
  };
  return next;
}

/** Close the turn's open work block: stamp `workEndedAt` and clear
 *  `workActive` so it collapses to a `Worked Xs ›` summary. An empty
 *  block (the turn produced no intermediate steps — a direct answer) is
 *  dropped entirely so no summary line / arrow appears. */
function closeActiveWork(prev: TranscriptRow[]): TranscriptRow[] {
  for (let i = prev.length - 1; i >= 0; i--) {
    const row = prev[i];
    if (row.kind !== 'work' || !row.workActive) continue;
    if (!row.steps || row.steps.length === 0) {
      return [...prev.slice(0, i), ...prev.slice(i + 1)];
    }
    const next = prev.slice();
    next[i] = { ...row, workActive: false, workEndedAt: Date.now() };
    return next;
  }
  return prev;
}

/** Close the open work block at the turn's terminal reply, peeling off a
 *  trailing `prose` step — that's the final answer the model streamed
 *  into the block — so the caller renders it as the standalone answer
 *  bubble below (from the authoritative `Frame::Message` content, so the
 *  peeled step's own text is just discarded). If peeling empties the
 *  block (a direct answer that only opened a block to stream into, e.g.
 *  reasoning-less), the block is dropped so no `Worked Xs` line shows. */
function closeWorkForFinalReply(prev: TranscriptRow[]): TranscriptRow[] {
  for (let i = prev.length - 1; i >= 0; i--) {
    const row = prev[i];
    if (row.kind !== 'work' || !row.workActive) continue;
    let steps = row.steps ?? [];
    if (steps.length > 0 && steps[steps.length - 1].kind === 'prose') {
      steps = steps.slice(0, -1);
    }
    if (steps.length === 0) {
      return [...prev.slice(0, i), ...prev.slice(i + 1)];
    }
    const next = prev.slice();
    next[i] = { ...row, steps, workActive: false, workEndedAt: Date.now() };
    return next;
  }
  return prev;
}

function finalizeMessage(
  prev: TranscriptRow[],
  role: 'user' | 'assistant',
  content: string,
  hasAttachments: boolean,
): TranscriptRow[] {
  const last = prev[prev.length - 1];
  if (role === 'assistant' && last?.streaming && last.role === 'assistant') {
    return [
      ...prev.slice(0, -1),
      {
        ...last,
        text: content,
        streaming: false,
        // Live attachments stay observable on the streaming row — the
        // bubble's `row.text || [attachment]` fallback then renders
        // correctly even when the assistant produced only media.
        hasAttachments: hasAttachments || last.hasAttachments,
      },
    ];
  }
  return [
    ...prev,
    {
      key: `msg-${prev.length}-${Date.now()}`,
      role,
      text: content,
      hasAttachments: hasAttachments || undefined,
      createdAt: new Date().toISOString(),
    },
  ];
}

function noticeLevel(level: string): 'info' | 'warn' | 'error' {
  if (level === 'error') return 'error';
  if (level === 'info') return 'info';
  return 'warn';
}

function roleFromString(role: string): 'user' | 'assistant' | 'system' {
  if (role === 'user') return 'user';
  if (role === 'system') return 'system';
  return 'assistant';
}

interface HistoryWorkStepDto {
  kind: 'reasoning' | 'prose' | 'tool';
  text?: string;
  tool?: string | null;
  tool_label?: string | null;
  tool_status?: string | null;
  tool_summary?: string | null;
}

interface HistoryRowDto {
  created_at: string;
  ordinal: number;
  /** `'message'` (default) or `'work'` — see the gateway's
   *  `ChatTranscriptItem`. A `work` row is the server's reconstruction of
   *  a tool-using turn's collapsed work block. */
  kind?: 'message' | 'work';
  role: string;
  text: string;
  has_attachments: boolean;
  steps?: HistoryWorkStepDto[];
  work_started_at?: string | null;
  work_ended_at?: string | null;
}

/** Translate one server-side transcript row into the local
 *  [`TranscriptRow`] shape, keying on the absolute ordinal so the
 *  same logical message coming back from another page-fetch (or a
 *  hot-reload during dev) reuses the same React node identity. A
 *  `work` row maps to a finished (collapsed) work block, rendered by
 *  the same `WorkBlock` the live path produces. */
function historyRowToTranscript(sessionId: string, row: HistoryRowDto): TranscriptRow {
  if (row.kind === 'work') {
    return {
      key: `hist-${sessionId}-${row.ordinal}`,
      role: 'system',
      text: '',
      kind: 'work',
      workActive: false,
      workStartedAt: row.work_started_at ? Date.parse(row.work_started_at) : undefined,
      workEndedAt: row.work_ended_at ? Date.parse(row.work_ended_at) : undefined,
      steps: (row.steps ?? []).map((s, i) => ({
        key: `hist-${sessionId}-${row.ordinal}-${i}`,
        kind: s.kind,
        text: s.text,
        tool: s.tool ?? undefined,
        toolLabel: s.tool_label ?? null,
        // Backend sends `ok` / `error` / `denied` (or null when the result
        // didn't land); `undefined` renders neutral, matching live.
        toolStatus: (s.tool_status ?? undefined) as WorkStep['toolStatus'],
        toolSummary: s.tool_summary ?? undefined,
      })),
    };
  }
  return {
    key: `hist-${sessionId}-${row.ordinal}`,
    role: roleFromString(row.role),
    text: row.text,
    hasAttachments: row.has_attachments,
    createdAt: row.created_at,
  };
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
 *  Sort order: keep "most-recent `last_active` first" so a session
 *  bumping its activity floats to the top without an explicit list
 *  refetch. Stable for rows whose `last_active` didn't change. */
function applySessionPatch(
  prev: SessionSummary[],
  sessionId: string,
  patch: SessionPatch,
): SessionSummary[] {
  if (patch.hidden === true) {
    return prev.filter((s) => s.session_id !== sessionId);
  }
  const idx = prev.findIndex((s) => s.session_id === sessionId);
  if (idx === -1) {
    if (patch.created_at == null || patch.last_active == null) return prev;
    return sortByLastActiveDesc([
      ...prev,
      {
        session_id: sessionId,
        created_at: patch.created_at,
        last_active: patch.last_active,
        unread: 0,
      },
    ]);
  }
  const current = prev[idx];
  const merged: SessionSummary = {
    session_id: current.session_id,
    created_at: patch.created_at ?? current.created_at,
    last_active: patch.last_active ?? current.last_active,
    unread: current.unread,
    last_user_text: current.last_user_text,
  };
  if (
    merged.created_at === current.created_at &&
    merged.last_active === current.last_active
  ) {
    return prev;
  }
  const next = prev.slice();
  next[idx] = merged;
  return sortByLastActiveDesc(next);
}

/** Merge a `SessionActivity` ping onto the sidebar list. Projects
 *  `at` onto the row's local `last_active` (so the age string and
 *  sort order both stay current without a list refetch) and bumps
 *  `unread` iff the activity isn't on the currently-foregrounded
 *  session. Activity for sessions we don't know about (raced ahead
 *  of Created, or hidden in this tab) is dropped on the floor —
 *  Created arrives separately, and rehydration after a hide isn't
 *  worth optimising. */
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
  return sortByLastActiveDesc(next);
}

function sortByLastActiveDesc(list: SessionSummary[]): SessionSummary[] {
  return list
    .slice()
    .sort((a, b) => Date.parse(b.last_active) - Date.parse(a.last_active));
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

/** Compact human-readable age. Same shape the logs page uses, kept
 *  local since the dep would be marginal. */
function relativeAge(iso: string, now: number = Date.now()): string {
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return '';
  const diffSec = Math.max(0, Math.floor((now - t) / 1000));
  if (diffSec < 5) return 'just now';
  if (diffSec < 60) return `${diffSec}s ago`;
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`;
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`;
  if (diffSec < 86400 * 7) return `${Math.floor(diffSec / 86400)}d ago`;
  return new Date(iso).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

// ── visual components ───────────────────────────────────────────────

function SessionRow({
  session,
  active,
  hasPending,
  unreadCount,
  onHide,
}: {
  session: SessionSummary;
  active: boolean;
  hasPending: boolean;
  unreadCount: number;
  onHide: (id: string) => void;
}) {
  // Unread badge only shows on background rows — the active row is
  // already cleared on entry, but guard anyway in case a frame races
  // the clearing effect.
  const showUnread = unreadCount > 0 && !active;
  return (
    <Link
      to={`/chat/${session.session_id}`}
      className={`group relative px-3 py-2 rounded-md border-2 ${
        active
          ? 'bg-brand text-white border-black'
          : 'border-transparent hover:bg-gray-100 text-ink'
      }`}
      title={session.session_id}
    >
      <div className="flex items-center gap-2">
        <span
          className={`text-sm font-bold flex-1 truncate ${
            active ? 'text-white' : 'text-ink'
          } ${session.last_user_text ? '' : 'italic opacity-70'}`}
          title={session.last_user_text ?? undefined}
        >
          {session.last_user_text ?? 'New conversation'}
        </span>
        {showUnread ? (
          <span
            className="shrink-0 min-w-[20px] h-5 px-1.5 rounded-full bg-brand text-white border-2 border-black font-mono text-[0.65rem] font-bold flex items-center justify-center leading-none"
            title={`${unreadCount} unread message${unreadCount === 1 ? '' : 's'}`}
          >
            {unreadCount > 99 ? '99+' : unreadCount}
          </span>
        ) : null}
        {hasPending ? (
          <span
            className={`w-2 h-2 rounded-full shrink-0 ${active ? 'bg-white' : 'bg-warn'}`}
            title="Approval pending"
          />
        ) : null}
        <button
          type="button"
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            onHide(session.session_id);
          }}
          className={`opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity p-0.5 rounded border-2 border-transparent hover:border-black shrink-0 ${
            active ? 'text-white hover:bg-brand-hover' : 'text-ink-soft hover:bg-white'
          }`}
          title="Hide from list (server-side row is kept)"
          aria-label="Hide conversation"
        >
          <RiDeleteBin6Line className="text-sm" />
        </button>
      </div>
      <div
        className={`mt-0.5 font-mono text-[0.65rem] ${
          active ? 'text-white/70' : 'text-ink-soft'
        }`}
      >
        {relativeAge(session.last_active)}
      </div>
    </Link>
  );
}

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
  ul: ({ children }) => (
    <ul className="my-2 first:mt-0 last:mb-0 list-disc pl-5 space-y-1">{children}</ul>
  ),
  ol: ({ children }) => (
    <ol className="my-2 first:mt-0 last:mb-0 list-decimal pl-5 space-y-1">{children}</ol>
  ),
  li: ({ children }) => <li className="leading-snug">{children}</li>,
  a: ({ href, children }) => (
    <a
      href={href}
      target="_blank"
      rel="noopener noreferrer"
      className="text-brand underline underline-offset-2 hover:text-brand-hover break-words"
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
          className="bg-canvas border border-black/30 rounded px-1 py-[1px] text-[0.85em]"
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
    <pre className="my-2 first:mt-0 last:mb-0 bg-canvas border-2 border-black rounded-md p-2 overflow-x-auto text-xs leading-snug">
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

function MessageBubble({ row }: { row: TranscriptRow }) {
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
      <div
        className={`border-2 rounded-md px-3 py-2 font-mono text-sm whitespace-pre-wrap ${palette}`}
      >
        {row.notice.text}
      </div>
    );
  }
  const isUser = row.role === 'user';
  const body = row.text || (row.hasAttachments ? '[attachment]' : '');
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
      <div className={`relative min-w-0 ${isUser ? 'max-w-2xl' : 'max-w-4xl'}`}>
        <div
          className={`border-2 border-black rounded-md px-3 py-2 text-sm transition-opacity break-words [overflow-wrap:anywhere] ${
            showMarkdown ? 'font-mono' : 'font-mono whitespace-pre-wrap'
          } ${isUser ? 'bg-brand text-white' : 'bg-white text-ink'} ${
            row.pending ? 'opacity-60' : ''
          }`}
        >
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
        {!isUser && !row.streaming && body ? <CopyButton text={body} /> : null}
      </div>
      {row.createdAt ? (
        <span
          className="mt-1 px-1 font-mono text-[0.65rem] text-ink-soft tabular-nums"
          title={formatTimestampTooltip(row.createdAt)}
        >
          {formatTimestampShort(row.createdAt)}
        </span>
      ) : null}
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
    return (
      <div className="font-mono text-xs text-ink-soft leading-relaxed">
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

// The turn's aggregated progress. While `workActive`, it's an expanded
// assistant-side bubble with an animated "Working" header + live elapsed
// timer and the steps revealed underneath. On completion it collapses to
// a dim `Worked Xs ›` line (click to re-expand the work) that sits above
// the final answer bubble. A turn that produced no steps is dropped on
// close (see `closeActiveWork`), so a collapsed block always has work to
// show and the arrow is always meaningful.
function WorkBlock({ row }: { row: TranscriptRow }) {
  const active = !!row.workActive;
  const steps = row.steps ?? [];
  // `expanded` is the user's explicit toggle for the *finished* block and
  // defaults closed; while the turn is still active the block is forced
  // open. Deriving `open` this way — rather than an effect that flips
  // `expanded` on completion — keeps the active→done collapse a single
  // render, so the body animates 1fr→0fr cleanly with no intermediate
  // flash.
  const [expanded, setExpanded] = useState(false);
  const open = active || expanded;

  // Pin the steps panel to its tail while the turn is producing so a long
  // tool loop reveals the newest reasoning/tool line at the bottom instead
  // of stranding the user at the top. Fires on each step list mutation —
  // new step, status update, prose append — and is layout-effect-scoped so
  // the catch-up happens pre-paint, no visible flash.
  const stepsContainerRef = useRef<HTMLDivElement | null>(null);
  useLayoutEffect(() => {
    if (!active) return;
    const el = stepsContainerRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [active, steps]);

  if (!active && steps.length === 0) return null;

  const secs =
    row.workEndedAt && row.workStartedAt
      ? Math.max(0, Math.round((row.workEndedAt - row.workStartedAt) / 1000))
      : 0;
  // A *collapsed* finished block sits directly above its turn's answer
  // bubble — pull the row gap in (the transcript's `gap-3`) so the
  // `Worked Xs ›` summary reads as attached to that answer. Only when
  // collapsed: while active there's nothing below it, and once expanded
  // the steps panel becomes its own box that needs the normal gap to the
  // answer. Transitioned so expand/collapse eases the gap rather than
  // snapping it.
  const tighten = open ? '' : '-mb-3';

  // One persistent element tree across active / collapsed / expanded so
  // the transitions actually animate (a branch swap would just hard-cut).
  // The chrome (border + bg + shadow) fades via `transition-all`; the
  // steps panel grows/shrinks via the grid-rows 0fr↔1fr trick — the
  // dependable way to animate to/from content height. Border *width*
  // stays 2px in every state (only its color fades) so nothing reflows.
  return (
    <div
      className={`group flex flex-col items-start w-full transition-[margin-bottom] duration-300 ease-out ${tighten}`}
    >
      <div
        className={`w-full max-w-4xl rounded-md overflow-hidden border-2 transition-all duration-300 ease-out ${
          open
            ? 'border-black bg-white shadow-brutal-sm'
            : // Collapsed: pull left by the 2px transparent border so the
              // `Worked Xs ›` line sits flush with the answer bubble's edge.
              'border-transparent bg-transparent shadow-none -ml-0.5'
        }`}
      >
        <button
          type="button"
          onClick={() => {
            if (!active) setExpanded((e) => !e);
          }}
          className={`w-full flex items-center gap-2 py-2 font-mono text-xs text-left border-b-2 transition-all duration-300 ease-out ${
            // Drop the horizontal padding when collapsed so the summary
            // aligns to the bubble's left edge, not its (indented) text.
            open ? 'px-3 border-black bg-canvas' : 'px-0 border-transparent bg-transparent'
          } ${active ? 'cursor-default' : 'cursor-pointer hover:bg-canvas'}`}
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
              <span className="text-ink-soft">Worked {secs}s</span>
              <RiArrowRightSLine
                className={`text-sm text-ink-soft shrink-0 transition-transform duration-300 ease-out ${
                  expanded ? 'rotate-90' : ''
                }`}
              />
            </>
          )}
        </button>
        <div
          className={`grid transition-[grid-template-rows] duration-300 ease-out ${
            open ? 'grid-rows-[1fr]' : 'grid-rows-[0fr]'
          }`}
        >
          <div
            ref={stepsContainerRef}
            className={`min-h-0 ${
              open
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
  return <>{secs}s</>;
}

// True when the trailing closed `work` block at position `i` represents
// a turn that ended without producing the final assistant reply —
// typically a cancellation (user stop, agent loop abort, gateway
// shutdown mid-turn). Only the very last transcript row is considered:
// a mid-transcript work block followed by a user message is ambiguous
// — it could be a real cancel, OR a race where the user typed during a
// long-running turn (e.g. a transient LLM SSE failure that retries for
// >1min) whose reply is still in flight. We can't distinguish those
// from the transcript alone, so we stay quiet rather than mis-label a
// recovering turn as cancelled. Live turns still in flight
// (`awaitingReply` / `pendingApproval`) are also excluded so the
// indicator doesn't flash mid-stream.
function isCancelledWorkAt(
  transcript: TranscriptRow[],
  i: number,
  awaitingReply: boolean,
  pendingApproval: PendingApproval | null,
): boolean {
  if (i !== transcript.length - 1) return false;
  if (awaitingReply || pendingApproval) return false;
  const row = transcript[i];
  if (row.kind !== 'work' || row.workActive) return false;
  return true;
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
      className="absolute -bottom-2 right-2 opacity-0 group-hover:opacity-100 focus:opacity-100 transition-opacity flex items-center gap-1 px-1.5 py-0.5 bg-white border-2 border-black rounded-md font-mono text-[0.65rem] uppercase tracking-wider hover:bg-gray-100 cursor-pointer"
      title="Copy message"
    >
      {copied ? <RiCheckLine className="text-xs" /> : <RiClipboardLine className="text-xs" />}
      {copied ? 'Copied' : 'Copy'}
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

/** Header dropdown for switching the active conversation's model.
 *  `current` is the session's pin (`null` ⇒ following `default-llm`);
 *  selecting an entry (or "Default") calls `onSelect`, which PUTs the
 *  change. Rendered only when more than one model is configured. */
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
        <div className="absolute right-0 mt-1 z-20 w-[260px] max-h-[60vh] overflow-auto bg-white border-2 border-black rounded-md shadow-brutal py-1">
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
