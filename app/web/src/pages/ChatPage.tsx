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
import { uuid } from '../uuid';
import ReactMarkdown, { type Components } from 'react-markdown';
import remarkGfm from 'remark-gfm';
import {
  RiAlertLine,
  RiArrowDownLine,
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiAttachmentLine,
  RiCheckLine,
  RiClipboardLine,
  RiCloseLine,
  RiDeleteBin6Line,
  RiErrorWarningLine,
  RiFileLine,
  RiInformation2Line,
  RiLoader4Line,
  RiSendPlane2Line,
  RiStopFill,
} from 'react-icons/ri';

import { useAdminClient, useAuth } from '../api/auth';
import {
  ChatWs,
  type ConnectionStatus,
  type Frame,
  type ResourceAccess,
  type SessionPatch,
  type TaskView,
  type WireApprovalCard,
  type WireAttachment,
  type WireWorkStep,
} from '../api/chatWs';
import type { components } from '../api/schema';
import { TaskChecklist } from '../components/chat/TaskChecklist';
import { AttachmentImage } from './chat/AttachmentImage';
import { QueuePanel } from './chat/QueuePanel';
import { SessionSidebar } from './chat/SessionSidebar';
import {
  MAX_AUTO_TRANSMISSIONS,
  OutboxStore,
  dueForBlindResend,
  resendExhausted,
  type OutboxEntry,
} from './chat/outboxStore';
import {
  INITIAL_CURSOR,
  advanceFromLive,
  advanceFromSync,
  type CursorState,
} from './chat/syncCursor';
import { useQueueStore, useSessionQueue, type QueuedItem } from './chat/queueStore';
import { useFolderStore } from './chat/folderStore';
import { useInputHistory } from './chat/inputHistory';
import type { SessionSummary } from './chat/types';

type ApiTranscriptItem = components['schemas']['ChatTranscriptItem'];
type ApiAttachment = components['schemas']['ChatAttachment'];

/** One progress entry inside a turn's work block. `reasoning`, `status`
 *  and `prose` carry `text`; `tool` carries the tool-call fields and is
 *  keyed by `toolCallId` so the completion frame resolves the step its
 *  start created. `prose` is mid-turn answer text the model emitted
 *  before its final reply — folded in here rather than left as its own
 *  bubble. */
export interface WorkStep {
  key: string;
  kind: 'reasoning' | 'tool' | 'status' | 'prose';
  text?: string;
  toolCallId?: string;
  tool?: string;
  toolLabel?: string | null;
  toolStatus?: 'running' | 'ok' | 'error' | 'denied';
  toolSummary?: string;
  /** Prompt id, set while this call is blocked on the user's approval — the
   *  step reads "waiting for approval" until the matching `approval_resolved`
   *  (or the call's own completion) lands. */
  awaitingApproval?: string;
  /** The decision the user gave. Persisted with the tool result, so a reload
   *  still shows what was judged. */
  approval?: 'approve' | 'approve_always' | 'deny';
}

export interface TranscriptRow {
  /** Stable key for React. Synthetic; not part of any server schema. */
  key: string;
  role: 'user' | 'assistant' | 'system';
  text: string;
  /** Streaming text appended via Frame::AnswerDelta until the final
   *  Frame::Message arrives. */
  streaming?: boolean;
  notice?: { level: 'info' | 'warn' | 'error'; text: string };
  hasAttachments?: boolean;
  /** Full attachment details for *live* rows (optimistic sends + WS
   *  frames). History rows carry only `hasAttachments` — the REST
   *  transcript DTO omits details — so those fall back to a placeholder. */
  attachments?: WireAttachment[];
  /** True while a user-authored row is on screen optimistically,
   *  waiting for the server's UserEcho. Cleared when the echo arrives
   *  carrying the same `clientMsgId` in its `platform_msg_id`. */
  pending?: boolean;
  /** True once the outbox exhausted its automatic transmissions for
   *  this row without an echo — renders the red retry affordance.
   *  Cleared by a manual retry, a late echo, or durability confirm. */
  failed?: boolean;
  /** The row's `platform_msg_id` — the send idempotency key. Set on
   *  optimistic sends from this tab (where it is the client-generated
   *  UUID the echo reconciles against) AND on server rows that carried
   *  one (sync / backfill redelivery), so redelivered rows dedup
   *  against the optimistic row and adopt its server identity. */
  clientMsgId?: string;
  /** ISO timestamp the bubble renders next to the message. For
   *  sync/backfill-loaded rows this is the persisted
   *  `session_messages.created_at`; for live WS frames (the wire
   *  shape doesn't carry it) it's the receive time, which is close
   *  enough for genuine live emissions and drifts only until the next
   *  sync re-delivers the row with the real persisted value. */
  createdAt?: string;
  /** Set on the single "work" row that aggregates a turn's intermediate
   *  progress — reasoning, tool calls, status, and mid-turn prose — into
   *  one collapsible block. Built live from progress frames, and
   *  reconstructed from persisted rows on a REST history load (the
   *  server folds each tool-using turn into a `work` transcript item);
   *  `applyTurnState` re-opens the reconstructed block when the server
   *  says its turn is still in flight. See `docs/turn-progress-events.md`. */
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
  /** True when this block's turn was cancelled (`/stop`) rather than run to a
   *  normal reply — the collapsed summary reads "Cancelled · Worked Xs". */
  workCancelled?: boolean;
  /** True for a block closed mid-turn by a user interjection: relabelled
   *  "Worked Xs" but kept EXPANDED (steps visible) until the turn fully ends,
   *  so the work the interjection split off doesn't vanish behind a collapse
   *  while the agent is still replying. Cleared (→ collapse) by
   *  `closeActiveWork` at turn-end. */
  workSettling?: boolean;
}

interface PendingApproval {
  callId: string;
  sessionId: string;
  tool: string;
  description: string | null;
  paramsPreview: string;
  accesses: ResourceAccess[];
}

type ApprovalDecision = 'approve' | 'approve_always' | 'deny';

/** One selectable model in the header picker, projected from a
 *  `GET /v1/llm/models` entry. `name` is the `baybo.json` entry name
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
export interface SessionView {
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
   *  pinned `baybo.json` entry name. Seeded from the GET-session
   *  detail's `last_llm` on history load and updated on a successful
   *  `PUT …/model`. */
  model?: string | null;
  /** The session's planning checklist, replaced wholesale by each
   *  `Frame::TaskList` snapshot (it's idempotent, not a delta). Empty
   *  when the agent has no active plan — the checklist panel hides. */
  tasks: TaskView[];
  /** Server-authoritative "is a turn in flight, since when (epoch ms)".
   *  Fed by `Frame::TurnState` — broadcast at every turn start/end and
   *  snapshotted to this connection on every Subscribe — so a tab that
   *  missed the turn's progress frames (opened mid-turn, reconnected)
   *  still knows the agent is working. `null` = no signal yet on this
   *  connection: nothing that depends on knowing (the Cancelled
   *  indicator) may render. */
  turn: { active: boolean; startedAt: number | null } | null;
}

export const EMPTY_VIEW: SessionView = {
  transcript: [],
  pendingApproval: null,
  historyLoaded: false,
  historyLoading: false,
  olderLoading: false,
  oldestOrdinal: null,
  hasMore: false,
  awaitingReply: false,
  model: null,
  tasks: [],
  turn: null,
};

/** Soft cap on `views` map size. Past this, the oldest non-active
 *  bucket (by frame recency) is evicted: transcript + pendingApproval
 *  freed, WS subscription dropped, recency entry cleared. Revisit
 *  re-subscribes and re-fetches via REST. Tuned high enough that
 *  casual session-switching stays free; bites only when the user has
 *  genuinely roamed across many conversations in one tab session. */
const VIEW_CACHE_LIMIT = 20;

/** Sync `limit` election (docs/sync-protocol.md, decided 2026-07-06): one
 *  UI page for a baseline / cold open (`since` absent — a newest-page
 *  REPLACE by definition), the server hard cap when merging into an
 *  already-rendered thread (a rebase is a REPLACE under a reading user,
 *  so incremental merge is preferred all the way to the cap). */
const SYNC_BASELINE_LIMIT = 50;
const SYNC_MERGE_LIMIT = 200;

/** Safety-net pull cadence: every 3 minutes, the FOREGROUND visible
 *  session only, skipped when any frame for it arrived within the last
 *  interval. Backstop for a lost `gap` nudge / suspended-tab windows. */
const SAFETY_TICK_MS = 180_000;

/** Outbox retry sweep cadence — fine-grained enough to fire within a few
 *  seconds of the 10s no-echo deadline without busy-spinning. */
const OUTBOX_TICK_MS = 3_000;

/** How many ended-turn `started_at` stamps to remember per session for
 *  the SubscribeState turn-identity staleness test. */
const ENDED_TURN_MEMORY = 8;

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

/** Accept slash-command completion: replace the leading `/command` token of
 *  `text` (up to the first whitespace) with `/name ` plus any trailing args —
 *  the web port of the TUI's `completion_accept`. Returns the new composer
 *  value and the caret offset, which lands just after the inserted `/name `. */
export function applySlashCompletion(
  text: string,
  name: string,
): { text: string; caret: number } {
  const firstWs = text.search(/\s/);
  const prefixEnd = firstWs === -1 ? text.length : firstWs;
  const suffix = text.slice(prefixEnd).replace(/^\s+/, '');
  return { text: `/${name} ${suffix}`, caret: name.length + 2 };
}

/** Whether the slash-command popup should be active: the draft is a `/command`
 *  and the caret still sits on that leading token (no whitespace before it) —
 *  the web port of the TUI's `completion_candidates` cursor ≤ prefix_end guard.
 *  Once the caret enters the args, the popup closes and Up/Down/Tab revert to
 *  history recall / focus-trap. */
export function caretOnSlashToken(text: string, caret: number): boolean {
  if (!text.startsWith('/')) return false;
  const firstWs = text.search(/\s/);
  const prefixEnd = firstWs === -1 ? text.length : firstWs;
  return caret <= prefixEnd;
}

/** A queued send carries real content — a non-blank message or an attachment.
 *  Blank items can only arise from an out-of-band localStorage write (the
 *  composer/edit paths refuse them); they're dropped before a batch is sized or
 *  a deferred flush runs so they can't wedge the queue or skew the threshold. */
export function hasSendableContent(item: QueuedItem): boolean {
  return item.text.trim().length > 0 || item.attachments.length > 0;
}

/** Whether a deferred flush goes out as ONE coalesced batch frame rather than
 *  individual sends: 2+ real messages with no slash command among them (a slash
 *  command is a coalescing barrier). Blanks are filtered before the count is
 *  taken, so one real message beside junk still sends individually. */
export function canBatchDeferred(items: readonly QueuedItem[]): boolean {
  const sendable = items.filter(hasSendableContent);
  return sendable.length >= 2 && sendable.every((i) => !isSlashText(i.text));
}

export type QueueFrameAction =
  | 'fire' // dispatch the single top parked item
  | 'fire-deferred' // dispatch every sendable deferred item (batched or one-by-one)
  | 'restore-deferred' // move still-pending deferred items back to the parked queue
  | 'pause-cancelled'
  | 'pause-error'
  | 'none';

/** Session state a queue decision reads, snapshot at the frame boundary. */
export interface QueueFrameCtx {
  /** The session was /stop'd; its salvaged partial reply must not drain the queue. */
  stopped: boolean;
  /** A live turn armed this session (turn token set) — false for a sync
   *  redelivery replayed on reload, which must never auto-fire. */
  armed: boolean;
  /** This turn already dispatched from the queue (fired === token). */
  alreadyFired: boolean;
  /** The queue is paused after a cancelled/errored reply. */
  paused: boolean;
  hasItems: boolean;
  hasDeferred: boolean;
}

/** Pure decision for what an inbound frame does to a session's send queue,
 *  extracted from `drainQueueOnFrame` so the auto-fire / restore / pause rules
 *  are unit-testable independent of the side effects (the send calls, the store
 *  mutations, the fired-this-turn bookkeeping) the caller still owns. Auto-fire
 *  keys on a real turn completion; a reload redelivery (unarmed) never fires. */
export function classifyQueueFrame(frame: Frame, ctx: QueueFrameCtx): QueueFrameAction {
  if (frame.kind === 'message' && frame.role !== 'user') {
    if (ctx.stopped || !ctx.armed || ctx.alreadyFired || ctx.paused) return 'none';
    if (ctx.hasDeferred) return 'fire-deferred';
    if (ctx.hasItems) return 'fire';
    return 'none';
  }
  if (frame.kind === 'turn_state' && !frame.active) {
    // The one turn-end signal that ALWAYS fires. If the message branch didn't
    // already dispatch this turn, still-pending deferred items can't ride this
    // completion — move them back to the parked queue rather than strand them.
    if (!ctx.alreadyFired && ctx.hasDeferred && !ctx.paused) return 'restore-deferred';
    return 'none';
  }
  if (frame.kind === 'notice' && frame.transient !== true) {
    if (!ctx.hasItems && !ctx.hasDeferred) return 'none';
    if (isStopCancellationNotice(frame.text)) return 'pause-cancelled';
    if (frame.level === 'error') return 'pause-error';
  }
  return 'none';
}

export function ChatPage() {
  const { sessionId } = useParams<{ sessionId?: string }>();
  const navigate = useNavigate();
  const client = useAdminClient();
  const { baseUrl, token: adminToken } = useAuth();
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

  // Files picked in the composer, uploaded to the blob store on select and
  // attached to the next outgoing message once their upload lands.
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const anchorSessionIdRef = useRef<string | null>(null);

  // Per-session view buckets keyed by session_id. `currentView` is
  // the derived projection of the URL's sessionId.
  const [views, setViews] = useState<Record<string, SessionView>>({});
  const currentView = (sessionId && views[sessionId]) || EMPTY_VIEW;
  const activeTitle = sessionId
    ? sessions.find((s) => s.session_id === sessionId)?.title
    : undefined;
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

  // ── Sync protocol v2 state ──────────────────────────────────────────
  // Per-session sync cursor: advanced max-wins from sync `next_cursor`
  // and from ordinal-stamped live Message frames (frozen against live
  // advances while rebase-dirty). In-memory only — `null` on a fresh tab
  // means the first sync is a newest-page baseline.
  const cursorsRef = useRef<Map<string, CursorState>>(new Map());
  // Epoch ms of the most recent inbound frame per session (ephemeral
  // frames count). The safety tick skips a session whose stream was live
  // within the interval.
  const lastFrameAtRef = useRef<Map<string, number>>(new Map());
  // `started_at` (epoch ms) of the turn currently known active per
  // session, and the recent turn starts already seen ENDING — the
  // SubscribeState turn-identity test discards the snapshot's turn/work
  // halves only when their `started_at` is in the ended set.
  const activeTurnStartRef = useRef<Map<string, number>>(new Map());
  const endedTurnStartsRef = useRef<Map<string, number[]>>(new Map());
  // One in-flight sync per session — open/gap/tick/reconnect triggers
  // coalesce onto the same request instead of stacking.
  const syncInFlightRef = useRef<Map<string, Promise<void>>>(new Map());
  // False until the first 'connected' edge. The bootstrap already
  // fetches the list/folders once, so only RE-connect edges refetch.
  const hadConnectedRef = useRef(false);
  // Persisted send outbox (localStorage-backed), stable for the tab.
  const outboxRef = useRef<OutboxStore | null>(null);
  if (outboxRef.current === null) outboxRef.current = new OutboxStore();
  const outbox = outboxRef.current;
  // Late-bound `markRead` so the sync loop (defined earlier) can advance the
  // server read cursor without a declaration-order cycle.
  const markReadRef = useRef<(sid: string) => void>(() => {});

  const [status, setStatus] = useState<ConnectionStatus>({ state: 'connecting' });
  // Mirrors `status` in a ref so the captured-once `onFrame` closure and the
  // session-agnostic `sendToSession` read the live connection state without a
  // stale 'connecting' after a reconnect. Updated in the same `onStatus`
  // callback that calls `setStatus`.
  const statusRef = useRef<ConnectionStatus>({ state: 'connecting' });
  const [composer, setComposer] = useState('');
  const [showSlashHints, setShowSlashHints] = useState(false);
  // Highlighted row in the slash-command popup; Up/Down move it, Tab/click accept.
  const [selectedSlash, setSelectedSlash] = useState(0);
  // Shell-style input ring (Up/Down recalls submitted messages), a port of the
  // TUI history. `pendingCaret` parks the caret at a target offset once React
  // has committed a programmatic composer replace (history recall → end of the
  // recalled entry; slash completion → just after the inserted command).
  const inputHistory = useInputHistory();
  const pendingCaret = useRef<number | null>(null);

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
  // session switch. The ref gives long-lived closures a stable handle to
  // "whatever navigate is right now".
  const navigateRef = useRef(navigate);
  useEffect(() => {
    navigateRef.current = navigate;
  });
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
  // live turn this page-load, so sync redeliveries applied on reload never
  // spuriously drain the queue.
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
    // Cursor and turn trackers go with the transcript: a revisit
    // re-baselines via sync(null) — keeping a cursor with no rendered
    // rows would make the next sync a difference into an empty thread.
    cursorsRef.current.delete(sid);
    lastFrameAtRef.current.delete(sid);
    activeTurnStartRef.current.delete(sid);
    endedTurnStartsRef.current.delete(sid);
    setViews((prev) => {
      if (!(sid in prev)) return prev;
      const next = { ...prev };
      delete next[sid];
      return next;
    });
    recencyRef.current.delete(sid);
  }, [cancelPacer]);

  // ── Sync loop + outbox plumbing (protocol v2) ───────────────────────

  const recordEndedTurn = useCallback((sid: string, startedAtMs: number) => {
    const ended = endedTurnStartsRef.current.get(sid) ?? [];
    if (ended.includes(startedAtMs)) return;
    endedTurnStartsRef.current.set(sid, [...ended, startedAtMs].slice(-ENDED_TURN_MEMORY));
  }, []);

  /** Patch the transcript row carrying `platformMsgId` (optimistic send
   *  or adopted redelivery) — drives the pending/failed indicators. */
  const markSendRow = useCallback(
    (sid: string, platformMsgId: string, patch: Partial<TranscriptRow>) => {
      setViews((prev) => {
        const view = prev[sid];
        if (!view) return prev;
        const idx = view.transcript.findIndex((r) => r.clientMsgId === platformMsgId);
        if (idx < 0) return prev;
        const next = view.transcript.slice();
        next[idx] = { ...next[idx], ...patch };
        return { ...prev, [sid]: { ...view, transcript: next } };
      });
    },
    [],
  );

  /** Re-transmit one outbox entry over the live WS (same
   *  `platform_msg_id` — the gateway's InboundDedup absorbs a duplicate
   *  inside its recency window). Counts toward the automatic cap. */
  const resendOutboxEntry = useCallback(
    (sid: string, entry: OutboxEntry): boolean => {
      const ws = wsRef.current;
      if (!ws || statusRef.current.state !== 'connected') return false;
      ws.sendMessage({
        sessionId: sid,
        userId: 'web-operator',
        content: entry.text,
        clientMsgId: entry.platformMsgId,
        attachments: entry.attachments,
      });
      outbox.recordTransmission(sid, entry.platformMsgId);
      return true;
    },
    [outbox],
  );

  const failOutboxEntry = useCallback(
    (sid: string, platformMsgId: string) => {
      outbox.markFailed(sid, platformMsgId);
      markSendRow(sid, platformMsgId, { pending: false, failed: true });
    },
    [outbox, markSendRow],
  );

  /** Resolve a rebase-floored (`unknown`) entry via the per-key point
   *  lookup — durability confirmed releases it; provable absence resumes
   *  the retry machine. Neither consumes a transmission. */
  const resolveUnknownEntry = useCallback(
    async (sid: string, platformMsgId: string) => {
      const { data, error } = await client.GET('/v1/chat/sessions/{session_id}/messages', {
        params: { path: { session_id: sid }, query: { platform_msg_id: platformMsgId } },
      });
      if (error || !data) return; // stays `unknown`; the next reconnect edge retries the probe
      if (data.found) {
        if (outbox.confirmDurable(sid, platformMsgId)) {
          markSendRow(sid, platformMsgId, { pending: false, failed: false });
        }
      } else {
        outbox.resumeSending(sid, platformMsgId);
      }
    },
    [client, outbox, markSendRow],
  );

  /** Ordinal-stamped rows carrying a `platform_msg_id` (sync / backfill
   *  pages) are durability confirmations — release their outbox entries. */
  const confirmDurableFromItems = useCallback(
    (sid: string, items: ApiTranscriptItem[]) => {
      for (const item of items) {
        if (item.kind !== 'message' || !item.platform_msg_id || item.ordinal == null) continue;
        if (outbox.confirmDurable(sid, item.platform_msg_id)) {
          markSendRow(sid, item.platform_msg_id, { pending: false, failed: false });
        }
      }
    },
    [outbox, markSendRow],
  );

  /** The one forward-recovery pull (docs/sync-protocol.md "The one client
   *  algorithm"): session open, reconnect, gap nudge and the safety tick
   *  all land here. `since = cursor` (absent on a fresh view → baseline
   *  REPLACE); a rebased response also REPLACEs and dirties the cursor. */
  const runSyncSession = useCallback(
    (sid: string): Promise<void> => {
      const inFlight = syncInFlightRef.current.get(sid);
      if (inFlight) return inFlight;
      const failBaseline = (reason: string) => {
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          if (view.historyLoaded) return prev;
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: [
                {
                  key: `sync-err-${sid}-${Date.now()}`,
                  role: 'system',
                  text: '',
                  notice: {
                    level: 'warn',
                    text: `Couldn't load conversation history: ${reason}. New messages will still arrive live.`,
                  },
                },
              ],
              historyLoaded: true,
              historyLoading: false,
            },
          };
        });
      };
      const task = (async () => {
        const before = cursorsRef.current.get(sid) ?? INITIAL_CURSOR;
        const baseline = before.cursor === null;
        try {
          const { data, error } = await client.GET('/v1/chat/sessions/{session_id}/sync', {
            params: {
              path: { session_id: sid },
              query: {
                ...(before.cursor === null ? {} : { since_ordinal: before.cursor }),
                limit: baseline ? SYNC_BASELINE_LIMIT : SYNC_MERGE_LIMIT,
              },
            },
          });
          if (error || !data) {
            console.warn('chat sync failed', sid, error);
            if (baseline) failBaseline(formatHttpError(error));
            return;
          }
          const replace = data.rebased || baseline;
          const pageRows = data.rows.map((item) => transcriptItemToRow(sid, item));
          confirmDurableFromItems(sid, data.rows);
          // A work row is a turn-END signal for the SubscribeState
          // turn-identity test ONLY when its turn is demonstrably complete —
          // i.e. a message row follows it in the (ascending) page. The
          // in-flight turn's trailing partial work row has no later message,
          // so recording it as ended would wrongly discard a live
          // SubscribeState for the SAME turn (its work block still active).
          for (let i = 0; i < data.rows.length; i++) {
            const item = data.rows[i];
            if (item.kind !== 'work') continue;
            const followedByMessage = data.rows.slice(i + 1).some((r) => r.kind === 'message');
            if (!followedByMessage) continue;
            const ms = parseEpochMs(item.work_started_at);
            if (ms !== null) recordEndedTurn(sid, ms);
          }
          const unconfirmed = outbox.unconfirmedIds(sid);
          setViews((prev) => {
            const view = prev[sid] ?? EMPTY_VIEW;
            const transcript = replace
              ? applySyncReplace(view.transcript, pageRows, unconfirmed, view.turn)
              : applySyncMerge(view.transcript, pageRows);
            return {
              ...prev,
              [sid]: {
                ...view,
                transcript,
                historyLoaded: true,
                historyLoading: false,
                ...(replace
                  ? { oldestOrdinal: data.oldest_ordinal ?? null, hasMore: data.has_more_older }
                  : {}),
              },
            };
          });
          // Advance against the CURRENT cursor (live final-reply
          // ordinals may have raced this response), max-wins; a rebased
          // page dirties it until one non-rebased sync completes.
          cursorsRef.current.set(
            sid,
            advanceFromSync(
              cursorsRef.current.get(sid) ?? INITIAL_CURSOR,
              data.next_cursor ?? null,
              data.rebased,
            ),
          );
          // Viewing this session and just synced it → it's read up to here.
          if (sid === currentSessionIdRef.current) markReadRef.current(sid);
          if (data.rebased) {
            // The rebase floor makes "absent from the page" unknowable —
            // park unconfirmed entries `unknown` and resolve each via the
            // point lookup instead of blind-resending (outbox rule 4).
            for (const entry of outbox.entries(sid)) {
              if (entry.state === 'failed') continue;
              outbox.markUnknown(sid, entry.platformMsgId);
              void resolveUnknownEntry(sid, entry.platformMsgId);
            }
          }
        } catch (e) {
          console.warn('chat sync threw', sid, e);
          if (baseline) failBaseline(String(e));
        }
      })().finally(() => {
        syncInFlightRef.current.delete(sid);
      });
      syncInFlightRef.current.set(sid, task);
      return task;
    },
    [client, outbox, confirmDurableFromItems, recordEndedTurn, resolveUnknownEntry],
  );

  /** The session-list/folder plane has no cursor — pull is its only loss
   *  recovery. Runs on every RE-connect edge and on `gap(None)`. `unread` is
   *  now server-computed (`unread_count`), so the pull reconciles the badge to
   *  the truth — a cold restart / a missed live ping self-heals here. */
  const refetchSessionsAndFolders = useCallback(async () => {
    const [{ data: list, error: listError }, { data: folderList, error: folderError }] =
      await Promise.all([client.GET('/v1/chat/sessions'), client.GET('/v1/chat/folders')]);
    if (listError) console.warn('chat refetch: list sessions failed', listError);
    if (folderError) console.warn('chat refetch: list folders failed', folderError);
    if (folderList) {
      folderStore.replaceFolders(
        (folderList.items ?? []).map((f) => ({
          id: f.id,
          parent_id: f.parent_id ?? undefined,
          name: f.name,
          position: f.position,
          created_at: f.created_at,
        })),
      );
    }
    if (list) {
      setSessions(() => {
        return (list.items ?? []).map((s) => ({
          session_id: s.session_id,
          created_at: s.created_at,
          last_active: s.last_active,
          unread: s.unread_count ?? 0,
          pinned: s.pinned,
          last_user_text: s.last_user_text ?? undefined,
          folder_id: s.folder_id ?? undefined,
          title: s.title ?? undefined,
          cron_job_id: s.cron_job_id ?? undefined,
          cron_job_title: s.cron_job_title ?? undefined,
        }));
      });
    }
  }, [client, folderStore]);

  /** Advance the server read cursor to the session's coverage watermark and
   *  optimistically clear the local badge. Fire-and-forget (the cursor is
   *  max-wins server-side); skipped when there's no cursor yet (nothing to
   *  mark). Called when the user is viewing the session — on open, after a
   *  sync completes for it, and on a live reply while it's foreground. */
  const markRead = useCallback(
    (sid: string) => {
      const cursor = cursorsRef.current.get(sid)?.cursor;
      if (cursor === null || cursor === undefined) return;
      void client.PUT('/v1/chat/sessions/{session_id}/read', {
        params: { path: { session_id: sid } },
        body: { ordinal: cursor },
      });
      setSessions((prev) => {
        const idx = prev.findIndex((s) => s.session_id === sid);
        if (idx === -1 || prev[idx].unread === 0) return prev;
        const next = prev.slice();
        next[idx] = { ...prev[idx], unread: 0 };
        return next;
      });
    },
    [client],
  );
  markReadRef.current = markRead;

  /** Manual retry from the failed-row affordance: same
   *  `platform_msg_id`, automatic-transmission budget reset. */
  const retryFailedSend = useCallback(
    (sid: string, platformMsgId: string) => {
      if (statusRef.current.state !== 'connected') return;
      const entry = outbox.resetForManualRetry(sid, platformMsgId);
      if (!entry) return;
      markSendRow(sid, platformMsgId, { pending: true, failed: false });
      resendOutboxEntry(sid, entry);
    },
    [outbox, markSendRow, resendOutboxEntry],
  );

  // ── Bootstrap: load session list + slash manifest ──────────────────
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
        unread: s.unread_count ?? 0,
        pinned: s.pinned,
        last_user_text: s.last_user_text ?? undefined,
        folder_id: s.folder_id ?? undefined,
        title: s.title ?? undefined,
        cron_job_id: s.cron_job_id ?? undefined,
        cron_job_title: s.cron_job_title ?? undefined,
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

      if (preferred) {
        anchorSessionIdRef.current = preferred;
        if (!sessionId || sessionId !== preferred) {
          navigateRef.current(`/chat/${preferred}`, { replace: true });
        }
      } else {
        const anchorId = await createAnchorSession();
        if (!anchorId) return;
        anchorSessionIdRef.current = anchorId;
        if (!sessionId || sessionId !== anchorId) {
          navigateRef.current(`/chat/${anchorId}`, { replace: true });
        }
      }

      async function createAnchorSession(): Promise<string | null> {
        const { data, error } = await client.POST('/v1/chat/sessions', { body: {} });
        if (cancelled) return null;
        if (error || !data?.session_id) {
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
        return data.session_id;
      }
    })();
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, folderStore]); // intentionally NOT depending on sessionId — bootstrap is one-shot

  // ── WS lifecycle: tied to adminToken, not to sessionId ──────────────
  // Opens once we have the admin token, lives until the component unmounts
  // (i.e. the user navigates away from /chat). Reconnect is internal
  // to ChatWs.
  useEffect(() => {
    if (!adminToken) return;
    const ws = new ChatWs({
      baseUrl,
      adminToken,
      initialSessionIds: [],
      onStatus: (s) => {
        statusRef.current = s;
        setStatus(s);
      },
      onFrame: (frame) => {
        // ANY frame for a session proves its stream is live — ephemeral
        // frames included — so the safety tick can skip its pull.
        if ('session_id' in frame && typeof frame.session_id === 'string') {
          lastFrameAtRef.current.set(frame.session_id, Date.now());
        }
        if (frame.kind === 'subscribe_state') {
          // The atomic state-plane bundle, once per Subscribe. Tasks and
          // approvals are latest-wins REPLACEs; the turn/work halves are
          // discarded only by turn identity — this client already saw a
          // turn-end signal for the SAME turn (matched by started_at),
          // never by comparing the cursor against as_of_ordinal.
          const sid = frame.session_id;
          const startedAtMs = parseEpochMs(frame.turn.started_at);
          const turnEnded =
            startedAtMs !== null &&
            (endedTurnStartsRef.current.get(sid) ?? []).includes(startedAtMs);
          if (frame.turn.active && startedAtMs !== null && !turnEnded) {
            activeTurnStartRef.current.set(sid, startedAtMs);
          }
          setViews((prev) => ({
            ...prev,
            [sid]: applySubscribeState(prev[sid] ?? EMPTY_VIEW, frame, turnEnded),
          }));
          return;
        }
        if (frame.kind === 'gap') {
          // Server-declared loss. Scoped → sync that session; session-less
          // → sync every subscribed session AND refetch the list/folder
          // plane (no cursor there; pull is its only recovery).
          if (frame.session_id) {
            void runSyncSession(frame.session_id);
          } else {
            for (const sid of wsRef.current?.subscribedSessions() ?? []) {
              void runSyncSession(sid);
            }
            void refetchSessionsAndFolders();
          }
          return;
        }
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
            // A conversation this tab has never seen just spoke — a recurring
            // cron fire opening its own conversation, or one started in
            // another tab. Activity carries no session metadata, so pull the
            // list to learn what it is; the row then renders with its title
            // and unread badge like any other.
            void refetchSessionsAndFolders();
          }
          return;
        }
        // The live final assistant reply carries its persisted ordinal —
        // advance the sync cursor (max-wins; frozen while rebase-dirty,
        // when only a sync next_cursor may move it).
        if (frame.kind === 'message' && frame.ordinal !== undefined) {
          const cur = cursorsRef.current.get(frame.session_id) ?? INITIAL_CURSOR;
          cursorsRef.current.set(frame.session_id, advanceFromLive(cur, frame.ordinal));
          // A reply while the user is looking at the session is already read —
          // advance the server cursor so the badge stays 0 across a refetch.
          if (frame.session_id === currentSessionIdRef.current) {
            markReadRef.current(frame.session_id);
          }
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
          // Remember the turn's identity for the SubscribeState
          // staleness test (matched by started_at on re-subscribe).
          const startedAtMs = parseEpochMs(frame.started_at);
          if (startedAtMs !== null) {
            activeTurnStartRef.current.set(frame.session_id, startedAtMs);
          }
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
          // Turn-end signal: record the ended turn's identity, and — on a
          // rebase-dirty cursor — run the follow-up sync that closes the
          // mid-turn interjection window (only a sync next_cursor may
          // advance a dirty cursor).
          const started = activeTurnStartRef.current.get(frame.session_id);
          if (started !== undefined) {
            recordEndedTurn(frame.session_id, started);
            activeTurnStartRef.current.delete(frame.session_id);
          }
          if (cursorsRef.current.get(frame.session_id)?.rebaseDirty) {
            void runSyncSession(frame.session_id);
          }
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
          // The ordinal-stamped final reply is also a turn-end signal:
          // record the turn identity, and heal a rebase-dirty cursor
          // with the follow-up sync.
          if (frame.ordinal !== undefined) {
            const started = activeTurnStartRef.current.get(frame.session_id);
            if (started !== undefined) recordEndedTurn(frame.session_id, started);
            if (cursorsRef.current.get(frame.session_id)?.rebaseDirty) {
              void runSyncSession(frame.session_id);
            }
          }
        }
        // The ordinal-less user echo is the transport ack for a send —
        // flip its outbox entry `sending → sent` (in-connection retries
        // stop; the entry is retained until an ordinal-stamped row with
        // the same platform_msg_id confirms durability).
        if (
          frame.kind === 'message' &&
          frame.role === 'user' &&
          frame.ordinal === undefined &&
          frame.platform_msg_id
        ) {
          if (outbox.markEchoed(frame.session_id, frame.platform_msg_id)) {
            markSendRow(frame.session_id, frame.platform_msg_id, { failed: false });
          }
        }
        routeInboundFrame(frame, setViews, setSessions);
        // Interjection queue: auto-fire the next parked item on a live normal
        // completion, and pause the pipeline on a /stop-cancel or error notice.
        queueFrameRef.current?.(frame);
      },
    });
    wsRef.current = ws;
    return () => {
      ws.close();
      wsRef.current = null;
    };
  }, [
    baseUrl,
    adminToken,
    releaseSessionView,
    enqueueDelta,
    cancelPacer,
    flushPacerKeepStreaming,
    folderStore,
    outbox,
    markSendRow,
    recordEndedTurn,
    runSyncSession,
    refetchSessionsAndFolders,
  ]);

  // ── Active session: subscribe + sync ────────────────────────────────
  // Subscribe stays sticky once added: when the user switches away,
  // we keep the subscription so background sessions still accumulate
  // Delta/Message frames into their view bucket. The LRU eviction
  // effect above caps the per-tab bucket count at `VIEW_CACHE_LIMIT`
  // and drops the WS subscription alongside the freed transcript.
  //
  // The transcript itself comes from the one sync loop: a cold open runs
  // a baseline sync (`since` absent → newest-page REPLACE), a revisit of
  // a loaded view re-syncs from its cursor (merge). The GET-session call
  // is kept only to read the session meta (`last_llm`) for the model
  // picker — its transcript slice (limit 1) is ignored. Turn/work state
  // arrives via the `subscribe_state` bundle, never from history.
  useEffect(() => {
    if (!sessionId || !wsRef.current) return;
    wsRef.current.subscribe(sessionId);
    const existing = views[sessionId];
    if (existing && existing.historyLoading) return;
    if (existing && existing.historyLoaded) {
      void runSyncSession(sessionId);
      return;
    }
    setViews((prev) => mergeView(prev, sessionId, { historyLoading: true }));
    void client
      .GET('/v1/chat/sessions/{session_id}', {
        params: { path: { session_id: sessionId }, query: { limit: 1 } },
      })
      .then(({ data }) => {
        if (!data) return;
        setViews((prev) => mergeView(prev, sessionId, { model: data.last_llm ?? null }));
        // Seed the conversation title from the detail response so the header's
        // `activeTitle` and the sidebar row converge even when the list fetch
        // predated the title (or this tab was disconnected and missed the WS
        // patch). Merge it like a live `SessionUpdated` patch — a title is only
        // ever set, never cleared, so an absent `title` means "no change".
        if (data.title) {
          const title = data.title;
          setSessions((prev) => applySessionPatch(prev, sessionId, { title }));
        }
      })
      .catch((e) => console.warn('chat session meta load failed', sessionId, e));
    void runSyncSession(sessionId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, adminToken, runSyncSession]); // views intentionally excluded — we react to nav; the sync loop self-guards

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
      // Advance the server read cursor too (best-effort — no-op until the
      // session's first sync sets a cursor, after which runSyncSession marks
      // it read). Otherwise the badge would reappear on the next list refetch.
      markReadRef.current(sessionId);
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

  // ── Connected edge: sync + outbox reconciliation ────────────────────
  // On every RE-connect: sync each subscribed session (the reconciliation
  // gate — the server replays nothing), refetch the session list +
  // folders (that plane has no cursor), then resend outbox entries still
  // lacking durability confirmation. The FIRST connected edge skips the
  // refetch (the bootstrap just fetched both) and the subscribed sweep
  // (the session-open effect already syncs the active session), but
  // still runs the outbox pass so sends persisted by a previous
  // page-load recover.
  useEffect(() => {
    if (status.state !== 'connected') return;
    const isReconnect = hadConnectedRef.current;
    hadConnectedRef.current = true;
    void (async () => {
      if (isReconnect) void refetchSessionsAndFolders();
      const sids = new Set<string>([
        ...(isReconnect ? (wsRef.current?.subscribedSessions() ?? []) : []),
        ...outbox.sessionIds(),
      ]);
      // Recover sessions in parallel — each session's own sync→outbox order
      // is preserved, but one slow session no longer blocks the rest. The
      // set is bounded by the view cache (~VIEW_CACHE_LIMIT) plus outbox
      // sessions, so the reconnect burst stays small.
      await Promise.all(
        [...sids].map(async (sid) => {
          // Sync first: a send whose ack was lost but that DID persist is
          // confirmed (and released) here instead of being re-sent.
          await runSyncSession(sid);
          for (const entry of outbox.entries(sid)) {
            if (entry.state === 'unknown') await resolveUnknownEntry(sid, entry.platformMsgId);
          }
          for (const entry of outbox.entries(sid)) {
            if (entry.state !== 'sending' && entry.state !== 'sent') continue;
            if (entry.transmissions >= MAX_AUTO_TRANSMISSIONS) {
              failOutboxEntry(sid, entry.platformMsgId);
              continue;
            }
            resendOutboxEntry(sid, entry);
          }
        }),
      );
    })();
  }, [
    status.state,
    outbox,
    refetchSessionsAndFolders,
    runSyncSession,
    resolveUnknownEntry,
    failOutboxEntry,
    resendOutboxEntry,
  ]);

  // ── Safety-net pull ─────────────────────────────────────────────────
  // Every 3 minutes, sync the FOREGROUND visible session — skipped when
  // any frame for it (ephemeral included) arrived within the interval,
  // and while the tab is hidden. Backstops a lost `gap` nudge.
  useEffect(() => {
    const id = window.setInterval(() => {
      const sid = currentSessionIdRef.current;
      if (!sid) return;
      if (document.visibilityState !== 'visible') return;
      const last = lastFrameAtRef.current.get(sid) ?? 0;
      if (Date.now() - last < SAFETY_TICK_MS) return;
      void runSyncSession(sid);
    }, SAFETY_TICK_MS);
    return () => window.clearInterval(id);
  }, [runSyncSession]);

  // ── Outbox retry sweep ──────────────────────────────────────────────
  // While connected: a `sending` entry with no echo for 10s gets ONE
  // blind in-connection resend (same platform_msg_id — the gateway's
  // dedup absorbs a duplicate); past the 3-transmission cap it flips to
  // `failed` and surfaces the manual retry affordance.
  useEffect(() => {
    const id = window.setInterval(() => {
      if (statusRef.current.state !== 'connected') return;
      const now = Date.now();
      for (const sid of outbox.sessionIds()) {
        for (const entry of outbox.entries(sid)) {
          if (dueForBlindResend(entry, now)) {
            resendOutboxEntry(sid, entry);
          } else if (resendExhausted(entry, now)) {
            failOutboxEntry(sid, entry.platformMsgId);
          }
        }
      }
    }, OUTBOX_TICK_MS);
    return () => window.clearInterval(id);
  }, [outbox, resendOutboxEntry, failOutboxEntry]);

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
      const newRows = data.transcript.map((item) => transcriptItemToRow(sessionId, item));
      // A backfill page is a durability surface too — release any outbox
      // entry whose ordinal-stamped row shows up here.
      confirmDurableFromItems(sessionId, data.transcript);
      // Real message-ordinal bound from the server (not the transcript items,
      // which may include control events without one).
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
  }, [client, sessionId, views, confirmDurableFromItems]);

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

  // After a programmatic composer replace (history recall / slash completion),
  // place the caret at the requested offset. Runs only when a replace set the
  // target, so ordinary typing leaves the caret untouched. Refocuses so a
  // mouse-driven slash pick lands the user back in the box ready to type args.
  useLayoutEffect(() => {
    if (pendingCaret.current === null) return;
    const target = pendingCaret.current;
    pendingCaret.current = null;
    const ta = composerRef.current;
    if (!ta) return;
    const pos = Math.min(target, ta.value.length);
    ta.focus();
    ta.setSelectionRange(pos, pos);
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
      const clientMsgId = uuid();
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
      // Enroll the send in the persisted outbox before it hits the wire.
      // Slash commands stay one-shot: their durable trace is a control
      // event that never carries platform_msg_id, so a durability
      // confirmation can't exist and the entry would retry forever.
      if (!isSlashText(trimmed)) {
        outbox.beginSend(targetSessionId, {
          platformMsgId: clientMsgId,
          text: trimmed,
          attachments: wireAttachments,
        });
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
    [outbox],
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
          clientMsgId: uuid(),
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
      // Same outbox enrollment as sendToSession (slash items can't reach
      // here — the drain sends those individually — but guard anyway).
      for (const m of prepared) {
        if (isSlashText(m.text)) continue;
        outbox.beginSend(targetSessionId, {
          platformMsgId: m.clientMsgId,
          text: m.text,
          attachments: m.attachments,
        });
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
    [outbox],
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
      const clientMsgId = uuid();
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
      // Record the submitted line in the input ring (send, park, or a typed
      // `/stop` all count; `commit` trims, dedupes, and ignores empties).
      inputHistory.commit(composer);
      if (action === 'stop') {
        sendText('/stop');
      } else if (action === 'direct') {
        sendText(composer, wire);
      } else {
        queue.enqueue({ id: uuid(), text: trimmed, attachments: wire });
      }
      setComposer('');
      attachments.forEach((a) => {
        if (a.previewUrl) URL.revokeObjectURL(a.previewUrl);
      });
      setAttachments([]);
      setShowSlashHints(false);
    },
    [composer, busy, attachments, sendText, queue, inputHistory],
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
      // Only these three carry a session_id the queue reacts to; anything else
      // is inert. The decision is pure (`classifyQueueFrame`); this callback
      // owns the side effects — the send calls, the store mutations, and the
      // fired-this-turn bookkeeping that keeps a live completion single-fire.
      if (frame.kind !== 'message' && frame.kind !== 'turn_state' && frame.kind !== 'notice') {
        return;
      }
      const store = queueStoreRef.current;
      const sid = frame.session_id;
      const token = turnTokenRef.current.get(sid);
      const snap = store.queue(sid);
      const action = classifyQueueFrame(frame, {
        stopped: stoppedSessionsRef.current.has(sid),
        armed: token !== undefined,
        alreadyFired: token !== undefined && firedForTurnRef.current.get(sid) === token,
        paused: snap.pauseReason !== null,
        hasItems: snap.items.length > 0,
        hasDeferred: snap.deferred.length > 0,
      });
      switch (action) {
        case 'none':
          return;
        case 'restore-deferred':
          store.restoreDeferred(sid);
          return;
        case 'pause-cancelled':
          // The reply a deferred message was waiting on was cancelled — move it
          // back to the parked queue and pause so it isn't auto-sent; the
          // banner's "Send remaining" is the explicit resume.
          store.restoreDeferred(sid);
          store.setPause(sid, 'cancelled');
          return;
        case 'pause-error':
          store.restoreDeferred(sid);
          store.setPause(sid, 'error');
          return;
        case 'fire': {
          const top = snap.items[0];
          if (!top || token === undefined) return;
          firedForTurnRef.current.set(sid, token);
          if (sendToSession(sid, top.text, top.attachments)) {
            store.removeItem(sid, top.id);
          } else {
            // Disconnected — leave it queued and allow a later retry.
            firedForTurnRef.current.delete(sid);
          }
          return;
        }
        case 'fire-deferred': {
          if (token === undefined) return;
          // Deferred ("waiting in the thread") messages ALL go out together as
          // soon as the reply completes, so the agent answers them as one
          // merged turn. Drop content-less junk first (an out-of-band
          // localStorage write — the composer/edit paths refuse blank items).
          const sendable = snap.deferred.filter(hasSendableContent);
          for (const item of snap.deferred) {
            if (!sendable.includes(item)) store.removeDeferred(sid, item.id);
          }
          if (sendable.length === 0) return;
          firedForTurnRef.current.set(sid, token);
          // 2+ plain messages go as ONE batch frame so the server coalesces
          // them deterministically (no per-message intake race).
          if (canBatchDeferred(sendable)) {
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
      }
    },
    [sendToSession],
  );

  useEffect(() => {
    queueFrameRef.current = drainQueueOnFrame;
  }, [drainQueueOnFrame]);

  const uploadAttachment = useCallback(
    async (file: File) => {
      const localId = uuid();
      const mime = file.type || 'application/octet-stream';
      // Instant composer thumbnail for images, straight from the local file
      // (no upload round-trip needed to preview it).
      const previewUrl = mime.startsWith('image/') ? URL.createObjectURL(file) : undefined;
      setAttachments((prev) => [
        ...prev,
        { localId, filename: file.name, mime, size: file.size, status: 'uploading', previewUrl },
      ]);
      try {
        // The web operator's admin bearer authorises `/v1/blobs` and
        // resolves to `AuthedClient::Web`, which bypasses pairing; the
        // returned content-addressed blob id is what the message references.
        const base = (baseUrl || '').replace(/\/+$/, '');
        const res = await fetch(`${base}/v1/blobs`, {
          method: 'POST',
          headers: {
            Authorization: `Bearer ${adminToken ?? ''}`,
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
    [baseUrl, adminToken],
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

  // Slash-command completion candidates: when the draft starts with `/`, the
  // commands whose name prefix-matches the typed token. Mirrors the TUI's
  // `completion_candidates`.
  const filteredSlash = useMemo(() => {
    if (!showSlashHints) return [];
    const query = composer.slice(1).split(/\s/)[0]?.toLowerCase() ?? '';
    return slashCommands.filter(
      (s) => query.length === 0 || s.command.toLowerCase().startsWith(query),
    );
  }, [showSlashHints, composer, slashCommands]);

  // Accept a slash candidate, replacing the command token up to the first
  // whitespace with `/name ` plus any trailing args — a port of the TUI's
  // `completion_accept`. The caret lands just after the inserted `/name `.
  const completeSlash = useCallback(
    (index: number) => {
      const name = filteredSlash[index]?.command;
      if (name === undefined) return;
      const { text, caret } = applySlashCompletion(composer, name);
      setShowSlashHints(false);
      setSelectedSlash(0);
      // Guard the no-op replace so a bailed-out render can't strand pendingCaret.
      if (text === composer) return;
      setComposer(text);
      pendingCaret.current = caret;
    },
    [composer, filteredSlash],
  );

  const handleComposerKey = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      // An IME composition is active: let the textarea finalize the candidate.
      // Must precede Enter — the Enter that commits a CJK candidate fires a
      // keydown with isComposing=true and must NOT submit the half-composed
      // draft (nor navigate history / hijack arrows).
      if (e.nativeEvent.isComposing) return;
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        const hasReady = attachments.some((a) => a.status === 'ready');
        // Enter submits whether idle or busy — handleSend decides send vs park.
        if (composer.trim().length > 0 || hasReady) {
          const form = e.currentTarget.form;
          form?.requestSubmit();
        }
        return;
      }
      // Tab never leaves the composer (no focus jump to the footer buttons); it
      // accepts the highlighted slash candidate when the popup is open.
      if (e.key === 'Tab' && !e.shiftKey) {
        e.preventDefault();
        if (filteredSlash.length > 0) {
          completeSlash(Math.min(selectedSlash, filteredSlash.length - 1));
        }
        return;
      }
      // Slash popup open: Up/Down move the highlight (wrapping), like the TUI's
      // completion nav — taking precedence over the input ring.
      if (filteredSlash.length > 0) {
        if (e.key === 'ArrowUp') {
          e.preventDefault();
          setSelectedSlash((i) => (i <= 0 ? filteredSlash.length - 1 : i - 1));
          return;
        }
        if (e.key === 'ArrowDown') {
          e.preventDefault();
          setSelectedSlash((i) => (i + 1) % filteredSlash.length);
          return;
        }
      }
      const recall = (text: string) => {
        e.preventDefault();
        // Re-pressing Up at the oldest entry returns the same text; skip the
        // no-op setState so the caret effect always runs (a bailed-out render
        // would strand `pendingCaret`).
        if (text === composer) return;
        setComposer(text);
        setShowSlashHints(false);
        pendingCaret.current = text.length;
      };
      // Unmodified Up/Down walk the input ring like the TUI — but only when the
      // caret is on the composer's edge line, so multi-line drafts keep native
      // cursor movement. A no-op recall (empty ring, or a non-empty fresh draft)
      // falls through to the browser default.
      const bareArrow = !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey;
      if (e.key === 'ArrowUp' && bareArrow) {
        const ta = e.currentTarget;
        const caretOnFirstLine = !composer.slice(0, ta.selectionStart).includes('\n');
        if (!caretOnFirstLine) return;
        const recalled = inputHistory.recallPrev(composer.length === 0);
        if (recalled !== null) recall(recalled);
        return;
      }
      if (e.key === 'ArrowDown' && bareArrow) {
        const ta = e.currentTarget;
        const caretOnLastLine = !composer.slice(ta.selectionEnd).includes('\n');
        if (!caretOnLastLine) return;
        const recalled = inputHistory.recallNext();
        if (recalled !== null) recall(recalled);
        return;
      }
      // Any other caret move or selection change (Left/Right, Home/End, a
      // modified arrow) exits history navigation, matching the TUI's reset on
      // every non-history action — so a later Down can't jump to a stale entry.
      if (
        e.key === 'ArrowUp' ||
        e.key === 'ArrowDown' ||
        e.key === 'ArrowLeft' ||
        e.key === 'ArrowRight' ||
        e.key === 'Home' ||
        e.key === 'End'
      ) {
        inputHistory.reset();
      }
    },
    [composer, attachments, inputHistory, filteredSlash, selectedSlash, completeSlash],
  );

  // Slash hints are open only while the draft is a `/command` AND the caret is
  // still on that token — so editing args closes the popup (and a caret move
  // back onto the token via onSelect reopens it). React bails out of the
  // setState when the boolean is unchanged, so onSelect is cheap.
  const refreshSlashHints = useCallback(
    (value: string, caret: number) => {
      setShowSlashHints(slashCommands.length > 0 && caretOnSlashToken(value, caret));
    },
    [slashCommands.length],
  );

  const handleComposerChange = useCallback(
    (value: string, caret: number = value.length) => {
      setComposer(value);
      refreshSlashHints(value, caret);
      // A refiltered list invalidates the old highlight — start at the top.
      setSelectedSlash(0);
      // Any edit leaves history-navigation mode, like the TUI.
      inputHistory.reset();
    },
    [refreshSlashHints, inputHistory],
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

  // Pin / unpin a cron GROUP. The bit lives on the JOB (`PUT /v1/cron/{id}/pin`),
  // not on any session — the group is a view over the job's fires — so the
  // optimistic flip has to touch every member row, which is what carries
  // `cron_group_pinned` into the bucketing. The server answers with a
  // session-less `Gap` (list-stale) rather than a SessionPatch, since no session
  // changed; other tabs converge on their next list pull.
  const handleToggleCronPin = useCallback(
    async (jobId: string, pinned: boolean) => {
      const flip = (want: boolean) =>
        setSessions((prev) =>
          prev.map((s) =>
            s.cron_job_id === jobId ? { ...s, cron_group_pinned: want } : s,
          ),
        );
      flip(pinned);
      const { error, response } = await client.PUT('/v1/cron/{id}/pin', {
        params: { path: { id: jobId } },
        body: { pinned },
      });
      if (error || !response.ok) {
        console.warn('toggle cron group pin failed', jobId, error);
        flip(!pinned);
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
        const { data } = await client.POST('/v1/chat/sessions', { body: {} });
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
      const { data } = await client.POST('/v1/chat/sessions', { body: {} });
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
        onToggleCronPin={handleToggleCronPin}
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
        <header className="h-12 px-4 border-b-2 border-black flex items-center justify-between gap-3 bg-canvas">
          <div className="flex items-center gap-2 min-w-0 flex-1">
            {sessionId ? (
              <div className="flex flex-col min-w-0 leading-tight">
                {activeTitle ? (
                  <span
                    className="font-bold text-sm text-ink truncate"
                    title={activeTitle}
                  >
                    {activeTitle}
                  </span>
                ) : null}
                <span
                  className="font-mono text-[11px] text-ink-soft select-all break-all truncate"
                  title={sessionId}
                >
                  <span className="select-none mr-1">
                    {activeTitle ? 'id:' : 'session id:'}
                  </span>
                  {sessionId}
                </span>
              </div>
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
                const retryId = row.failed ? row.clientMsgId : undefined;
                const nodes: React.ReactNode[] = [
                  <MessageBubble
                    key={row.key}
                    row={row}
                    adminToken={adminToken}
                    baseUrl={baseUrl}
                    onRetry={
                      retryId !== undefined && sessionId
                        ? () => retryFailedSend(sessionId, retryId)
                        : undefined
                    }
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
                  adminToken={adminToken}
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
                adminToken={adminToken}
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
                  {filteredSlash.map((s, i) => {
                    const active = i === Math.min(selectedSlash, filteredSlash.length - 1);
                    return (
                      <button
                        key={s.command}
                        type="button"
                        onMouseEnter={() => setSelectedSlash(i)}
                        // Keep focus in the textarea; the click handler completes.
                        onMouseDown={(e) => e.preventDefault()}
                        onClick={() => completeSlash(i)}
                        className={`text-left px-2 py-1.5 border-2 rounded font-mono text-sm flex items-center gap-2 cursor-pointer ${
                          active
                            ? 'border-black bg-canvas'
                            : 'border-transparent hover:border-black hover:bg-canvas'
                        }`}
                      >
                        <span className="font-bold shrink-0">/{s.command}</span>
                        <span className="text-ink-soft truncate">{s.description}</span>
                      </button>
                    );
                  })}
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
                onChange={(e) =>
                  handleComposerChange(e.target.value, e.target.selectionStart ?? e.target.value.length)
                }
                onKeyDown={handleComposerKey}
                onMouseDown={() => inputHistory.reset()}
                // Caret moves (click/arrow) re-evaluate whether it's still on the
                // slash token, so the popup tracks the caret in both directions.
                onSelect={(e) =>
                  refreshSlashHints(
                    e.currentTarget.value,
                    e.currentTarget.selectionStart ?? e.currentTarget.value.length,
                  )
                }
                placeholder={
                  status.state === 'connected'
                    ? 'Message Baybo…  (Shift+Enter for newline)'
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
                    disabled={!adminToken || status.state !== 'connected'}
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

// ── frame routing ───────────────────────────────────────────────────

/** Update the right per-session bucket based on a frame's session_id.
 *  Always operates on the views map via setViews so background
 *  sessions accumulate frames even when not currently viewed. Unread
 *  accounting lives elsewhere — `Frame::SessionActivity` is the single
 *  source of truth for sidebar badges, fired by the gateway's
 *  dispatch observer regardless of subscription state. */
export function routeInboundFrame(
  frame: Frame,
  setViews: React.Dispatch<React.SetStateAction<Record<string, SessionView>>>,
  setSessions: React.Dispatch<React.SetStateAction<SessionSummary[]>>,
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
            transcript: appendReasoningStep(view.transcript, frame.text, view.turn?.active ?? null),
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
              view.turn?.active ?? null,
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
              approvalFromWire(frame.approval),
              view.turn?.active ?? null,
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
            transcript: pushStatusStep(view.transcript, text, view.turn?.active ?? null),
            awaitingReply: false,
          },
        };
      });
      return;
    }
    case 'task_list': {
      // Idempotent snapshot — REPLACE the session's checklist wholesale
      // (not a delta). An empty array means the plan is currently empty
      // and the checklist panel hides.
      const sid = frame.session_id;
      setViews((prev) => mergeView(prev, sid, { tasks: frame.tasks }));
      return;
    }
    case 'turn_state': {
      // Server-authoritative turn lifecycle: broadcast at every turn
      // start/end, snapshotted on every Subscribe. Recorded on the view
      // (drives the Cancelled indicator) and reconciled into the
      // transcript's trailing work block (open/elapsed-timer/close).
      // On `active` it also takes over from the optimistic
      // awaiting-reply indicator — the (possibly still empty) work
      // block is the working affordance from here.
      const sid = frame.session_id;
      const startedAt = parseEpochMs(frame.started_at);
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            turn: { active: frame.active, startedAt },
            transcript: applyTurnState(view.transcript, frame.active, startedAt),
            awaitingReply: frame.active ? false : view.awaitingReply,
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
          const transcript = closeActiveWork(view.transcript);
          if (transcript === view.transcript && !view.awaitingReply) return prev;
          return { ...prev, [sid]: { ...view, transcript, awaitingReply: false } };
        });
      }
      // Ordinal-stamped Message — the live final assistant reply carries
      // its persisted ordinal. Key it by its stable row id (`m<ordinal>`)
      // so the sync/backfill redelivery of the same row is a no-op, and
      // reconcile by `platform_msg_id` equality (the server preserves it
      // on every redelivery — the old text-match heuristics are gone).
      if (frame.ordinal !== undefined) {
        const rowKey = transcriptRowKey(sid, `m${frame.ordinal}`);
        const frameAttachments =
          (frame.attachments?.length ?? 0) > 0 ? frame.attachments : undefined;
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
          if (view.transcript.some((r) => r.key === rowKey)) return prev;
          if (frame.platform_msg_id) {
            const idx = view.transcript.findIndex(
              (r) => r.clientMsgId === frame.platform_msg_id,
            );
            if (idx >= 0) {
              // The optimistic row this ordinal-stamped delivery confirms
              // — adopt the server identity in place.
              const next = view.transcript.slice();
              next[idx] = {
                ...next[idx],
                key: rowKey,
                role,
                text: frame.content,
                pending: false,
                failed: false,
              };
              return { ...prev, [sid]: { ...view, transcript: next } };
            }
          }
          if (role === 'assistant') {
            const lastIdx = view.transcript.length - 1;
            const last = view.transcript[lastIdx];
            if (last?.streaming && last.role === 'assistant') {
              const next = view.transcript.slice();
              // Preserve the streaming row's `createdAt` (stamped at
              // first Delta) — the persisted final is the same logical
              // bubble, the user just saw it earlier.
              next[lastIdx] = {
                ...last,
                key: rowKey,
                role: 'assistant',
                text: frame.content,
                streaming: false,
                hasAttachments: frameAttachments !== undefined || last.hasAttachments,
                attachments: frameAttachments ?? last.attachments,
              };
              return { ...prev, [sid]: { ...view, transcript: next } };
            }
          }
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: [
                ...view.transcript,
                {
                  key: rowKey,
                  role,
                  text: frame.content,
                  hasAttachments: frameAttachments !== undefined || undefined,
                  attachments: frameAttachments,
                  clientMsgId: frame.platform_msg_id || undefined,
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
          // Match by the idempotency key alone (not `pending`): a re-echo
          // after a blind resend, or an echo landing on a row a sync page
          // already delivered/adopted, must update that row in place
          // rather than appending a duplicate bubble.
          const idx = view.transcript.findIndex((r) => r.clientMsgId === clientMsgId);
          if (idx >= 0) {
            const next = view.transcript.slice();
            next[idx] = {
              ...view.transcript[idx],
              text: frame.content,
              pending: false,
              failed: false,
              hasAttachments: hasAttachments || next[idx].hasAttachments,
            };
            return { ...prev, [sid]: { ...view, transcript: next } };
          }
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: finalizeMessage(view.transcript, role, frame.content, hasAttachments, frame.attachments),
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
      // A transient notice is the progress observer's mid-turn narration,
      // NOT the turn's reply: fold it into the open work block as a status
      // step and leave the turn running, exactly like the `status`
      // (compaction) path. Treating it as terminal here is what split one
      // long turn into two `Worked Xs` blocks — the observer collapsed the
      // block, then later tool activity opened a fresh one.
      if (frame.transient) {
        setViews((prev) => {
          const view = prev[sid] ?? EMPTY_VIEW;
          return {
            ...prev,
            [sid]: {
              ...view,
              transcript: pushStatusStep(view.transcript, frame.text, view.turn?.active ?? null),
              awaitingReply: false,
            },
          };
        });
        return;
      }
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        // A notice is terminal for the turn (slash-command reply,
        // refusal, compaction confirmation, …) — close any open work
        // block so it collapses above the notice instead of dangling.
        // When the notice is a `/stop` that actually cancelled the reply,
        // label that block "Cancelled" — this is the path EVERY tab takes
        // (the notice is broadcast), so an observer agrees with the
        // originator (which marked it optimistically) and with a reload.
        // `markLast` also covers the case where `turn_state{inactive}`
        // already closed the block to "Worked" a moment earlier.
        const isStopCancel = isStopCancellationNotice(frame.text);
        // On a cancelling /stop, keep the in-progress reply as its own bubble
        // (finalizeTrailingAnswer) below the collapsed, "Cancelled"-labelled
        // work block — mirroring the REST reload path — rather than folding it
        // into the block.
        const closed = closeActiveWork(
          isStopCancel ? finalizeTrailingAnswer(view.transcript) : view.transcript,
        );
        const base = isStopCancel ? markLastWorkCancelled(closed) : closed;
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
                createdAt: new Date().toISOString(),
              },
            ],
            // Some turns reply with `AgentOutput::Notice` and never
            // emit a Delta/Message — slash commands like `/compact`,
            // refusal / error paths, etc. Without this, the working
            // indicator would hang forever for those sends. The notice
            // itself is now the reply, so awaitingReply ends here.
            awaitingReply: false,
            // The terminal notice ends the turn locally — so a frame that
            // lands after it (a tool finishing post-`/stop`, a paced flush)
            // folds into the now-closed block via `ensureWork(active:false)`
            // instead of opening a fresh ticking block below the notice. The
            // authoritative `turn_state{active:false}` confirms this moments
            // later; setting it here just closes the race window.
            turn: { active: false, startedAt: null },
          },
        };
      });
      return;
    }
    case 'approval_requested': {
      const sid = frame.session_id;
      setViews((prev) => {
        const view = prev[sid] ?? EMPTY_VIEW;
        return {
          ...prev,
          [sid]: {
            ...view,
            transcript: frame.tool_call_id
              ? markStepAwaitingApproval(view.transcript, frame.tool_call_id, frame.call_id)
              : view.transcript,
            pendingApproval: {
              callId: frame.call_id,
              sessionId: sid,
              tool: frame.tool,
              description: frame.description ?? null,
              paramsPreview: frame.params_preview,
              accesses: frame.accesses,
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
    case 'approval_resolved': {
      setViews((prev) => {
        // Walk every session bucket since we don't know which one
        // the call_id belongs to. Map is small (~tabs visited), so
        // this is cheap. Return `prev` unchanged when no card matches
        // — the call_id may belong to an already-resolved session, and
        // a fresh object would force every SessionRow to re-render.
        let next: Record<string, SessionView> | null = null;
        for (const [sid, view] of Object.entries(prev)) {
          const transcript = resolveStepApproval(view.transcript, frame.call_id, frame.decision);
          const card = view.pendingApproval?.callId === frame.call_id;
          if (!card && transcript === view.transcript) continue;
          next ??= { ...prev };
          next[sid] = {
            ...view,
            transcript,
            pendingApproval: card ? null : view.pendingApproval,
          };
        }
        return next ?? prev;
      });
      return;
    }
    default:
      // history_snapshot / start_bot / stop_bot / slash_manifest /
      // subscribe / unsubscribe / register / register_ack are not
      // expected on the web client (the SDK strips most of them
      // before they reach onFrame; the rest are debug noise).
      // `subscribe_state` / `gap` are handled upstream in the WS
      // onFrame closure — they need the sync loop, not the views map.
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
// The answer always streams into a standalone assistant bubble — even when a
// work block is open above it — so the reply types out *below* the working
// card, not inside it. If a progress frame (reasoning / tool / status)
// interrupts the stream, `ensureWork` folds the bubble-so-far into the block
// as an intermediate `prose` step; the terminal `Frame::Message` finalizes the
// bubble in place. A still-open block with no steps yet (a turn that opened the
// "Working" affordance but produced no reasoning/tools) is dropped as the
// answer starts — the streaming bubble is itself the activity signal, so an
// empty card must not hover above it.
function writeStreamingAnswer(prev: TranscriptRow[], text: string): TranscriptRow[] {
  const last = prev[prev.length - 1];
  if (last?.streaming && last.role === 'assistant') {
    if (last.text === text) return prev;
    return [...prev.slice(0, -1), { ...last, text }];
  }
  const base =
    last?.kind === 'work' && last.workActive && (last.steps?.length ?? 0) === 0
      ? prev.slice(0, -1)
      : prev;
  return [
    ...base,
    {
      key: `stream-${base.length}-${Date.now()}`,
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

/** The single source of truth for the empty/active work-block row shape.
 *  Both `ensureWork` (live progress) and `applyTurnState` (turn-state
 *  reconciliation) open blocks through here so a schema change lands in
 *  one place. `position` only disambiguates the React key from sibling
 *  blocks already in the transcript. */
function newWorkRow(startedAt: number, position: number): TranscriptRow {
  return {
    key: `work-${startedAt}-${position}`,
    role: 'system',
    text: '',
    kind: 'work',
    steps: [],
    workActive: true,
    workStartedAt: startedAt,
  };
}

/** Parse a server ISO timestamp to epoch ms, mapping both absent and
 *  unparseable inputs to `null` — so `Date.parse`'s `NaN` failure
 *  sentinel never leaks into elapsed-timer math, React keys, or the
 *  `===` identity checks that drive idempotency. */
function parseEpochMs(iso: string | null | undefined): number | null {
  if (!iso) return null;
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? null : ms;
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
function ensureWork(
  prev: TranscriptRow[],
  turnActive: boolean | null,
): { rows: TranscriptRow[]; idx: number } {
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
  if (tail?.kind === 'work' && (tail.workActive || turnActive === false)) {
    // Reuse the trailing work block: an active one (a live turn), OR —
    // when the server says the turn already ended — its just-closed block,
    // so a late trailing frame (e.g. a tool call that completed after a
    // `/stop` cancel) folds into that turn's collapsed block instead of
    // spawning a perpetual "Working" box that no turn-end frame will close.
    idx = rows.length - 1;
  } else {
    // No reusable block. Open one — pre-closed when the server says the
    // turn already ended, so a late frame renders as a collapsed summary
    // rather than a ticking box anchored to its receive time.
    const fresh = newWorkRow(Date.now(), rows.length);
    rows = [
      ...rows,
      turnActive === false ? { ...fresh, workActive: false, workEndedAt: Date.now() } : fresh,
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
function pushWorkStep(
  prev: TranscriptRow[],
  step: WorkStep,
  turnActive: boolean | null,
): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev, turnActive);
  const block = rows[idx];
  const next = rows.slice();
  next[idx] = { ...block, steps: [...(block.steps ?? []), step] };
  return next;
}

/** Append a reasoning chunk, merging into a trailing reasoning step so
 *  the streamed thinking reads as one paragraph. */
function appendReasoningStep(
  prev: TranscriptRow[],
  text: string,
  turnActive: boolean | null,
): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev, turnActive);
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
export function pushToolStartedStep(
  prev: TranscriptRow[],
  callId: string,
  tool: string,
  label: string | null,
  turnActive: boolean | null,
): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev, turnActive);
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
export function applyToolCompletedStep(
  prev: TranscriptRow[],
  callId: string,
  status: string,
  summary: string,
  approval: WorkStep['approval'],
  turnActive: boolean | null,
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
    nextSteps[sIdx] = {
      ...nextSteps[sIdx],
      toolStatus,
      toolSummary: summary,
      // The call is done, so nothing waits on the user any more — including
      // when the gate TIMED OUT, which broadcasts no `approval_resolved` and
      // would otherwise strand the badge.
      awaitingApproval: undefined,
      approval: approval ?? nextSteps[sIdx].approval,
    };
    const next = prev.slice();
    next[i] = { ...row, steps: nextSteps };
    return next;
  }
  return pushWorkStep(
    prev,
    {
      key: `tool-${callId}`,
      kind: 'tool',
      toolCallId: callId,
      tool: 'tool',
      toolStatus,
      toolSummary: summary,
      approval,
    },
    turnActive,
  );
}

/** Narrow a wire decision string onto the step model. Anything unknown (a
 *  newer server) is dropped rather than rendered raw. */
function approvalFromWire(decision: string | undefined): WorkStep['approval'] {
  return decision === 'approve' || decision === 'approve_always' || decision === 'deny'
    ? decision
    : undefined;
}

/** Badge the work step of the TOOL call a prompt just blocked. Never opens a
 *  block: an approval frame is not work, and a prompt always follows its
 *  call's `tool_started`. */
export function markStepAwaitingApproval(
  prev: TranscriptRow[],
  toolCallId: string,
  promptId: string,
): TranscriptRow[] {
  return rewriteToolStep(prev, (s) => (s.toolCallId === toolCallId ? { ...s, awaitingApproval: promptId } : s));
}

/** A prompt was answered (here or on another client): clear the badge and
 *  label the step. Matched by PROMPT id — that is all `approval_resolved`
 *  carries. The label is provisional until the call's own completion brings
 *  the persisted twin. */
export function resolveStepApproval(
  prev: TranscriptRow[],
  promptId: string,
  decision: string,
): TranscriptRow[] {
  return rewriteToolStep(prev, (s) =>
    s.awaitingApproval === promptId
      ? { ...s, awaitingApproval: undefined, approval: approvalFromWire(decision) }
      : s,
  );
}

/** Rewrite every tool step of the newest work block in place. Returns `prev`
 *  UNCHANGED when nothing matched — `approval_resolved` is broadcast to every
 *  session bucket, and minting a fresh array per bucket would re-render every
 *  open thread on a decision that belongs to exactly one of them. */
function rewriteToolStep(
  prev: TranscriptRow[],
  mutate: (step: WorkStep) => WorkStep,
): TranscriptRow[] {
  for (let i = prev.length - 1; i >= 0; i--) {
    const row = prev[i];
    if (row.kind !== 'work') continue;
    const steps = row.steps ?? [];
    const nextSteps = steps.map((s) => (s.kind === 'tool' ? mutate(s) : s));
    if (nextSteps.every((s, idx) => s === steps[idx])) return prev;
    const next = prev.slice();
    next[i] = { ...row, steps: nextSteps };
    return next;
  }
  return prev;
}

/** Push a status step (compaction, …) into the turn's open work block. */
function pushStatusStep(
  prev: TranscriptRow[],
  text: string,
  turnActive: boolean | null,
): TranscriptRow[] {
  const { rows, idx } = ensureWork(prev, turnActive);
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
 *  dropped entirely so no summary line / arrow appears. Pass
 *  `cancelled = true` to label it "Cancelled" — used by the optimistic
 *  `/stop` path, where the user's own action is proof the turn was cancelled
 *  (so the block flips instantly, without waiting on a backend signal). */
export function closeActiveWork(prev: TranscriptRow[], cancelled = false): TranscriptRow[] {
  // The turn is ending, so any block left "settling" by a mid-turn interjection
  // (kept expanded so its split-off work stayed visible) now collapses to its
  // summary too. Clear the flag here — co-located with the active-block close so
  // every turn-end path collapses settling blocks without a separate sweep.
  const hasSettling = prev.some((r) => r.kind === 'work' && r.workSettling);
  const rows = hasSettling
    ? prev.map((r) => (r.kind === 'work' && r.workSettling ? { ...r, workSettling: false } : r))
    : prev;
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i];
    if (row.kind !== 'work' || !row.workActive) continue;
    if (!row.steps || row.steps.length === 0) {
      return [...rows.slice(0, i), ...rows.slice(i + 1)];
    }
    const next = rows.slice();
    next[i] = {
      ...row,
      workActive: false,
      workEndedAt: Date.now(),
      workCancelled: row.workCancelled || cancelled,
    };
    return next;
  }
  return rows;
}

/** Close the open work block on a mid-turn user interjection: relabel it
 *  "Worked Xs" (clear `workActive`) but keep it EXPANDED via `workSettling`
 *  until the turn fully ends, so the work the interjection split off stays
 *  visible instead of collapsing mid-reply. An empty block (no steps) is still
 *  dropped — nothing to keep open. `closeActiveWork` clears the flag (→
 *  collapse) at turn-end. */
export function settleActiveWork(prev: TranscriptRow[]): TranscriptRow[] {
  for (let i = prev.length - 1; i >= 0; i--) {
    const row = prev[i];
    if (row.kind !== 'work' || !row.workActive) continue;
    if (!row.steps || row.steps.length === 0) {
      return [...prev.slice(0, i), ...prev.slice(i + 1)];
    }
    const next = prev.slice();
    next[i] = {
      ...row,
      workActive: false,
      workSettling: true,
      workEndedAt: Date.now(),
    };
    return next;
  }
  return prev;
}

/** Finalize a trailing streaming answer bubble on `/stop`: the cut-short
 *  reply is the agent's final output, so it stays as its own (now non-
 *  streaming) bubble below the collapsed "Cancelled" work block rather than
 *  being folded inside it. No-op unless the tail is a streaming assistant
 *  bubble; an empty partial (nothing streamed yet) is dropped so no blank
 *  bubble lingers. */
export function finalizeTrailingAnswer(prev: TranscriptRow[]): TranscriptRow[] {
  const last = prev[prev.length - 1];
  if (!last?.streaming || last.role !== 'assistant') return prev;
  if (last.text.trim().length === 0) return prev.slice(0, -1);
  return [...prev.slice(0, -1), { ...last, streaming: false }];
}

/** Whether `text` leads with any slash command, mirroring the backend's
 *  `is_slash_command` (a `/` immediately followed by a non-whitespace char).
 *  Such a message is a hard coalescing barrier on the server, so it must not
 *  ride in a "send all at once" batch frame — the drain sends these
 *  individually instead. */
export function isSlashText(text: string): boolean {
  return /^\/\S/.test(text.trim());
}

/** Recognise a `/stop` the user typed, mirroring the gateway's parser
 *  (leading `/`, first token, tolerant of a `@bot` suffix / args) so the
 *  client can optimistically reflect the cancel before the round-trip. */
export function isStopCommand(text: string): boolean {
  const trimmed = text.trim();
  if (!trimmed.startsWith('/')) return false;
  const cmd = trimmed.slice(1).split(/[\s@]/, 1)[0]?.toLowerCase();
  return cmd === 'stop';
}

/** Stable substring of `baybo-channels`' `STOP_CANCELLED_REPLY_LINE` — present
 *  in a `/stop` notice ONLY when it actually cancelled an in-progress reply
 *  (a no-op stop says "Nothing in progress to stop."). Keep in sync with that
 *  const; a server-side test pins the producer text. */
const STOP_CANCELLED_NOTICE_MARKER = 'Cancelled the in-progress reply';

/** Whether a terminal notice is a `/stop` that actually cancelled a reply —
 *  so its turn's work block reads "Cancelled" on every tab, not just the one
 *  that typed `/stop` (which marks it optimistically) or after a reload. */
export function isStopCancellationNotice(text: string): boolean {
  return text.includes(STOP_CANCELLED_NOTICE_MARKER);
}

/** Mark the turn-just-stopped's work block cancelled. The block is the last
 *  row, or sits just above a salvaged trailing reply bubble (a /stop'd partial
 *  answer kept as its own bubble). Only that one block is touched and only an
 *  *assistant* tail is skipped — a newer user message (a fresh turn) is the
 *  barrier, so an earlier turn's block can't be mis-labelled. Idempotent. */
export function markLastWorkCancelled(rows: TranscriptRow[]): TranscriptRow[] {
  let i = rows.length - 1;
  const last = rows[i];
  if (last && last.kind !== 'work' && last.role === 'assistant') i -= 1;
  const block = rows[i];
  if (block?.kind !== 'work' || block.workCancelled) return rows;
  const next = rows.slice();
  next[i] = { ...block, workCancelled: true };
  return next;
}

/** Reconcile the transcript's tail with the server's `TurnState`.
 *
 *  Active (`started_at` is always present — the server asserts it iff
 *  `active`): pin an already-open block's `workStartedAt` to the server's
 *  start, re-open a *closed* block whose start matches this turn (the
 *  in-flight turn a REST reload reconstructed as collapsed `Worked Xs`),
 *  or — when the tail is no work block at all (turn started, nothing
 *  streamed to this tab yet) — open an empty one as the working
 *  affordance. A closed block with a *different* start belongs to a
 *  finished turn and is left alone (a fresh block opens instead). A null
 *  `started_at` under `active` is a stale/lossy artifact and is ignored,
 *  so a finished turn can never be resurrected as a phantom "Working" box.
 *
 *  Inactive: close any open block. This is the turn-end signal that
 *  doesn't depend on a terminal `Message`/`Notice` arriving, so a turn
 *  that ends without either (error, cancel, blank cron reply) can't
 *  leave the block spinning forever.
 *
 *  Idempotent — driven only by `turn_state` frames (the authoritative
 *  server signal); the REST history reload no longer folds a cached turn
 *  through here. */
export function applyTurnState(
  prev: TranscriptRow[],
  active: boolean,
  startedAt: number | null,
): TranscriptRow[] {
  if (!active) {
    // Turn end. Collapse the open work block to its `Worked Xs` summary. The
    // actor emits `active: false` when its loop returns, BEFORE the terminal
    // Message/Notice ships on the same ordered stream — but any answer the
    // model is still streaming lives in its own bubble *below* the block, so
    // collapsing here never swallows the reply (the imminent Message finalizes
    // that bubble in place). Also covers turns that end with no terminal frame
    // at all — error, cancel, blank cron reply.
    return closeActiveWork(prev);
  }
  // A server `active:true` ALWAYS carries a real `started_at` (the
  // gateway asserts `started_at` iff `active`). An `active:true` with a
  // null start is a stale/lossy artifact (e.g. a cached turn folded in
  // after a dropped close frame) — never fabricate or re-anchor a block
  // off it, or a finished turn resurfaces as a phantom "Working" box whose
  // elapsed counts from the wrong (old) start.
  if (startedAt === null) return prev;
  const last = prev[prev.length - 1];
  if (last?.kind === 'work' && (last.workActive || last.workStartedAt === startedAt)) {
    // Re-pin an already-open block, or re-open a *closed* block only when
    // its start matches this turn — the same in-flight turn a REST reload
    // reconstructed as collapsed. A closed block with a *different* start
    // belongs to a finished turn; falling through opens a fresh block
    // rather than resurrecting that turn's steps.
    if (last.workActive && last.workStartedAt === startedAt) return prev;
    const next = prev.slice();
    next[next.length - 1] = {
      ...last,
      workActive: true,
      workStartedAt: startedAt,
      workEndedAt: undefined,
    };
    return next;
  }
  return [...prev, newWorkRow(startedAt, prev.length)];
}

function finalizeMessage(
  prev: TranscriptRow[],
  role: 'user' | 'assistant',
  content: string,
  hasAttachments: boolean,
  attachments: WireAttachment[] = [],
): TranscriptRow[] {
  const details = attachments.length > 0 ? attachments : undefined;
  const last = prev[prev.length - 1];
  if (role === 'assistant' && last?.streaming && last.role === 'assistant') {
    return [
      ...prev.slice(0, -1),
      {
        ...last,
        text: content,
        streaming: false,
        // Live attachments stay observable on the streaming row — the
        // bubble renders the thumbnails/filenames (or the `[attachment]`
        // fallback) even when the assistant produced only media.
        hasAttachments: hasAttachments || last.hasAttachments,
        attachments: details ?? last.attachments,
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
      attachments: details,
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

/** React key for a server transcript row, derived from its stable row id
 *  (`m<ordinal>` message / `w<ordinal>` work / `n<seq>` control event) so
 *  every redelivery — sync, backfill, the ordinal-stamped live final —
 *  converges on the same node. Doubles as the redelivery dedup key. */
function transcriptRowKey(sessionId: string, rowId: string): string {
  return `row-${sessionId}-${rowId}`;
}

function attachmentFromApi(a: ApiAttachment): WireAttachment {
  const kind: WireAttachment['kind'] =
    a.kind === 'image' || a.kind === 'audio' ? a.kind : 'file';
  return {
    kind,
    blob_id: a.blob_id,
    mime_type: a.mime_type,
    size: a.size,
    filename: a.filename ?? undefined,
  };
}

/** Translate one server transcript row (the sync / get-session /
 *  backfill DTO) into the local [`TranscriptRow`] shape, keyed by the
 *  row's stable id so the same logical row from any surface reuses the
 *  same React node identity. A `work` item maps to a finished
 *  (collapsed) work block; a message row carries its `platform_msg_id`
 *  as `clientMsgId` so it reconciles with the optimistic send row. */
export function transcriptItemToRow(sessionId: string, item: ApiTranscriptItem): TranscriptRow {
  const key = transcriptRowKey(sessionId, item.id);
  if (item.kind === 'work') {
    return {
      key,
      role: 'system',
      text: '',
      kind: 'work',
      workActive: false,
      workCancelled: item.cancelled ?? false,
      workStartedAt: parseEpochMs(item.work_started_at) ?? undefined,
      workEndedAt: parseEpochMs(item.work_ended_at) ?? undefined,
      steps: (item.steps ?? []).map((s, i) => ({
        key: `${key}-${i}`,
        kind: s.kind,
        text: s.text,
        tool: s.tool ?? undefined,
        toolLabel: s.tool_label ?? null,
        // Backend sends `ok` / `error` / `denied` (or null when the result
        // didn't land); `undefined` renders neutral, matching live.
        toolStatus: (s.tool_status ?? undefined) as WorkStep['toolStatus'],
        toolSummary: s.tool_summary ?? undefined,
        // Persisted on the tool result (`ToolResultMeta::approval`), so a
        // reloaded thread still shows what the user judged.
        approval: approvalFromWire(s.approval ?? undefined),
      })),
    };
  }
  if (item.kind === 'notice') {
    // Persisted control event (slash echo / out-of-band notice), rendered
    // at the severity the live frame carried (`notice_level`).
    return {
      key,
      role: 'system',
      text: '',
      notice: { level: noticeLevel(item.notice_level ?? 'info'), text: item.text },
      createdAt: item.created_at,
    };
  }
  const attachments = (item.attachments ?? []).map(attachmentFromApi);
  return {
    key,
    role: roleFromString(item.role),
    text: item.text,
    hasAttachments: item.has_attachments,
    attachments: attachments.length > 0 ? attachments : undefined,
    createdAt: item.created_at,
    clientMsgId: item.platform_msg_id || undefined,
  };
}

/** Index of the open work block matching `startedAt`, newest-first. */
function findOpenWorkIndex(rows: TranscriptRow[], startedAt: number): number {
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i];
    if (row.kind === 'work' && row.workActive && row.workStartedAt === startedAt) return i;
  }
  return -1;
}

/** Merge a difference sync page into the rendered thread. Dedup is by
 *  the stable row key (redelivery) and by `platform_msg_id` (a
 *  redelivered user row reconciles with — and adopts the server
 *  identity of — the optimistic row that produced it). A work row for a
 *  turn this client was watching live replaces the matching open block
 *  (same start) instead of duplicating it, and a final assistant row
 *  lands in the trailing streaming bubble when the live final frame was
 *  lost. Rows arrive ascending and strictly above the cursor, so plain
 *  append preserves order. */
export function applySyncMerge(prev: TranscriptRow[], page: TranscriptRow[]): TranscriptRow[] {
  if (page.length === 0) return prev;
  const next = prev.slice();
  let changed = false;
  const keys = new Set(prev.map((r) => r.key));
  for (const row of page) {
    if (keys.has(row.key)) continue;
    keys.add(row.key);
    if (row.clientMsgId !== undefined) {
      const idx = next.findIndex((r) => r.clientMsgId === row.clientMsgId);
      if (idx >= 0) {
        const local = next[idx];
        next[idx] = {
          ...row,
          // Optimistic rows carry full attachment details; keep whichever
          // side has them.
          attachments: row.attachments ?? local.attachments,
          hasAttachments: row.hasAttachments || local.hasAttachments,
        };
        changed = true;
        continue;
      }
    }
    if (row.kind === 'work' && row.workStartedAt !== undefined) {
      const idx = findOpenWorkIndex(next, row.workStartedAt);
      if (idx >= 0) {
        // The server's reconstruction of the turn we were watching live —
        // its closed work row supersedes the open block.
        next[idx] = row;
        changed = true;
        continue;
      }
    }
    if (row.role === 'assistant' && row.kind === undefined && row.notice === undefined) {
      const last = next[next.length - 1];
      if (last?.streaming === true && last.role === 'assistant') {
        // The live final frame was lost (that's why we're syncing) — the
        // persisted final lands in the streaming bubble it finalizes.
        next[next.length - 1] = { ...row, createdAt: last.createdAt ?? row.createdAt };
        changed = true;
        continue;
      }
    }
    next.push(row);
    changed = true;
  }
  return changed ? next : prev;
}

/** REPLACE the thread with a baseline / rebased page, then re-overlay
 *  the local state the page cannot carry:
 *  * optimistic send rows whose `platform_msg_id` is still in the outbox
 *    and absent from the page — their content lives only client-side
 *    until durability confirms (the REPLACE-overlay rule);
 *  * the in-flight turn's open work block — re-opened from the page's
 *    own partial fold when the starts match (`applyTurnState`), else
 *    carried over from the previous thread;
 *  * a trailing streaming answer bubble (the pacer keeps writing it). */
export function applySyncReplace(
  prev: TranscriptRow[],
  page: TranscriptRow[],
  unconfirmedSendIds: ReadonlySet<string>,
  turn: SessionView['turn'],
): TranscriptRow[] {
  const pageSendIds = new Set<string>();
  for (const row of page) {
    if (row.clientMsgId !== undefined) pageSendIds.add(row.clientMsgId);
  }
  const keptSends = prev.filter(
    (r) =>
      r.clientMsgId !== undefined &&
      unconfirmedSendIds.has(r.clientMsgId) &&
      !pageSendIds.has(r.clientMsgId),
  );
  let rows = [...page, ...keptSends];
  if (turn?.active && turn.startedAt !== null) {
    let inherited: WorkStep[] | undefined;
    for (let i = prev.length - 1; i >= 0; i--) {
      const row = prev[i];
      if (row.kind === 'work' && row.workActive) {
        inherited = row.steps;
        break;
      }
    }
    rows = applyTurnState(rows, true, turn.startedAt);
    const tail = rows[rows.length - 1];
    if (
      tail?.kind === 'work' &&
      tail.workActive &&
      (tail.steps?.length ?? 0) === 0 &&
      inherited !== undefined &&
      inherited.length > 0
    ) {
      // applyTurnState opened a fresh empty block (the page carried no
      // fold for this turn) — keep the steps we already rendered live.
      rows = [...rows.slice(0, -1), { ...tail, steps: inherited }];
    }
  }
  const last = prev[prev.length - 1];
  if (last?.streaming === true && last.role === 'assistant') rows = [...rows, last];
  return rows;
}

function approvalFromCard(sessionId: string, card: WireApprovalCard): PendingApproval {
  return {
    callId: card.call_id,
    sessionId,
    tool: card.tool,
    description: card.description ?? null,
    paramsPreview: card.params_preview,
    accesses: card.accesses,
  };
}

function workStepFromWire(step: WireWorkStep, i: number): WorkStep {
  if (step.kind === 'tool') {
    return {
      // Same key shape as the live `tool_started` path so a later live
      // `tool_completed` pairs with the snapshot step by call id.
      key: `tool-${step.call_id ?? `snap-${i}`}`,
      kind: 'tool',
      toolCallId: step.call_id,
      tool: step.tool ?? 'tool',
      toolLabel: step.label ?? null,
      toolStatus:
        step.status === 'error'
          ? 'error'
          : step.status === 'denied'
            ? 'denied'
            : step.status === 'ok'
              ? 'ok'
              : 'running',
      toolSummary: step.summary,
      approval: approvalFromWire(step.approval),
    };
  }
  return { key: `snap-${i}-${step.kind}`, kind: step.kind, text: step.text };
}

/** REPLACE the steps of the transcript's open work block wholesale (the
 *  `subscribe_state` work-steps half — the shape the retired on-subscribe
 *  WorkSnapshot apply had). No-op when no block is open. */
function replaceOpenWorkSteps(prev: TranscriptRow[], steps: WorkStep[]): TranscriptRow[] {
  for (let i = prev.length - 1; i >= 0; i--) {
    const row = prev[i];
    if (row.kind !== 'work' || !row.workActive) continue;
    const next = prev.slice();
    next[i] = { ...row, steps };
    return next;
  }
  return prev;
}

/** Apply one `subscribe_state` bundle to a session view: REPLACE the
 *  task list and the pending-approval card set wholesale (they are
 *  latest-wins — live frames arriving after the snapshot win by normal
 *  frame order), and apply the turn/work halves unless the caller
 *  determined they are stale by turn identity (`turnEnded`: this client
 *  already holds a turn-end signal for the SAME turn, matched by
 *  `started_at` — never by ordinal arithmetic, since the coverage
 *  watermark advances mid-turn). */
export function applySubscribeState(
  view: SessionView,
  frame: Extract<Frame, { kind: 'subscribe_state' }>,
  turnEnded: boolean,
): SessionView {
  const cards = frame.pending_approvals ?? [];
  let next: SessionView = {
    ...view,
    tasks: frame.tasks ?? [],
    // The view renders one card at a time; the queue's head is the call
    // the turn is blocked on.
    pendingApproval: cards.length > 0 ? approvalFromCard(frame.session_id, cards[0]) : null,
  };
  if (turnEnded) return next;
  const startedAt = parseEpochMs(frame.turn.started_at);
  if (frame.turn.active && startedAt !== null) {
    // The bundle REPLACES the block's steps, so the awaiting badges have to be
    // re-derived from the authoritative pending set: the `approval_requested`
    // frame that first set them may predate this connection.
    const awaitingByCall = new Map(
      cards.filter((c) => c.tool_call_id).map((c) => [c.tool_call_id as string, c.call_id]),
    );
    let transcript = applyTurnState(view.transcript, true, startedAt);
    transcript = replaceOpenWorkSteps(
      transcript,
      (frame.work_steps ?? []).map(workStepFromWire).map((step) =>
        step.kind === 'tool' && step.toolCallId && awaitingByCall.has(step.toolCallId)
          ? { ...step, awaitingApproval: awaitingByCall.get(step.toolCallId) }
          : step,
      ),
    );
    next = {
      ...next,
      transcript,
      turn: { active: true, startedAt },
      awaitingReply: false,
    };
  } else if (!frame.turn.active) {
    next = {
      ...next,
      transcript: applyTurnState(view.transcript, false, null),
      turn: { active: false, startedAt: null },
    };
  }
  return next;
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
    // No cron fields here, and that is sound: only `POST /v1/chat/sessions`
    // broadcasts a row-constructing Created patch, and a cron fire is never
    // minted through it. A fire announces itself with `SessionActivity` for an
    // unknown id, which triggers the list refetch that carries its grouping.
    return [
      {
        session_id: sessionId,
        created_at: patch.created_at,
        last_active: patch.last_active,
        unread: 0,
        pinned: patch.pinned ?? false,
        folder_id: patchedFolder,
        title: patch.title,
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
    title: patch.title ?? current.title,
    // Cron grouping is read off the session's trigger and never changes, so no
    // patch carries it — but it must survive the merge, or a title/pin patch
    // would silently drop the row out of its cron group.
    cron_job_id: current.cron_job_id,
    cron_job_title: current.cron_job_title,
  };
  if (
    merged.created_at === current.created_at &&
    merged.last_active === current.last_active &&
    merged.pinned === current.pinned &&
    merged.folder_id === current.folder_id &&
    merged.title === current.title
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
  adminToken,
}: {
  attachments: WireAttachment[];
  baseUrl: string;
  adminToken: string | null;
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
            adminToken={adminToken}
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
  adminToken,
  baseUrl,
  onRetry,
}: {
  row: TranscriptRow;
  adminToken: string | null;
  baseUrl: string;
  /** Set on a `failed` send row: fires the manual outbox retry (same
   *  platform_msg_id, transmission budget reset). */
  onRetry?: () => void;
}) {
  if (row.kind === 'work') {
    return <WorkBlock row={row} />;
  }
  if (row.notice) {
    // The icon + colored left rail + soft tint carry the severity, so the body
    // text stays readable `ink` rather than the whole line being tinted (which
    // got garish on longer error messages). Rail mirrors the brutalist accent
    // used elsewhere in the chat.
    const { Icon, accent, tint, ring, iconColor } =
      row.notice.level === 'error'
        ? {
            Icon: RiErrorWarningLine,
            accent: 'bg-err',
            tint: 'bg-err/10',
            ring: 'border-err/30',
            iconColor: 'text-err',
          }
        : row.notice.level === 'warn'
          ? {
              Icon: RiAlertLine,
              accent: 'bg-warn',
              tint: 'bg-warn/10',
              ring: 'border-warn/30',
              iconColor: 'text-warn',
            }
          : {
              Icon: RiInformation2Line,
              accent: 'bg-info',
              tint: 'bg-info/10',
              ring: 'border-info/25',
              iconColor: 'text-info',
            };
    return (
      <div className="flex flex-col items-start min-w-0">
        <div className="flex flex-col w-fit max-w-full min-w-0">
          <div
            className={`relative w-fit max-w-full overflow-hidden rounded-md border ${ring} ${tint}`}
          >
            <span className={`absolute left-0 top-0 bottom-0 w-1 ${accent}`} aria-hidden />
            <div className="flex items-start gap-2 pl-3.5 pr-3 py-2">
              <Icon className={`${iconColor} text-base shrink-0 mt-[0.15rem]`} aria-hidden />
              <span className="font-mono text-sm text-ink/90 whitespace-pre-wrap break-words [overflow-wrap:anywhere] leading-snug">
                {row.notice.text}
              </span>
            </div>
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
                  adminToken={adminToken}
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
          {row.failed ? (
            <button
              type="button"
              onClick={onRetry}
              disabled={!onRetry}
              className="absolute -bottom-1.5 -right-1.5 flex items-center justify-center bg-white text-err rounded-full border-2 border-err cursor-pointer disabled:cursor-not-allowed"
              title="Send failed — click to retry"
              aria-label="Retry send"
            >
              <RiErrorWarningLine className="text-sm" />
            </button>
          ) : row.pending ? (
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
  const approvalLabel = step.awaitingApproval
    ? 'waiting for approval'
    : step.approval === 'approve'
      ? 'approved'
      : step.approval === 'approve_always'
        ? 'always approved'
        : step.approval === 'deny'
          ? 'denied'
          : null;
  return (
    <div className="flex flex-col gap-0.5 font-mono text-xs">
      <div className="flex items-center gap-1.5">
        <span className="text-info">⏺</span>
        <span className="font-bold text-ink">{step.tool}</span>
        {step.toolLabel ? <span className="text-ink-soft">({step.toolLabel})</span> : null}
        {/* A call parked on the approval card is NOT running — no spinner, or it
            would read as work in progress while nothing executes. */}
        {step.toolStatus === 'running' && !step.awaitingApproval ? (
          <RiLoader4Line className="text-ink-soft animate-spin" title="Running…" />
        ) : null}
        {approvalLabel ? (
          <span
            className={`rounded border px-1 ${
              step.awaitingApproval ? 'border-ink text-ink' : 'border-line text-ink-soft'
            }`}
          >
            {approvalLabel}
          </span>
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

/** The WorkBlock's two display flags, derived from turn/step/expand state.
 *  Pure + exported so the "spinner first, expand on the first step" contract
 *  is unit-testable without rendering:
 *   • `boxed`     — draw the bordered card (live turn, or a re-expanded
 *     finished block).
 *   • `panelOpen` — reveal the steps panel (only once a live turn has a step,
 *     or the user expanded a finished block). */
export function workBlockDisplay(
  active: boolean,
  hasSteps: boolean,
  expanded: boolean,
  settling = false,
): { boxed: boolean; panelOpen: boolean } {
  return {
    // `settling` (an interjection-paused block, still mid-turn) reads like an
    // expanded finished block: bordered card with its steps panel open.
    boxed: active || expanded || settling,
    panelOpen: (active && hasSteps) || expanded || (settling && hasSteps),
  };
}

/** Collapsed-summary label for a finished work block. A sub-second turn drops
 *  the duration (never "Worked 0s"); a cancelled turn (`/stop`) is labelled
 *  "Cancelled" so it reads distinctly from a turn that ran to completion. */
export function formatWorkedLabel(secs: number, cancelled = false): string {
  const worked = secs >= 1 ? `Worked ${secs}s` : 'Worked';
  return cancelled ? (secs >= 1 ? `Cancelled · ${worked}` : 'Cancelled') : worked;
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
