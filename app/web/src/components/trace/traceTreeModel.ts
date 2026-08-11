/**
 * Pure logic behind the trace tree: which nodes carry a failure, which are
 * expanded by default (the failure path + the current selection), which turns'
 * step trees must be fetched, and span/step lookups. No React, no rendering —
 * so it stays unit-testable and the `TraceTree` component reads as layout.
 */
import type {
  ExternalAgentKind,
  LifecycleState,
  ReplayStep,
  SessionMessageRow,
  Span,
  TraceTurnSummary,
  TurnStatusKind,
  TurnTrace,
} from '../../types/trace';
import { isChatTurn } from '../../types/trace';

/**
 * A node "needs attention" when its outcome is anything other than `ok` —
 * `failed` / `cancelled` (terminal problems) or `pending` (still in flight).
 * These are the nodes the tree auto-expands and badges.
 */
export function attention(state: LifecycleState): boolean {
  return state.outcome !== 'ok';
}

/** How one turn row is labelled: `short` for the sidebar chip, `long` for the tree header. */
export interface TurnLabel {
  short: string;
  long: string;
}

/** Labels for the two kinds the chat never showed as turns. */
const NON_CHAT_TURN_LABEL: Record<string, TurnLabel> = {
  compact: { short: 'cmp', long: 'Compaction' },
  cron_notification: { short: 'dlv', long: 'Cron delivery' },
};

/**
 * Label every turn row, in the overview's own oldest-first order.
 *
 * Only chat turns are numbered, and they are numbered among themselves — a
 * `/compact` or a cron-result delivery opens a real turn row but was never a
 * turn the transcript showed, so counting them made the sidebar say "#3" for a
 * session the chat rendered as two turns. Non-chat rows get their kind as the
 * label instead of a number.
 *
 * Returned as a whole array rather than a per-index lookup so the numbering is
 * one pass, not one per rendered row.
 */
export function turnLabels(turns: TraceTurnSummary[]): TurnLabel[] {
  let n = 0;
  return turns.map((turn) => {
    if (!isChatTurn(turn.turn_input_kind)) {
      return NON_CHAT_TURN_LABEL[turn.turn_input_kind] ?? { short: '·', long: turn.turn_input_kind };
    }
    n++;
    return { short: `#${n}`, long: `Turn #${n}` };
  });
}

export function turnFailed(status: TurnStatusKind): boolean {
  return status === 'failed' || status === 'stuck';
}

export function isTurnLive(status: TurnStatusKind): boolean {
  return status === 'pending' || status === 'in_progress' || status === 'stuck';
}

export function traceHasPendingSpan(trace: TurnTrace | undefined): boolean {
  if (!trace) return false;
  for (const rs of trace.steps) {
    if (rs.step.outcome.outcome === 'pending') return true;
    for (const s of rs.spans) {
      if (s.outcome.outcome === 'pending') return true;
    }
  }
  return false;
}

/**
 * Count the failing leaves in a loaded trace — spans whose outcome is
 * `failed`/`cancelled`, plus any span-less step that itself failed. Drives the
 * precise roll-up count on a turn/step row once its tree is loaded.
 */
export function failureCount(trace: TurnTrace): number {
  let n = 0;
  for (const rs of trace.steps) {
    let stepSpanFailures = 0;
    for (const s of rs.spans) {
      if (s.outcome.outcome === 'failed' || s.outcome.outcome === 'cancelled') {
        n += 1;
        stepSpanFailures += 1;
      }
    }
    if (
      stepSpanFailures === 0 &&
      (rs.step.outcome.outcome === 'failed' || rs.step.outcome.outcome === 'cancelled')
    ) {
      n += 1;
    }
  }
  return n;
}

export interface TurnRollup {
  /** Whether this turn's subtree contains (or is presumed to contain) a failure. */
  hasFailure: boolean;
  /** Precise failing-leaf count once the trace is loaded; `null` when unknown. */
  count: number | null;
}

/**
 * Roll-up badge state for a turn row. With the trace loaded we count failing
 * leaves exactly, but still honor `turn_status_kind` — a `stuck`/`failed` turn
 * whose recorded spans are all `ok` (the failure is the missing next step, or
 * recorded at the turn level) must keep its badge and survive the "failures
 * only" filter. Without a trace we fall back to status alone — a cheap
 * approximation (see PR1 deferred notes in docs/todo/trace-tree-redesign.md).
 */
export function turnRollup(summary: TraceTurnSummary, trace: TurnTrace | undefined): TurnRollup {
  const statusFail = turnFailed(summary.turn_status_kind);
  if (trace) {
    const count = failureCount(trace);
    return { hasFailure: count > 0 || statusFail, count: count > 0 ? count : null };
  }
  return { hasFailure: statusFail, count: null };
}

/** Resolve a node's expanded state: an explicit user toggle wins over the
 *  default. Turns and steps are expanded by default (nothing is hidden — the
 *  bench-web "show the whole flow" model), so the default is `true`; a chevron
 *  click records `false` to collapse a specific node. */
export function resolveExpanded(id: string, userToggles: Map<string, boolean>, def: boolean): boolean {
  const override = userToggles.get(id);
  return override === undefined ? def : override;
}

/**
 * The set of turn ids whose step tree must be fetched. Since every turn is
 * expanded by default (nothing collapsed), that's every turn — except the ones
 * the user explicitly collapsed (a collapsed turn needs no tree). The selected
 * turn is always included.
 */
export function neededTurnIds(
  turns: TraceTurnSummary[],
  userToggles: Map<string, boolean>,
  selectedTurnId: string | null,
): string[] {
  const out: string[] = [];
  for (const j of turns) {
    if (j.turn_id === selectedTurnId || resolveExpanded(j.turn_id, userToggles, true)) {
      out.push(j.turn_id);
    }
  }
  return out;
}

/** Locate a span (and its step) inside a loaded turn trace. */
export function findSpan(trace: TurnTrace | undefined, spanId: string): { span: Span; stepId: string } | null {
  if (!trace) return null;
  for (const rs of trace.steps) {
    const span = rs.spans.find((s) => s.id === spanId);
    if (span) return { span, stepId: rs.step.id };
  }
  return null;
}

export function findStep(trace: TurnTrace | undefined, stepId: string): ReplayStep | null {
  if (!trace) return null;
  return trace.steps.find((rs) => rs.step.id === stepId) ?? null;
}

/**
 * Whether a filter string is a bare trace id rather than search text.
 *
 * Step and span ids are ULIDs: 26 characters of Crockford base32, which
 * excludes I / L / O / U. Ordinary search words are shorter, lowercase, or
 * contain excluded letters, so the shape is a reliable discriminator — and
 * being wrong is harmless either way, since a "jump" that resolves to
 * nothing simply stays a text filter.
 */
const ULID_RE = /^[0-9ABCDEFGHJKMNPQRSTVWXYZ]{26}$/;

export function looksLikeTraceId(text: string): boolean {
  return ULID_RE.test(text.trim().toUpperCase());
}

/** The node a pasted id names, across every loaded turn. */
export interface JumpTarget {
  turnId: string;
  stepId: string;
  spanId: string | null;
}

/**
 * Resolve a pasted step/span id to the turn that holds it.
 *
 * Searches every loaded turn rather than the selected one: an id is pasted
 * precisely when the reader does not know where it lives. A text filter
 * forces every turn's tree to load, which is what makes this reach the whole
 * session rather than the turns that happened to be expanded.
 */
export function resolveJumpTarget(
  traces: Map<string, TurnTrace>,
  rawId: string,
): JumpTarget | null {
  const id = rawId.trim().toUpperCase();
  if (!looksLikeTraceId(id)) return null;
  for (const [turnId, trace] of traces) {
    const span = findSpan(trace, id);
    if (span) return { turnId, stepId: span.stepId, spanId: id };
    if (findStep(trace, id)) return { turnId, stepId: id, spanId: null };
  }
  return null;
}

/**
 * Where a node sits in the order the trace store returns.
 *
 * `steps` come back ordered by `started_at` within their turn and `spans`
 * likewise within their step (`ORDER BY started_at` in both queries), so a
 * node's position in the array it arrived in IS its recorded order. Numbers
 * are 1-based for display: step `#3`, its second span `#3.2`.
 */
export interface StepOrder {
  step: number;
  stepTotal: number;
}

export interface SpanOrder extends StepOrder {
  span: number;
  spanTotal: number;
}

export function stepOrder(trace: TurnTrace | undefined, stepId: string): StepOrder | null {
  if (!trace) return null;
  const i = trace.steps.findIndex((rs) => rs.step.id === stepId);
  return i < 0 ? null : { step: i + 1, stepTotal: trace.steps.length };
}

export function spanOrder(trace: TurnTrace | undefined, spanId: string): SpanOrder | null {
  if (!trace) return null;
  for (let i = 0; i < trace.steps.length; i++) {
    const j = trace.steps[i].spans.findIndex((s) => s.id === spanId);
    if (j >= 0) {
      return {
        step: i + 1,
        stepTotal: trace.steps.length,
        span: j + 1,
        spanTotal: trace.steps[i].spans.length,
      };
    }
  }
  return null;
}

/**
 * Whether a turn's trace is an external agent (claude/codex) whose
 * internal loop is opaque — it records no step/span tree ever, so its
 * transcript is the trace.
 *
 * `externalAgent` is the session-level wire marker (`TraceOverview.external_agent`).
 * When present it is authoritative and the turn qualifies **while it is still
 * running** — that is the whole point: a live external run would otherwise
 * show nothing at all until it terminated, because its steps are never coming.
 *
 * Without the marker (sessions written before the backend tag reached the
 * wire) we keep the old heuristic, `!isTurnLive` gate included: a pending /
 * in_progress *internal* turn can momentarily have zero recorded steps and must
 * NOT be mislabeled an external agent — it self-corrects as steps land.
 */
export function isExternalAgentTurn(
  trace: TurnTrace | undefined,
  status: TurnStatusKind,
  externalAgent?: ExternalAgentKind | null,
): boolean {
  // With the marker there is nothing to wait for — an external session's step
  // tree is never coming, and its turns are not even fetched, so an unfetched
  // trace must qualify too. Only an actual tree with steps in it disqualifies.
  if (externalAgent != null) return !trace || trace.steps.length === 0;
  return !!trace && trace.steps.length === 0 && !isTurnLive(status);
}

/**
 * Partition a session transcript across its turns.
 *
 * `session_messages` rows carry no `turn_id` — only an ordinal and a
 * timestamp — so a row belongs to the last turn that had started when it was
 * written. Rows predating the first turn (an external run persists its task
 * prompt around the same instant the turn opens, and the two orderings are not
 * guaranteed) fold into that first turn rather than vanishing, and the newest
 * turn's window stays open-ended so a live run's rows land as they arrive.
 *
 * Superseded rows are dropped: a compaction rewrote them, and replaying the
 * pre-compaction text as though it were still the transcript would show the
 * reader history the agent no longer has.
 *
 * Returns a map keyed by `turn_id`; turns with no rows are absent.
 */
export function partitionTranscript(
  messages: SessionMessageRow[],
  turns: TraceTurnSummary[],
): Map<string, SessionMessageRow[]> {
  const out = new Map<string, SessionMessageRow[]>();
  if (turns.length === 0) return out;
  // Ascending by start; the overview already sorts turns oldest-first, but a
  // local sort keeps this correct if a caller passes an unordered slice.
  const bounds = turns
    .map((t) => ({ id: t.turn_id, at: Date.parse(t.started_at ?? t.created_at) }))
    .sort((a, b) => a.at - b.at);

  for (const row of messages) {
    if (row.superseded_by != null) continue;
    const at = Date.parse(row.created_at);
    let idx = 0;
    for (let i = 0; i < bounds.length; i++) {
      if (Number.isNaN(at) || bounds[i].at <= at) idx = i;
      else break;
    }
    const key = bounds[idx].id;
    const bucket = out.get(key);
    if (bucket) bucket.push(row);
    else out.set(key, [row]);
  }
  return out;
}
