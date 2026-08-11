/**
 * The unified trace navigator: a collapsible `Turn → Step → Span` tree that
 * replaces the old flat turn sidebar + step-card stream. Turns collapse to a
 * single row; the failure path (failed / stuck / pending nodes) auto-expands
 * and collapsed parents carry a red roll-up badge, so a complex trace is
 * navigable at a glance. See docs/todo/trace-tree-redesign.md.
 *
 * Rows carry an information-density layer (learned from the LangSmith / Langfuse
 * / Phoenix / Braintrust field survey): a per-kind colour stripe, an inline
 * latency bar scaled to the slowest sibling, rolled-up token badges, an optional
 * sibling-relative heat tint, and an optional inline I/O preview line — so the
 * tree reads like a table and rarely needs the detail panel.
 *
 * Filter state is owned by the page (controlled) so it can eager-load every
 * turn's step tree while a filter is active — a filter that searched only the
 * already-loaded turns would silently hide matches.
 */
import { useMemo, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import {
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiCornerDownLeftLine,
  RiExternalLinkLine,
  RiLoader4Line,
} from 'react-icons/ri';
import type {
  ExternalAgentKind,
  LifecycleState,
  LineageSession,
  ReplayStep,
  SessionMessageRow,
  Span,
  TraceOverview,
  TraceTurnSummary,
  TurnTrace,
} from '../../types/trace';
import { resolveToolCallOutput } from '../../types/trace';
import { Button } from '../Button';
import { SearchBox } from '../SearchBox';
import {
  CHILD_SESSION_VISUAL,
  durationMs,
  externalAgentLabel,
  formatDuration,
  formatTok,
  OutcomeBadge,
  stepSummaryText,
  stepVisual,
  stripeClass,
  sumLlmTokens,
  TRANSCRIPT_VISUALS,
  turnDurationMs,
  turnTokens,
} from './traceFormat';
import type { TraceGroup } from './traceFormat';
import { nodeGroup, spanToolName, spanVisualOf } from './traceFormat';
import type { TraceForest } from './traceForest';
import { sessionHasFailure, sessionIsLive } from './traceForest';
import type { TranscriptNode } from './transcriptModel';
import { buildTranscriptNodes, transcriptPreview, transcriptSearchText } from './transcriptModel';
import type { TurnRollup } from './traceTreeModel';
import {
  attention,
  isExternalAgentTurn,
  isTurnLive,
  partitionTranscript,
  resolveExpanded,
  turnLabels,
  turnRollup,
} from './traceTreeModel';

const INDENT_TURN = 8;
const INDENT_STEP = 26;
const INDENT_SPAN = 46;
/** How far a nested subagent's whole subtree shifts right of the span that
 *  spawned it, so the boundary reads as containment rather than as a sibling. */
const INDENT_NEST = 14;

/**
 * Hard stop on subagent nesting depth while rendering.
 *
 * The render walks child → turns → steps → spans → child, so a lineage that
 * points back at an ancestor would recurse until the stack blows and the whole
 * page white-screens. The backend's own walk is capped at 32 and de-duplicates
 * by session id, so this should be unreachable — which is exactly why it is
 * here: an unreachable guard that costs one integer compare beats a blank page
 * if the data ever disagrees with that assumption.
 */
const MAX_RENDER_DEPTH = 32;

export interface TraceTreeProps {
  overview: TraceOverview;
  turnTraces: Map<string, TurnTrace>;
  loadingTurns: Set<string>;
  userToggles: Map<string, boolean>;
  /** Toggle a node's expansion. `currentlyOpen` is its displayed state, so the
   *  handler can flip it regardless of whether it was open by default. */
  onToggle: (id: string, currentlyOpen: boolean) => void;
  selectedTurnId: string | null;
  selectedStepId: string | null;
  selectedSpanId: string | null;
  /** Selected transcript row, as its `TranscriptNode.id`. */
  selectedMessageId: string | null;
  /** Selected subagent boundary row, as its child session id. */
  selectedSessionId: string | null;
  onSelectTurn: (turnId: string) => void;
  onSelectStep: (turnId: string, stepId: string) => void;
  onSelectSpan: (turnId: string, spanId: string) => void;
  onSelectMessage: (turnId: string, nodeId: string) => void;
  onSelectSession: (sessionId: string) => void;
  /** Leave for the child's own trace page — the secondary action, now that the
   *  child renders in place. */
  onOpenSession: (sessionId: string) => void;
  interjectionCountByTurn: Map<string, number>;
  interjectionSpanIds: Set<string>;
  /** Session transcript — needed to resolve transcript-backed tool outputs for
   *  the inline I/O preview. */
  messageLog: SessionMessageRow[];
  /** Subagent descendants, indexed by where they attach. */
  forest: TraceForest;
  /** Lazily-fetched overview (transcript + turns) per expanded child session. */
  childOverviews: Map<string, TraceOverview>;
  loadingChildren: Set<string>;
  /** Legend group to highlight; non-matching rows dim. `null` = no highlight. */
  highlight: TraceGroup | null;
  // Controlled filter (owned by the page):
  filterRaw: string;
  onFilterRawChange: (v: string) => void;
  failuresOnly: boolean;
  onToggleFailures: () => void;
  filter: string; // debounced value used for matching
}

// ── Text projections used for the text filter ────────────────────────

// Ids are part of every projection so a pasted step/span id narrows the tree
// to that one row (the page then selects and scrolls to it). Turn rows have
// carried their id since the start; steps and spans had no way to be found by
// the identifier every deep link, log line, and API response quotes.
function spanText(span: Span): string {
  const v = spanVisualOf(span).label;
  const what = span.kind.kind === 'llm_call' ? span.kind.begin.model_id : span.kind.begin.tool_name;
  return `${v} ${what} ${span.id}`;
}

function stepText(rs: ReplayStep): string {
  return `${stepVisual(rs.step.kind.kind).label} ${rs.step.kind.kind} ${stepSummaryText(rs.step, rs.spans)} ${rs.step.id}`;
}

function turnText(label: string, turn: TraceTurnSummary): string {
  return `${label} ${turn.turn_status_kind} ${turn.turn_id}`;
}

// A one-line preview of what a span did: llm output or its emitted tool calls,
// and a tool call's output or params. A larger tool output rides as a transcript
// pointer, so it must be resolved against the message log — printing it raw
// would show a `$baybo_ref` object instead of the result.
function spanPreview(span: Span, messageLog: SessionMessageRow[]): string | null {
  if (span.kind.kind === 'llm_call') {
    const r = span.kind.result;
    if (r?.output_content != null && r.output_content !== '') return r.output_content;
    if (r?.tool_calls && r.tool_calls.length > 0) return '→ ' + r.tool_calls.map((t) => `${t.name}(…)`).join(', ');
    return null;
  }
  const r = span.kind.result;
  if (r) {
    const out = resolveToolCallOutput(r.output, messageLog, span.started_at);
    return typeof out === 'string' ? out : JSON.stringify(out);
  }
  try {
    return JSON.stringify(span.kind.begin.params);
  } catch {
    return null;
  }
}

// ── Small building blocks ────────────────────────────────────────────

function RollupBadge({ count }: { count: number | null }) {
  return (
    <span
      title={count === null ? 'contains a failure' : `${count} failure${count === 1 ? '' : 's'} in subtree`}
      className="shrink-0 inline-flex items-center gap-1 border-2 border-err rounded bg-err/10 px-1 py-px text-[0.6rem] font-bold uppercase tracking-wider text-err"
    >
      <span className="inline-block w-1.5 h-1.5 rounded-full bg-err" />
      {count === null ? 'fail' : count}
    </span>
  );
}

function Chevron({ open, onClick }: { open: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      className="shrink-0 flex items-center justify-center h-5 w-5 text-ink-soft hover:text-ink cursor-pointer"
      aria-label={open ? 'Collapse' : 'Expand'}
    >
      {open ? <RiArrowDownSLine /> : <RiArrowRightSLine />}
    </button>
  );
}

// A compact one-char kind marker (bench-web style) — replaces the icon disc so
// rows stay dense and the left colour stripe carries the kind cue.
function Glyph({ ch, accent }: { ch: string; accent: string }) {
  return <span className={`w-4 text-center font-mono text-[0.82rem] font-bold shrink-0 ${accent}`}>{ch}</span>;
}

// Latency bar scaled to the slowest sibling (Phoenix/Langfuse "the row is the
// waterfall"): the slowest node in a group fills the track, the rest scale down.
function LatencyBar({ ms, maxMs, outcome }: { ms: number | null; maxMs: number; outcome: LifecycleState }) {
  const pct = ms != null && maxMs > 0 ? Math.max(3, Math.min(100, (ms / maxMs) * 100)) : 0;
  const color =
    outcome.outcome === 'failed' || outcome.outcome === 'cancelled'
      ? 'bg-err/70'
      : outcome.outcome === 'pending'
        ? 'bg-info/60'
        : 'bg-ink/25';
  return (
    <span
      className="hidden md:block w-20 lg:w-32 xl:w-48 h-1.5 rounded-full bg-black/10 relative shrink-0"
      title={formatDuration(ms)}
    >
      <span className={`absolute left-0 top-0 h-full rounded-full ${color}`} style={{ width: `${pct}%` }} />
    </span>
  );
}

// Sibling-relative heat tint (Langfuse "%"): the slowest branch in each group
// glows red, the next tier amber. Only applied to unselected rows.
//
// `siblings` gates it: with a single child there is nothing to compare against —
// that row is trivially 100% of its group's max and would always glow red, which
// tints most of the tree and destroys the signal. Needs at least two siblings.
function heatTint(ms: number | null, maxMs: number, siblings: number): string {
  if (ms == null || maxMs <= 0 || siblings < 2) return '';
  const r = ms / maxMs;
  if (r >= 0.75) return 'bg-err/10';
  if (r >= 0.5) return 'bg-warn/10';
  return '';
}

function rowShell(selected: boolean, stripe: string, tint: string, dim = false): string {
  const bg = selected ? 'bg-selected text-ink' : `${tint || 'bg-transparent'} hover:bg-gray-50`;
  const border = selected ? 'border-l-black' : stripe;
  return `group w-full text-left flex items-center gap-2 pr-3 py-1.5 border-l-[3px] ${border} border-b border-b-black/10 cursor-pointer transition-[colors,opacity] ${bg} ${
    dim ? 'opacity-25' : ''
  }`;
}

function rowInteractive(onSelect: () => void) {
  return {
    role: 'button' as const,
    tabIndex: 0,
    onClick: onSelect,
    onKeyDown: (e: ReactKeyboardEvent<HTMLDivElement>) => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        onSelect();
      }
    },
  };
}

function TokenBadge({ input, output }: { input: number; output: number }) {
  if (input <= 0 && output <= 0) return null;
  return (
    <span className="shrink-0 hidden lg:inline-flex items-center gap-1 border border-black/25 rounded bg-white px-1 py-px text-[0.6rem] font-bold font-mono text-ink">
      ↑{formatTok(input)} ↓{formatTok(output)}
    </span>
  );
}

// ── Span leaf row ────────────────────────────────────────────────────

/** The `#3` / `#3.2` position marker. Rendered as its own dim column so it
 *  reads as an index rather than as part of the title. */
function OrderMark({ label }: { label: string }) {
  return (
    <span className="shrink-0 w-8 text-right font-mono text-[0.65rem] font-bold text-ink-soft tabular-nums">
      {label}
    </span>
  );
}

function SpanRow({
  span,
  order,
  selected,
  interjected,
  maxMs,
  siblings,
  heat,
  dim,
  indent,
  onSelect,
}: {
  span: Span;
  /** Position in storage order: `<step>.<span>`, both 1-based. */
  order: string;
  selected: boolean;
  interjected: boolean;
  maxMs: number;
  siblings: number;
  dim: boolean;
  heat: boolean;
  indent: number;
  onSelect: () => void;
}) {
  const visual = spanVisualOf(span);
  const ms = durationMs(span);
  const tokens = sumLlmTokens([span]);

  let title = '';
  let subtitle = visual.label;
  if (span.kind.kind === 'llm_call') {
    title = span.kind.begin.model_id;
    if (span.kind.result) {
      // Usage stays in the always-visible subtitle rather than moving to the
      // `TokenBadge` column: that column is `hidden lg:`, so a narrow window
      // would drop a span's token figures entirely.
      const cached = tokens.cached + tokens.cacheCreate;
      const parts = [`${tokens.input.toLocaleString()} in / ${tokens.output.toLocaleString()} out`];
      if (cached > 0) parts.push(`${cached.toLocaleString()} cached`);
      const toolCount = span.kind.begin.tools?.count ?? 0;
      if (toolCount > 0) parts.push(`${toolCount} tools`);
      subtitle = parts.join(' · ');
    } else {
      subtitle = 'in flight';
    }
  } else {
    title = span.kind.begin.tool_name;
    if (!span.kind.result) subtitle = 'in flight';
    else if (span.kind.result.success) subtitle = 'success';
    else subtitle = span.outcome.outcome === 'failed' && span.outcome.reason ? `failed: ${span.outcome.reason}` : 'failed';
  }

  const tint = heat && !selected ? heatTint(ms, maxMs, siblings) : '';

  return (
    <div {...rowInteractive(onSelect)} className={rowShell(selected, stripeClass(visual), tint, dim)} style={{ paddingLeft: indent }}>
      <OrderMark label={order} />
      <Glyph ch={visual.glyph} accent={visual.accent} />
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 min-w-0">
          <span className="font-bold text-[0.8rem] truncate">{title}</span>
          {interjected && (
            <span
              title="A mid-turn user interjection was folded into this iteration's input"
              className="shrink-0 inline-flex items-center gap-0.5 text-[0.6rem] font-bold uppercase tracking-wider text-warn border border-warn rounded px-1"
            >
              <RiCornerDownLeftLine className="text-[0.7rem]" />
            </span>
          )}
          {span.parallel_group != null && span.parallel_group !== '' && (
            <span className="shrink-0 text-[0.6rem] font-bold uppercase tracking-wider text-warn border border-warn rounded px-1">
              ‖
            </span>
          )}
        </div>
        <div className="text-[0.7rem] text-ink-soft font-mono truncate">{subtitle}</div>
      </div>
      <div className="shrink-0 flex items-center gap-2">
        <LatencyBar ms={ms} maxMs={maxMs} outcome={span.outcome} />
        <OutcomeBadge state={span.outcome} />
        <span className="text-[0.7rem] font-mono text-ink-soft w-14 text-right">{formatDuration(ms)}</span>
      </div>
    </div>
  );
}

// ── Step row ─────────────────────────────────────────────────────────

function StepRow({
  rs,
  order,
  selected,
  open,
  hasSpans,
  maxMs,
  siblings,
  heat,
  dim,
  indent,
  onToggle,
  onSelect,
}: {
  rs: ReplayStep;
  /** 1-based position among the turn's steps, in storage order. */
  order: number;
  selected: boolean;
  open: boolean;
  hasSpans: boolean;
  maxMs: number;
  siblings: number;
  dim: boolean;
  heat: boolean;
  indent: number;
  onToggle: () => void;
  onSelect: () => void;
}) {
  const visual = stepVisual(rs.step.kind.kind);
  const summary = stepSummaryText(rs.step, rs.spans);
  const ms = durationMs(rs.step);
  const tokens = sumLlmTokens(rs.spans);
  const tint = heat && !selected ? heatTint(ms, maxMs, siblings) : '';

  return (
    <div {...rowInteractive(onSelect)} className={rowShell(selected, stripeClass(visual), tint, dim)} style={{ paddingLeft: indent }}>
      {hasSpans ? <Chevron open={open} onClick={onToggle} /> : <span className="shrink-0 w-5" />}
      <OrderMark label={`#${order}`} />
      <Glyph ch={visual.glyph} accent={visual.accent} />
      <div className="flex-1 min-w-0">
        <div className="font-bold uppercase tracking-wide text-[0.78rem] truncate">{visual.label}</div>
        <div className="text-[0.72rem] text-ink-soft font-mono truncate">{summary}</div>
      </div>
      <div className="shrink-0 flex items-center gap-2">
        <TokenBadge input={tokens.input} output={tokens.output} />
        <LatencyBar ms={ms} maxMs={maxMs} outcome={rs.step.outcome} />
        <OutcomeBadge state={rs.step.outcome} />
        <span className="text-[0.7rem] font-mono text-ink-soft w-14 text-right">{formatDuration(ms)}</span>
      </div>
    </div>
  );
}

// ── Turn row ─────────────────────────────────────────────────────────

function TurnRow({
  turn,
  label,
  trace,
  loading,
  selected,
  open,
  rollup,
  interjections,
  indent,
  onToggle,
  onSelect,
}: {
  turn: TraceTurnSummary;
  label: string;
  trace: TurnTrace | undefined;
  loading: boolean;
  selected: boolean;
  open: boolean;
  rollup: TurnRollup;
  interjections: number;
  indent: number;
  onToggle: () => void;
  onSelect: () => void;
}) {
  const tokens = turnTokens(turn, trace);
  const dur = turnDurationMs(turn, trace);
  const live = isTurnLive(turn.turn_status_kind);

  return (
    <div {...rowInteractive(onSelect)} className={rowShell(selected, 'border-l-brand', '')} style={{ paddingLeft: indent }}>
      <Chevron open={open} onClick={onToggle} />
      <Glyph ch="◆" accent="text-brand" />
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 min-w-0">
          <span className="font-bold text-[0.85rem]">{label}</span>
          <span className="text-[0.72rem] text-ink-soft font-mono truncate">{turn.turn_status_kind}</span>
          {live && <RiLoader4Line className="shrink-0 text-info animate-spin text-xs" />}
        </div>
        <div className="text-[0.7rem] text-ink-soft font-mono">
          ↑{formatTok(tokens.input)} ↓{formatTok(tokens.output)}
        </div>
      </div>
      <div className="shrink-0 flex items-center gap-2">
        {interjections > 0 && (
          <span
            title="This turn folded in mid-turn user message(s) (steering)"
            className="inline-flex items-center gap-0.5 border-2 border-black rounded bg-warn/15 px-1 py-px text-[0.6rem] font-bold text-warn"
          >
            <RiCornerDownLeftLine className="text-[0.7rem]" />
            {interjections}
          </span>
        )}
        {loading && <RiLoader4Line className="text-ink-soft animate-spin text-xs" />}
        {rollup.hasFailure && <RollupBadge count={rollup.count} />}
        <span className="text-[0.7rem] font-mono text-ink-soft w-14 text-right">{formatDuration(dur)}</span>
      </div>
    </div>
  );
}

// ── Transcript row (external agent) ──────────────────────────────────

/**
 * One row of an external agent's transcript. An external run records no
 * step/span tree, so these rows ARE its trace — styled as span rows so a
 * claude/codex subagent reads like the rest of the tree rather than like a
 * chat log bolted on the side.
 */
function TranscriptRow({
  node,
  selected,
  showPreview,
  dim,
  indent,
  onSelect,
}: {
  node: TranscriptNode;
  selected: boolean;
  showPreview: boolean;
  dim: boolean;
  indent: number;
  onSelect: () => void;
}) {
  const visual = TRANSCRIPT_VISUALS[node.kind];
  const pending = node.tool != null && node.tool.output == null;
  const preview = transcriptPreview(node);

  return (
    <div>
      <div
        {...rowInteractive(onSelect)}
        className={rowShell(selected, stripeClass(visual), '', dim)}
        style={{ paddingLeft: indent }}
      >
        <Glyph ch={visual.glyph} accent={visual.accent} />
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2 min-w-0">
            <span className="font-bold text-[0.8rem] truncate">{node.title}</span>
            {pending && <RiLoader4Line className="shrink-0 text-info animate-spin text-xs" />}
          </div>
          {!showPreview && preview !== '' && (
            <div className="text-[0.7rem] text-ink-soft font-mono truncate">{preview.slice(0, 200)}</div>
          )}
        </div>
        <span className="shrink-0 text-[0.7rem] font-mono text-ink-soft w-14 text-right">#{node.ordinal}</span>
      </div>
      {showPreview && preview !== '' && (
        <div
          className="pr-3 pb-1.5 text-[0.68rem] text-ink-soft font-mono italic truncate border-b border-b-black/10"
          style={{ paddingLeft: indent }}
          title={preview.slice(0, 400)}
        >
          {preview.slice(0, 160)}
        </div>
      )}
    </div>
  );
}

// ── Child session row (subagent boundary) ────────────────────────────

/** The seam where a subagent's own trace begins, nested under the
 *  `spawn_subagent` span that opened it. */
function ChildSessionRow({
  child,
  selected,
  open,
  loading,
  failed,
  live,
  indent,
  onToggle,
  onSelect,
  onOpenFullPage,
}: {
  child: LineageSession;
  selected: boolean;
  open: boolean;
  loading: boolean;
  failed: boolean;
  live: boolean;
  indent: number;
  onToggle: () => void;
  onSelect: () => void;
  onOpenFullPage: () => void;
}) {
  const backend = child.external_agent != null ? externalAgentLabel(child.external_agent) : 'baybo';
  const turns = child.turns.length;

  return (
    <div
      {...rowInteractive(onSelect)}
      className={rowShell(selected, stripeClass(CHILD_SESSION_VISUAL), '')}
      style={{ paddingLeft: indent }}
    >
      <Chevron open={open} onClick={onToggle} />
      <Glyph ch="⧉" accent={CHILD_SESSION_VISUAL.accent} />
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 min-w-0">
          <span className="font-bold text-[0.8rem] truncate">{child.subagent_type ?? 'subagent'}</span>
          <span className="shrink-0 text-[0.6rem] font-bold uppercase tracking-wider text-brand border border-brand rounded px-1">
            {backend}
          </span>
          {live && <RiLoader4Line className="shrink-0 text-info animate-spin text-xs" />}
        </div>
        <div className="text-[0.7rem] text-ink-soft font-mono truncate">
          {turns} {turns === 1 ? 'turn' : 'turns'} · {child.session_id}
        </div>
      </div>
      <div className="shrink-0 flex items-center gap-2">
        {loading && <RiLoader4Line className="text-ink-soft animate-spin text-xs" />}
        {failed && <RollupBadge count={null} />}
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onOpenFullPage();
          }}
          title="Open this subagent as its own trace page"
          aria-label="Open this subagent as its own trace page"
          className="shrink-0 flex items-center justify-center h-5 w-5 text-ink-soft hover:text-ink cursor-pointer"
        >
          <RiExternalLinkLine />
        </button>
      </div>
    </div>
  );
}

// A compact toggle button for the density controls.
function Toggle({ on, onClick, children, title }: { on: boolean; onClick: () => void; children: string; title: string }) {
  return (
    <Button
      variant={on ? 'primary' : 'default'}
      onClick={onClick}
      className="!py-1 !px-2 !text-[0.72rem] h-8 shrink-0 whitespace-nowrap"
      title={title}
    >
      {children}
    </Button>
  );
}

// ── Tree ─────────────────────────────────────────────────────────────

/**
 * Everything a subtree needs that doesn't vary per node. Threaded as one
 * object because the tree is now recursive — a subagent's session nests a
 * whole `Turn → Step → Span` tree of its own under the span that spawned it,
 * and prop-drilling twenty parameters through that recursion is unreadable.
 */
interface TreeCtx {
  turnTraces: Map<string, TurnTrace>;
  loadingTurns: Set<string>;
  userToggles: Map<string, boolean>;
  onToggle: (id: string, currentlyOpen: boolean) => void;
  selectedTurnId: string | null;
  selectedStepId: string | null;
  selectedSpanId: string | null;
  selectedMessageId: string | null;
  selectedSessionId: string | null;
  onSelectTurn: (turnId: string) => void;
  onSelectStep: (turnId: string, stepId: string) => void;
  onSelectSpan: (turnId: string, spanId: string) => void;
  onSelectMessage: (turnId: string, nodeId: string) => void;
  onSelectSession: (sessionId: string) => void;
  onOpenSession: (sessionId: string) => void;
  interjectionCountByTurn: Map<string, number>;
  interjectionSpanIds: Set<string>;
  highlight: TraceGroup | null;
  heat: boolean;
  preview: boolean;
  filtering: boolean;
  failuresOnly: boolean;
  q: string;
  matchers: {
    spanMatch: (span: Span) => boolean;
    stepVisible: (rs: ReplayStep) => boolean;
  };
  forest: TraceForest;
  childOverviews: Map<string, TraceOverview>;
  loadingChildren: Set<string>;
  /**
   * Whether a subagent survives the current filter — its own identity, its
   * subtree's content, or its failure state. Memoized per render because the
   * step and span gates both have to consult it to avoid filtering away a
   * matching child's ancestors.
   */
  childVisible: (child: LineageSession) => boolean;
}

function transcriptText(node: TranscriptNode): string {
  return transcriptSearchText(node).toLowerCase();
}

function childText(child: LineageSession): string {
  return `${child.subagent_type ?? ''} ${child.external_agent ?? ''} ${child.session_id} subagent`.toLowerCase();
}

/**
 * Whether a text filter should keep a subagent row — its own identity, or
 * anything in the subtree beneath it.
 *
 * Without the deep half, a filter hid the child row on an identity miss and
 * took its whole subtree with it, so a matching transcript row or step inside a
 * subagent became unreachable. Only *loaded* content can be searched (a
 * collapsed child has fetched nothing), which is the same
 * already-loaded-content limitation the turn-level filter has.
 */
/** Whether any subagent hanging off this span survives the current filter. */
function spanHostsVisibleChild(spanId: string, ctx: TreeCtx): boolean {
  const kids = ctx.forest.bySpan.get(spanId);
  return kids != null && kids.some((c) => ctx.childVisible(c));
}

function childSubtreeMatches(child: LineageSession, ctx: TreeCtx, seen: Set<string>): boolean {
  if (seen.has(child.session_id)) return false;
  seen.add(child.session_id);
  if (childText(child).includes(ctx.q)) return true;

  const overview = ctx.childOverviews.get(child.session_id);
  const turns = overview && overview.turns.length > 0 ? overview.turns : child.turns;
  if (turns.some((t) => `${t.turn_status_kind} ${t.turn_id}`.toLowerCase().includes(ctx.q))) return true;

  if (overview) {
    const nodes = buildTranscriptNodes(overview.session_messages);
    if (nodes.some((n) => transcriptText(n).includes(ctx.q))) return true;
  }
  for (const t of turns) {
    const trace = ctx.turnTraces.get(t.turn_id);
    if (trace == null) continue;
    const hit = trace.steps.some(
      (rs) =>
        stepText(rs).toLowerCase().includes(ctx.q) ||
        rs.spans.some((sp) => spanText(sp).toLowerCase().includes(ctx.q)),
    );
    if (hit) return true;
  }
  return (ctx.forest.byParentSession.get(child.session_id) ?? []).some((g) =>
    childSubtreeMatches(g, ctx, seen),
  );
}

// ── Nested child sessions ────────────────────────────────────────────

/** The subagent sessions attached at one point in the tree (a span, or a turn
 *  when the spawning span wasn't recorded), each expanding into its own trace. */
function ChildSessions({
  children,
  offset,
  depth,
  ctx,
}: {
  children: LineageSession[] | undefined;
  offset: number;
  depth: number;
  ctx: TreeCtx;
}) {
  if (!children || children.length === 0) return null;
  if (depth >= MAX_RENDER_DEPTH) {
    return (
      <div className="py-1.5 text-ink-soft text-[0.72rem] italic" style={{ paddingLeft: offset + INDENT_SPAN }}>
        subagent nesting truncated at {MAX_RENDER_DEPTH} levels
      </div>
    );
  }
  return (
    <>
      {children.map((child) => (
        <ChildSession key={child.session_id} child={child} offset={offset} depth={depth} ctx={ctx} />
      ))}
    </>
  );
}

function ChildSession({
  child,
  offset,
  depth,
  ctx,
}: {
  child: LineageSession;
  offset: number;
  depth: number;
  ctx: TreeCtx;
}) {
  const failed = sessionHasFailure(ctx.forest, child.session_id, ctx.turnTraces);
  // A failing subagent must survive "failures only" — that filter exists to
  // find exactly this, and a child hidden behind it takes its whole subtree
  // with it.
  if (ctx.filtering && !ctx.childVisible(child)) return null;

  const open = resolveExpanded(child.session_id, ctx.userToggles, true);
  const overview = ctx.childOverviews.get(child.session_id);
  const indent = offset + INDENT_SPAN + INDENT_NEST;

  return (
    <div data-child-session-id={child.session_id}>
      <ChildSessionRow
        child={child}
        selected={ctx.selectedSessionId === child.session_id}
        open={open}
        loading={ctx.loadingChildren.has(child.session_id)}
        failed={failed}
        live={sessionIsLive(ctx.forest, child.session_id)}
        indent={indent}
        onToggle={() => ctx.onToggle(child.session_id, open)}
        onSelect={() => ctx.onSelectSession(child.session_id)}
        onOpenFullPage={() => ctx.onOpenSession(child.session_id)}
      />
      {open &&
        (overview ? (
          <SessionTurns
            turns={overview.turns.length > 0 ? overview.turns : child.turns}
            messages={overview.session_messages}
            externalAgent={overview.external_agent ?? child.external_agent}
            offset={indent}
            depth={depth + 1}
            ctx={ctx}
          />
        ) : (
          <div
            className="flex items-center gap-2 py-1.5 text-ink-soft text-[0.75rem] italic"
            style={{ paddingLeft: indent + INDENT_NEST }}
          >
            <RiLoader4Line className="animate-spin" /> Loading subagent trace…
          </div>
        ))}
    </div>
  );
}

// ── One session's turns ──────────────────────────────────────────────

function SessionTurns({
  turns,
  messages,
  externalAgent,
  offset,
  depth,
  ctx,
}: {
  turns: TraceTurnSummary[];
  messages: SessionMessageRow[];
  externalAgent: ExternalAgentKind | null | undefined;
  offset: number;
  depth: number;
  ctx: TreeCtx;
}) {
  const labels = useMemo(() => turnLabels(turns), [turns]);
  // Transcript rows carry no turn id of their own, so the session's log is
  // split across its turns by timestamp once per session rather than per row.
  const transcripts = useMemo(() => partitionTranscript(messages, turns), [messages, turns]);

  return (
    <>
      {turns.map((turn, i) => (
        <TurnSubtree
          key={turn.turn_id}
          turn={turn}
          label={labels[i].long}
          transcriptRows={transcripts.get(turn.turn_id) ?? []}
          messages={messages}
          externalAgent={externalAgent}
          offset={offset}
          depth={depth}
          ctx={ctx}
        />
      ))}
    </>
  );
}

// ── One turn, and everything under it ────────────────────────────────

function TurnSubtree({
  turn,
  label,
  transcriptRows,
  messages,
  externalAgent,
  offset,
  depth,
  ctx,
}: {
  turn: TraceTurnSummary;
  label: string;
  transcriptRows: SessionMessageRow[];
  messages: SessionMessageRow[];
  externalAgent: ExternalAgentKind | null | undefined;
  offset: number;
  depth: number;
  ctx: TreeCtx;
}) {
  const trace = ctx.turnTraces.get(turn.turn_id);
  const loading = ctx.loadingTurns.has(turn.turn_id);
  const rollup = turnRollup(turn, trace);

  // With the session-level marker in hand there is nothing to wait for: an
  // external run's step tree is never going to arrive, so its transcript
  // renders immediately — including while it is still running, which is the
  // whole point of the marker.
  const external = isExternalAgentTurn(trace, turn.turn_status_kind, externalAgent);
  const nodes = useMemo(() => (external ? buildTranscriptNodes(transcriptRows) : []), [external, transcriptRows]);

  const maxStepMs = trace ? Math.max(1, ...trace.steps.map((rs) => durationMs(rs.step) ?? 0)) : 1;
  const turnChildren = ctx.forest.byTurn.get(turn.turn_id);

  let showAllChildren = false;
  if (ctx.filtering) {
    const turnShallow =
      (!ctx.failuresOnly || rollup.hasFailure) && (!ctx.q || turnText(label, turn).toLowerCase().includes(ctx.q));
    const deepSteps = trace ? trace.steps.some(ctx.matchers.stepVisible) : false;
    const deepTranscript = !ctx.failuresOnly && !!ctx.q && nodes.some((n) => transcriptText(n).includes(ctx.q));
    // Children hang off a SPAN in every trace written since `parent_span_id`
    // existed, so counting only the turn-attached fallback bucket missed
    // essentially all of them — a turn whose only match was its subagent got
    // filtered away with it.
    const spanChildren =
      trace?.steps.some((rs) => rs.spans.some((sp) => spanHostsVisibleChild(sp.id, ctx))) ?? false;
    const deepTurnChildren = (turnChildren ?? []).some((c) => ctx.childVisible(c));
    const turnDeep = deepSteps || deepTranscript || deepTurnChildren || spanChildren;
    // `!!trace` stands for "we know what is in this turn". An external turn is
    // known to hold no tree at all and is never fetched, so requiring one would
    // make "failures only" render a failed external subagent as an empty row.
    const known = external || !!trace;
    const statusOnly = ctx.failuresOnly && !ctx.q && rollup.hasFailure && known && !deepSteps;
    if (!(turnShallow || turnDeep || statusOnly || loading)) return null;
    showAllChildren = statusOnly;
  }

  const turnOpen = resolveExpanded(turn.turn_id, ctx.userToggles, true);
  const turnSelected =
    ctx.selectedTurnId === turn.turn_id &&
    ctx.selectedStepId == null &&
    ctx.selectedSpanId == null &&
    ctx.selectedMessageId == null &&
    ctx.selectedSessionId == null;

  return (
    <div data-turn-id={turn.turn_id}>
      <TurnRow
        turn={turn}
        label={label}
        trace={trace}
        loading={loading}
        selected={turnSelected}
        open={turnOpen}
        rollup={rollup}
        interjections={ctx.interjectionCountByTurn.get(turn.turn_id) ?? 0}
        indent={offset + INDENT_TURN}
        onToggle={() => ctx.onToggle(turn.turn_id, turnOpen)}
        onSelect={() => ctx.onSelectTurn(turn.turn_id)}
      />
      {turnOpen && (
        <>
          {loading && !trace && !external && (
            <div
              className="flex items-center gap-2 py-1.5 text-ink-soft text-[0.75rem] italic"
              style={{ paddingLeft: offset + INDENT_STEP }}
            >
              <RiLoader4Line className="animate-spin" /> Loading turn…
            </div>
          )}

          {external && (
            <TranscriptBody
              nodes={nodes}
              live={isTurnLive(turn.turn_status_kind)}
              turnId={turn.turn_id}
              showAllChildren={showAllChildren}
              offset={offset}
              ctx={ctx}
            />
          )}

          {!external && !!trace && trace.steps.length === 0 && (
            <div
              className="py-1.5 text-ink-soft text-[0.72rem] italic"
              style={{ paddingLeft: offset + INDENT_STEP }}
            >
              {isTurnLive(turn.turn_status_kind) ? 'no steps recorded yet…' : 'no steps recorded'}
            </div>
          )}

          {/* `stepIndex` / `spanIndex` are the position in the array the API
              returned, which IS the stored order (`ORDER BY started_at` on both
              queries) — so a filtered-out row does not renumber its siblings. */}
          {trace?.steps.map((rs, stepIndex) => {
            // A step that hosts a surviving subagent must stay, or the child
            // is unreachable behind an ancestor the filter dropped.
            const stepHostsChild = rs.spans.some((sp) => spanHostsVisibleChild(sp.id, ctx));
            if (ctx.filtering && !showAllChildren && !ctx.matchers.stepVisible(rs) && !stepHostsChild) {
              return null;
            }
            const hasSpans = rs.spans.length > 0;
            const stepOpen = resolveExpanded(rs.step.id, ctx.userToggles, true);
            const stepSelected =
              ctx.selectedStepId === rs.step.id ||
              (!stepOpen && ctx.selectedSpanId != null && rs.spans.some((s) => s.id === ctx.selectedSpanId));
            const onStepClick = () =>
              rs.spans.length === 1
                ? ctx.onSelectSpan(turn.turn_id, rs.spans[0].id)
                : ctx.onSelectStep(turn.turn_id, rs.step.id);
            const maxSpanMs = Math.max(1, ...rs.spans.map((s) => durationMs(s) ?? 0));
            return (
              <div key={rs.step.id} data-step-id={rs.step.id}>
                <StepRow
                  rs={rs}
                  order={stepIndex + 1}
                  selected={stepSelected}
                  open={stepOpen}
                  hasSpans={hasSpans}
                  maxMs={maxStepMs}
                  siblings={trace.steps.length}
                  heat={ctx.heat}
                  dim={
                    ctx.highlight != null && nodeGroup(stepVisual(rs.step.kind.kind), rs.step.outcome) !== ctx.highlight
                  }
                  indent={offset + INDENT_STEP}
                  onToggle={() => ctx.onToggle(rs.step.id, stepOpen)}
                  onSelect={onStepClick}
                />
                {stepOpen &&
                  rs.spans.map((span, spanIndex) => {
                    if (
                      ctx.filtering &&
                      !showAllChildren &&
                      !ctx.matchers.spanMatch(span) &&
                      !spanHostsVisibleChild(span.id, ctx)
                    ) {
                      return null;
                    }
                    const pv = ctx.preview ? spanPreview(span, messages) : null;
                    return (
                      <div key={span.id}>
                        <SpanRow
                          span={span}
                          order={`${stepIndex + 1}.${spanIndex + 1}`}
                          selected={ctx.selectedSpanId === span.id}
                          interjected={ctx.interjectionSpanIds.has(span.id)}
                          maxMs={maxSpanMs}
                          siblings={rs.spans.length}
                          heat={ctx.heat}
                          dim={
                            ctx.highlight != null &&
                            nodeGroup(spanVisualOf(span), span.outcome, spanToolName(span)) !== ctx.highlight
                          }
                          indent={offset + INDENT_SPAN}
                          onSelect={() => ctx.onSelectSpan(turn.turn_id, span.id)}
                        />
                        {pv != null && pv !== '' && (
                          <div
                            className="pr-3 pb-1.5 text-[0.68rem] text-ink-soft font-mono italic truncate border-b border-b-black/10"
                            style={{ paddingLeft: offset + INDENT_SPAN }}
                            title={pv.slice(0, 400)}
                          >
                            {pv.slice(0, 160)}
                          </div>
                        )}
                        {/* The subagent's own trace, in place — the span that
                            spawned it is its parent row, not a link away. */}
                        <ChildSessions
                          children={ctx.forest.bySpan.get(span.id)}
                          offset={offset}
                          depth={depth}
                          ctx={ctx}
                        />
                      </div>
                    );
                  })}
              </div>
            );
          })}

          {/* Children whose spawning span was never recorded still belong to
              the turn, so they attach here rather than disappearing. */}
          <ChildSessions children={turnChildren} offset={offset} depth={depth} ctx={ctx} />
        </>
      )}
    </div>
  );
}

function TranscriptBody({
  nodes,
  live,
  turnId,
  showAllChildren,
  offset,
  ctx,
}: {
  nodes: TranscriptNode[];
  live: boolean;
  turnId: string;
  showAllChildren: boolean;
  offset: number;
  ctx: TreeCtx;
}) {
  if (nodes.length === 0) {
    return (
      <div className="flex items-center gap-2 py-1.5 text-ink-soft text-[0.72rem] italic" style={{ paddingLeft: offset + INDENT_STEP }}>
        {live && <RiLoader4Line className="animate-spin" />}
        {live ? 'external agent · waiting for its first message…' : 'external agent · no transcript recorded'}
      </div>
    );
  }
  return (
    <>
      {nodes.map((node) => {
        if (ctx.filtering && !showAllChildren) {
          // A transcript row has no outcome of its own, so "failures only"
          // can only reach it through its turn's status (`showAllChildren`).
          if (ctx.failuresOnly) return null;
          if (ctx.q && !transcriptText(node).includes(ctx.q)) return null;
        }
        const visual = TRANSCRIPT_VISUALS[node.kind];
        return (
          <TranscriptRow
            key={node.id}
            node={node}
            selected={ctx.selectedMessageId === node.id && ctx.selectedTurnId === turnId}
            showPreview={ctx.preview}
            dim={ctx.highlight != null && visual.group !== ctx.highlight}
            indent={offset + INDENT_STEP}
            onSelect={() => ctx.onSelectMessage(turnId, node.id)}
          />
        );
      })}
      {live && (
        <div
          className="flex items-center gap-2 py-1.5 text-info text-[0.7rem] font-bold uppercase tracking-wider"
          style={{ paddingLeft: offset + INDENT_STEP }}
        >
          <RiLoader4Line className="animate-spin" /> running…
        </div>
      )}
    </>
  );
}

export function TraceTree(props: TraceTreeProps) {
  const {
    overview,
    turnTraces,
    loadingTurns,
    userToggles,
    onToggle,
    selectedTurnId,
    selectedStepId,
    selectedSpanId,
    selectedMessageId,
    selectedSessionId,
    onSelectTurn,
    onSelectStep,
    onSelectSpan,
    onSelectMessage,
    onSelectSession,
    onOpenSession,
    interjectionCountByTurn,
    interjectionSpanIds,
    messageLog,
    highlight,
    filterRaw,
    onFilterRawChange,
    failuresOnly,
    onToggleFailures,
    filter,
    forest,
    childOverviews,
    loadingChildren,
  } = props;

  // Density controls — local UI state; they don't affect data loading.
  const [heat, setHeat] = useState(false);
  const [preview, setPreview] = useState(false);

  const q = filter.trim().toLowerCase();
  const filtering = q.length > 0 || failuresOnly;

  const matchers = useMemo(() => {
    const spanMatch = (span: Span) =>
      (!failuresOnly || attention(span.outcome)) && (!q || spanText(span).toLowerCase().includes(q));
    const stepSelfMatch = (rs: ReplayStep) =>
      (!failuresOnly || attention(rs.step.outcome)) && (!q || stepText(rs).toLowerCase().includes(q));
    const stepVisible = (rs: ReplayStep) => stepSelfMatch(rs) || rs.spans.some(spanMatch);
    return { spanMatch, stepVisible };
  }, [q, failuresOnly]);

  const childVisibleCache = new Map<string, boolean>();
  const ctx: TreeCtx = {
    childVisible: (child) => {
      const cached = childVisibleCache.get(child.session_id);
      if (cached !== undefined) return cached;
      // Seed against re-entry: a cycle would otherwise recurse through
      // `childSubtreeMatches` -> grandchildren -> back here.
      childVisibleCache.set(child.session_id, false);
      const textOk = q === '' || childSubtreeMatches(child, ctx, new Set());
      const failOk = !failuresOnly || sessionHasFailure(ctx.forest, child.session_id, turnTraces);
      const visible = textOk && failOk;
      childVisibleCache.set(child.session_id, visible);
      return visible;
    },
    turnTraces,
    loadingTurns,
    userToggles,
    onToggle,
    selectedTurnId,
    selectedStepId,
    selectedSpanId,
    selectedMessageId,
    selectedSessionId,
    onSelectTurn,
    onSelectStep,
    onSelectSpan,
    onSelectMessage,
    onSelectSession,
    onOpenSession,
    interjectionCountByTurn,
    interjectionSpanIds,
    highlight,
    heat,
    preview,
    filtering,
    failuresOnly,
    q,
    matchers,
    forest,
    childOverviews,
    loadingChildren,
  };

  const subagents = forest.byId.size;

  return (
    <div className="flex-1 min-w-0 border-r-[3px] border-black bg-canvas flex flex-col z-10">
      <div className="px-3 py-2 border-b-2 border-black bg-canvas flex flex-col gap-2">
        <div className="flex items-center gap-2 flex-wrap">
          <SearchBox
            className="h-8"
            placeholder="Filter by kind, tool, model…"
            value={filterRaw}
            onChange={(e) => onFilterRawChange(e.target.value)}
          />
          <Toggle on={failuresOnly} onClick={onToggleFailures} title="Show only the failure path">
            Failures
          </Toggle>
          <Toggle on={heat} onClick={() => setHeat((v) => !v)} title="Tint rows by latency, relative to their siblings">
            Heat
          </Toggle>
          <Toggle on={preview} onClick={() => setPreview((v) => !v)} title="Show a one-line input/output preview under each span">
            I/O
          </Toggle>
        </div>
        <div className="text-[0.62rem] font-bold uppercase tracking-wider text-ink-soft">
          {overview.turns.length} {overview.turns.length === 1 ? 'turn' : 'turns'}
          {subagents > 0 && ` · ${subagents} subagent${subagents === 1 ? '' : 's'}`}
          {filtering && ' · filtered'}
        </div>
      </div>

      <div className="flex-1 overflow-y-auto">
        <SessionTurns
          turns={overview.turns}
          messages={messageLog}
          externalAgent={overview.external_agent}
          offset={0}
          depth={0}
          ctx={ctx}
        />
      </div>
    </div>
  );
}
