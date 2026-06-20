// Pure, framework-agnostic transcript + work-block builders. Each takes a
// `TranscriptRow[]` (or the views map, for `mergeView`) and returns the next
// immutable value — no React, no DOM. Shared by `./frames` (live frame
// routing) and each app's history-load + render code.
import type { WireAttachment } from './chatWs';
import { EMPTY_VIEW, type SessionView, type TranscriptRow, type WorkStep } from './model';

export function mergeView(
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
export function writeStreamingAnswer(prev: TranscriptRow[], text: string): TranscriptRow[] {
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

export function appendStreamingDelta(prev: TranscriptRow[], text: string): TranscriptRow[] {
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
export function parseEpochMs(iso: string | null | undefined): number | null {
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
export function appendReasoningStep(
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
    nextSteps[sIdx] = { ...nextSteps[sIdx], toolStatus, toolSummary: summary };
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
    },
    turnActive,
  );
}

/** Push a status step (compaction, …) into the turn's open work block. */
export function pushStatusStep(
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
      workEndedAt: Date.now(),
      workCancelled: row.workCancelled || cancelled,
    };
    return next;
  }
  return prev;
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

/** Stable substring of `aura-channels`' `STOP_CANCELLED_REPLY_LINE` — present
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

/** Mark the transcript's trailing work block cancelled. Only the *last* row is
 *  touched, so a cancelled turn whose own block was dropped (no steps) can't
 *  mis-label an earlier turn's block. Idempotent. */
export function markLastWorkCancelled(rows: TranscriptRow[]): TranscriptRow[] {
  const last = rows[rows.length - 1];
  if (last?.kind !== 'work' || last.workCancelled) return rows;
  const next = rows.slice();
  next[next.length - 1] = { ...last, workCancelled: true };
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
    // The actor emits `active: false` when its loop returns, BEFORE the
    // terminal Message/Notice ships on the same ordered stream. A block
    // whose tail is `prose` is holding the streamed answer that the
    // imminent Message will peel into the real bubble
    // (`closeWorkForFinalReply`) — closing it here would fossilise that
    // prose inside the collapsed block and the answer would render
    // twice. Leave it for the terminal frame; close everything else
    // (tool/reasoning tails, turns that end with no terminal frame at
    // all — cancel, blank cron reply). A *failed* user turn now always
    // gets a terminal error notice (see `run_agent_loop`), whose handler
    // closes the block via `closeActiveWork` — so the prose-tail block
    // doesn't dangle on the error path either.
    const last = prev[prev.length - 1];
    if (last?.kind === 'work' && last.workActive) {
      const steps = last.steps ?? [];
      if (steps[steps.length - 1]?.kind === 'prose') return prev;
    }
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

/** Close the open work block at the turn's terminal reply, peeling off a
 *  trailing `prose` step — that's the final answer the model streamed
 *  into the block — so the caller renders it as the standalone answer
 *  bubble below (from the authoritative `Frame::Message` content, so the
 *  peeled step's own text is just discarded). If peeling empties the
 *  block (a direct answer that only opened a block to stream into, e.g.
 *  reasoning-less), the block is dropped so no `Worked Xs` line shows. */
export function closeWorkForFinalReply(prev: TranscriptRow[]): TranscriptRow[] {
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

export function finalizeMessage(
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

export function noticeLevel(level: string): 'info' | 'warn' | 'error' {
  if (level === 'error') return 'error';
  if (level === 'info') return 'info';
  return 'warn';
}

function roleFromString(role: string): 'user' | 'assistant' | 'system' {
  if (role === 'user') return 'user';
  if (role === 'system') return 'system';
  return 'assistant';
}

export interface HistoryWorkStepDto {
  kind: 'reasoning' | 'prose' | 'tool';
  text?: string;
  tool?: string | null;
  tool_label?: string | null;
  tool_status?: string | null;
  tool_summary?: string | null;
}

export interface HistoryRowDto {
  created_at: string;
  ordinal: number;
  /** `'message'` (default), `'work'`, or `'notice'` — see the gateway's
   *  `ChatTranscriptItem`. A `work` row is the server's reconstruction of
   *  a tool-using turn's collapsed work block; a `notice` row is a persisted
   *  out-of-band notice (e.g. a `/compact` confirmation). */
  kind?: 'message' | 'work' | 'notice';
  role: string;
  text: string;
  has_attachments: boolean;
  steps?: HistoryWorkStepDto[];
  work_started_at?: string | null;
  work_ended_at?: string | null;
  /** True when a `work` row's turn was cancelled (`/stop`); the block then
   *  collapses to a "Cancelled · Worked Xs" summary. */
  cancelled?: boolean;
  /** Severity of a `notice` row, so reload colors it like the live frame.
   *  Normalized through `noticeLevel()` at the call site. */
  notice_level?: string | null;
}

/** Translate one server-side transcript row into the local
 *  [`TranscriptRow`] shape, keying on the absolute ordinal so the
 *  same logical message coming back from another page-fetch (or a
 *  hot-reload during dev) reuses the same React node identity. A
 *  `work` row maps to a finished (collapsed) work block, rendered by
 *  the same `WorkBlock` the live path produces. */
export function historyRowToTranscript(sessionId: string, row: HistoryRowDto): TranscriptRow {
  if (row.kind === 'work') {
    return {
      // A direct-answer turn's work block shares its ordinal with the answer
      // bubble (both reconstructed from the same row), so suffix the key to
      // keep React identities distinct and let the answer's ordinal-keyed
      // replay dedup (`hist-<sid>-<ordinal>`) match the bubble, not this block.
      key: `hist-${sessionId}-${row.ordinal}-work`,
      role: 'system',
      text: '',
      kind: 'work',
      workActive: false,
      workCancelled: row.cancelled ?? false,
      workStartedAt: parseEpochMs(row.work_started_at) ?? undefined,
      workEndedAt: parseEpochMs(row.work_ended_at) ?? undefined,
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
  if (row.kind === 'notice') {
    // Persisted out-of-band notice (e.g. a `/compact` confirmation), rendered at
    // the same severity the live frame carried (`notice_level`).
    return {
      key: `hist-${sessionId}-${row.ordinal}`,
      role: 'system',
      text: '',
      notice: { level: noticeLevel(row.notice_level ?? 'info'), text: row.text },
      createdAt: row.created_at,
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
): { boxed: boolean; panelOpen: boolean } {
  return {
    boxed: active || expanded,
    panelOpen: (active && hasSteps) || expanded,
  };
}

/** Collapsed-summary label for a finished work block. A sub-second turn drops
 *  the duration (never "Worked 0s"); a cancelled turn (`/stop`) is labelled
 *  "Cancelled" so it reads distinctly from a turn that ran to completion. */
export function formatWorkedLabel(secs: number, cancelled = false): string {
  const worked = secs >= 1 ? `Worked ${secs}s` : 'Worked';
  return cancelled ? (secs >= 1 ? `Cancelled · ${worked}` : 'Cancelled') : worked;
}
