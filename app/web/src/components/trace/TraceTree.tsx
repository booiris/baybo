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
import { RiArrowDownSLine, RiArrowRightSLine, RiCornerDownLeftLine, RiLoader4Line } from 'react-icons/ri';
import type {
  LifecycleState,
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
  durationMs,
  formatDuration,
  formatTok,
  OutcomeBadge,
  stepSummaryText,
  stepVisual,
  stripeClass,
  sumLlmTokens,
  summaryTokens,
  traceTokens,
  turnDurationMs,
} from './traceFormat';
import type { TraceGroup } from './traceFormat';
import { nodeGroup, spanToolName, spanVisualOf } from './traceFormat';
import type { TurnRollup } from './traceTreeModel';
import { attention, isExternalAgentTurn, isTurnLive, resolveExpanded, turnLabels, turnRollup } from './traceTreeModel';

const INDENT_TURN = 8;
const INDENT_STEP = 26;
const INDENT_SPAN = 46;

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
  onSelectTurn: (turnId: string) => void;
  onSelectStep: (turnId: string, stepId: string) => void;
  onSelectSpan: (turnId: string, spanId: string) => void;
  interjectionCountByTurn: Map<string, number>;
  interjectionSpanIds: Set<string>;
  /** Session transcript — needed to resolve transcript-backed tool outputs for
   *  the inline I/O preview. */
  messageLog: SessionMessageRow[];
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

function spanText(span: Span): string {
  const v = spanVisualOf(span).label;
  if (span.kind.kind === 'llm_call') return `${v} ${span.kind.begin.model_id}`;
  return `${v} ${span.kind.begin.tool_name}`;
}

function stepText(rs: ReplayStep): string {
  return `${stepVisual(rs.step.kind.kind).label} ${rs.step.kind.kind} ${stepSummaryText(rs.step, rs.spans)}`;
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

function SpanRow({
  span,
  selected,
  interjected,
  maxMs,
  siblings,
  heat,
  dim,
  onSelect,
}: {
  span: Span;
  selected: boolean;
  interjected: boolean;
  maxMs: number;
  siblings: number;
  dim: boolean;
  heat: boolean;
  onSelect: () => void;
}) {
  const visual = spanVisualOf(span);
  const ms = durationMs(span);

  let title = '';
  let subtitle = visual.label;
  if (span.kind.kind === 'llm_call') {
    title = span.kind.begin.model_id;
    if (span.kind.result) {
      const cached = span.kind.result.cached_input_tokens ?? 0;
      subtitle = `${span.kind.result.input_tokens ?? 0} in / ${span.kind.result.output_tokens ?? 0} out${
        cached > 0 ? ` (${cached} cached)` : ''
      }`;
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
    <div {...rowInteractive(onSelect)} className={rowShell(selected, stripeClass(visual), tint, dim)} style={{ paddingLeft: INDENT_SPAN }}>
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
  selected,
  open,
  hasSpans,
  maxMs,
  siblings,
  heat,
  dim,
  onToggle,
  onSelect,
}: {
  rs: ReplayStep;
  selected: boolean;
  open: boolean;
  hasSpans: boolean;
  maxMs: number;
  siblings: number;
  dim: boolean;
  heat: boolean;
  onToggle: () => void;
  onSelect: () => void;
}) {
  const visual = stepVisual(rs.step.kind.kind);
  const summary = stepSummaryText(rs.step, rs.spans);
  const ms = durationMs(rs.step);
  const tokens = sumLlmTokens(rs.spans);
  const tint = heat && !selected ? heatTint(ms, maxMs, siblings) : '';

  return (
    <div {...rowInteractive(onSelect)} className={rowShell(selected, stripeClass(visual), tint, dim)} style={{ paddingLeft: INDENT_STEP }}>
      {hasSpans ? <Chevron open={open} onClick={onToggle} /> : <span className="shrink-0 w-5" />}
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
  onToggle: () => void;
  onSelect: () => void;
}) {
  const tokens = trace ? traceTokens(trace) : summaryTokens(turn);
  const dur = turnDurationMs(turn, trace);
  const live = isTurnLive(turn.turn_status_kind);

  return (
    <div {...rowInteractive(onSelect)} className={rowShell(selected, 'border-l-brand', '')} style={{ paddingLeft: INDENT_TURN }}>
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
    onSelectTurn,
    onSelectStep,
    onSelectSpan,
    interjectionCountByTurn,
    interjectionSpanIds,
    messageLog,
    highlight,
    filterRaw,
    onFilterRawChange,
    failuresOnly,
    onToggleFailures,
    filter,
  } = props;

  // Density controls — local UI state; they don't affect data loading.
  const [heat, setHeat] = useState(false);
  const [preview, setPreview] = useState(false);

  const q = filter.trim().toLowerCase();
  const labels = useMemo(() => turnLabels(overview.turns), [overview.turns]);
  const filtering = q.length > 0 || failuresOnly;

  const matchers = useMemo(() => {
    const spanMatch = (span: Span) =>
      (!failuresOnly || attention(span.outcome)) && (!q || spanText(span).toLowerCase().includes(q));
    const stepSelfMatch = (rs: ReplayStep) =>
      (!failuresOnly || attention(rs.step.outcome)) && (!q || stepText(rs).toLowerCase().includes(q));
    const stepVisible = (rs: ReplayStep) => stepSelfMatch(rs) || rs.spans.some(spanMatch);
    return { spanMatch, stepVisible };
  }, [q, failuresOnly]);

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
          {filtering && ' · filtered'}
        </div>
      </div>

      <div className="flex-1 overflow-y-auto">
        {overview.turns.map((turn, i) => {
          const trace = turnTraces.get(turn.turn_id);
          const loading = loadingTurns.has(turn.turn_id);
          const rollup = turnRollup(turn, trace);
          const maxStepMs = trace ? Math.max(1, ...trace.steps.map((rs) => durationMs(rs.step) ?? 0)) : 1;

          let showAllChildren = false;
          if (filtering) {
            const turnShallow =
              (!failuresOnly || rollup.hasFailure) && (!q || turnText(labels[i].long, turn).toLowerCase().includes(q));
            const turnDeep = trace ? trace.steps.some(matchers.stepVisible) : false;
            const statusOnly = failuresOnly && !q && rollup.hasFailure && !!trace && !turnDeep;
            const turnVisible = turnShallow || turnDeep || statusOnly || loading;
            if (!turnVisible) return null;
            showAllChildren = statusOnly;
          }

          // Everything is expanded by default (nothing hidden — the bench-web
          // "show the whole flow" model); a chevron records an explicit collapse
          // that `resolveExpanded` honours.
          const turnOpen = resolveExpanded(turn.turn_id, userToggles, true);
          const turnSelected =
            selectedTurnId === turn.turn_id && selectedStepId == null && selectedSpanId == null;
          const emptyTrace = !!trace && trace.steps.length === 0;

          return (
            <div key={turn.turn_id} data-turn-id={turn.turn_id}>
              <TurnRow
                turn={turn}
                label={labels[i].long}
                trace={trace}
                loading={loading}
                selected={turnSelected}
                open={turnOpen}
                rollup={rollup}
                interjections={interjectionCountByTurn.get(turn.turn_id) ?? 0}
                onToggle={() => onToggle(turn.turn_id, turnOpen)}
                onSelect={() => onSelectTurn(turn.turn_id)}
              />
              {turnOpen && (
                <>
                  {loading && !trace && (
                    <div
                      className="flex items-center gap-2 py-1.5 text-ink-soft text-[0.75rem] italic"
                      style={{ paddingLeft: INDENT_STEP }}
                    >
                      <RiLoader4Line className="animate-spin" /> Loading turn…
                    </div>
                  )}
                  {emptyTrace && (
                    <div className="py-1.5 text-ink-soft text-[0.72rem] italic" style={{ paddingLeft: INDENT_STEP }}>
                      {isExternalAgentTurn(trace, turn.turn_status_kind)
                        ? 'external agent · no step tree — select the turn for its transcript'
                        : isTurnLive(turn.turn_status_kind)
                          ? 'no steps recorded yet…'
                          : 'no steps recorded'}
                    </div>
                  )}
                  {trace &&
                    trace.steps.map((rs) => {
                      if (filtering && !showAllChildren && !matchers.stepVisible(rs)) return null;
                      const hasSpans = rs.spans.length > 0;
                      const stepOpen = resolveExpanded(rs.step.id, userToggles, true);
                      const stepSelected =
                        selectedStepId === rs.step.id ||
                        (!stepOpen && selectedSpanId != null && rs.spans.some((s) => s.id === selectedSpanId));
                      const onStepClick = () =>
                        rs.spans.length === 1
                          ? onSelectSpan(turn.turn_id, rs.spans[0].id)
                          : onSelectStep(turn.turn_id, rs.step.id);
                      const maxSpanMs = Math.max(1, ...rs.spans.map((s) => durationMs(s) ?? 0));
                      return (
                        <div key={rs.step.id} data-step-id={rs.step.id}>
                          <StepRow
                            rs={rs}
                            selected={stepSelected}
                            open={stepOpen}
                            hasSpans={hasSpans}
                            maxMs={maxStepMs}
                            siblings={trace.steps.length}
                            heat={heat}
                            dim={highlight != null && nodeGroup(stepVisual(rs.step.kind.kind), rs.step.outcome) !== highlight}
                            onToggle={() => onToggle(rs.step.id, stepOpen)}
                            onSelect={onStepClick}
                          />
                          {stepOpen &&
                            rs.spans.map((span) => {
                              if (filtering && !showAllChildren && !matchers.spanMatch(span)) return null;
                              const pv = preview ? spanPreview(span, messageLog) : null;
                              return (
                                <div key={span.id}>
                                  <SpanRow
                                    span={span}
                                    selected={selectedSpanId === span.id}
                                    interjected={interjectionSpanIds.has(span.id)}
                                    maxMs={maxSpanMs}
                                    siblings={rs.spans.length}
                                    heat={heat}
                                    dim={
                                      highlight != null &&
                                      nodeGroup(spanVisualOf(span), span.outcome, spanToolName(span)) !== highlight
                                    }
                                    onSelect={() => onSelectSpan(turn.turn_id, span.id)}
                                  />
                                  {pv != null && pv !== '' && (
                                    <div
                                      className="pr-3 pb-1.5 text-[0.68rem] text-ink-soft font-mono italic truncate border-b border-b-black/10"
                                      style={{ paddingLeft: INDENT_SPAN }}
                                      title={pv.slice(0, 400)}
                                    >
                                      {pv.slice(0, 160)}
                                    </div>
                                  )}
                                </div>
                              );
                            })}
                        </div>
                      );
                    })}
                </>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
