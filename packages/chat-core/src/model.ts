// Normalized view-model for a chat session: the transcript rows, work
// blocks, pending approval, and per-session turn/history state the UI
// renders. Framework-agnostic — built from wire `Frame`s by `./frames`
// and `./transcript`, consumed by each app's own React components.
import type { ResourceAccess, TaskView, WireAttachment } from './chatWs';

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

export interface PendingApproval {
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
   *  pinned `aura.json` entry name. Seeded from the GET-session
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
