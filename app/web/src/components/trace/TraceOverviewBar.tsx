/**
 * A full-width "trace overview" strip above the tree — bench-web's FlowRail
 * glance header: a clickable sequence minimap (one cell per step/span across the
 * whole session, colour-coded by kind, red for failures), token totals, wall-
 * clock, and per-tool call counts. Clicking a cell selects that node and scrolls
 * the tree to it. Aggregates from turn summaries (tokens, always available) plus
 * whatever turn step-trees have loaded (cells, counts, tools).
 */
import { useMemo, useState } from 'react';
import { RiArrowDownSLine, RiArrowRightSLine } from 'react-icons/ri';
import type { LifecycleState, TraceOverview, TraceTurnSummary, TurnTrace } from '../../types/trace';
import type { TraceGroup } from './traceFormat';
import {
  formatDuration,
  formatTok,
  nodeGroup,
  spanToolName,
  spanVisualOf,
  stepVisual,
  summaryTokens,
  TRACE_LEGEND,
} from './traceFormat';
import { scrollToAnchor } from './scrollToAnchor';

interface Cell {
  key: string;
  turnId: string;
  stepId: string;
  spanId: string | null;
  cellClass: string;
  failed: boolean;
  group: TraceGroup;
  label: string;
}

interface Aggregate {
  cells: Cell[];
  stepCount: number;
  spanCount: number;
  toolCounts: [string, number][];
}

function isFail(outcome: LifecycleState['outcome']): boolean {
  return outcome === 'failed' || outcome === 'cancelled';
}

function buildAggregate(overview: TraceOverview, turnTraces: Map<string, TurnTrace>): Aggregate {
  const cells: Cell[] = [];
  const tools = new Map<string, number>();
  let stepCount = 0;
  let spanCount = 0;

  for (const turn of overview.turns) {
    const trace = turnTraces.get(turn.turn_id);
    if (!trace) continue;
    for (const rs of trace.steps) {
      stepCount += 1;
      if (rs.step.kind.kind === 'llm_iteration') {
        for (const span of rs.spans) {
          spanCount += 1;
          const v = spanVisualOf(span);
          const failed = isFail(span.outcome.outcome);
          let label = v.label;
          if (span.kind.kind === 'llm_call') {
            label = span.kind.begin.model_id;
          } else {
            label = span.kind.begin.tool_name;
            tools.set(label, (tools.get(label) ?? 0) + 1);
          }
          cells.push({
            key: span.id,
            turnId: turn.turn_id,
            stepId: rs.step.id,
            spanId: span.id,
            cellClass: failed ? 'bg-err' : v.cell,
            failed,
            group: nodeGroup(v, span.outcome, spanToolName(span)),
            label,
          });
        }
      } else {
        spanCount += rs.spans.length;
        const v = stepVisual(rs.step.kind.kind);
        // Fail if the step OR any nested span failed — matches the tree's
        // roll-up (traceTreeModel.failureCount) so the overview's dots agree.
        const failed = isFail(rs.step.outcome.outcome) || rs.spans.some((s) => isFail(s.outcome.outcome));
        // Mirror the tree's row-click (onStepClick): a single-span step selects
        // that span, so the cell must carry the span id — otherwise clicking the
        // cell vs the row would select different nodes and the tree selection
        // wouldn't light this cell.
        const single = rs.spans.length === 1 ? rs.spans[0] : null;
        cells.push({
          key: rs.step.id,
          turnId: turn.turn_id,
          stepId: rs.step.id,
          spanId: single ? single.id : null,
          cellClass: failed ? 'bg-err' : v.cell,
          failed,
          group: failed ? 'failed' : v.group,
          label: v.label,
        });
      }
    }
  }

  const toolCounts = [...tools.entries()].sort((a, b) => (b[1] - a[1] !== 0 ? b[1] - a[1] : a[0].localeCompare(b[0])));
  return { cells, stepCount, spanCount, toolCounts };
}

// Wall-clock across the session: earliest start → latest end, using `now` as the
// upper bound while any turn is still running (so it doesn't under-report or
// vanish mid-session, matching the header's live per-turn duration).
function overallDuration(turns: TraceTurnSummary[]): number | null {
  let minS = Infinity;
  let maxE = -Infinity;
  let running = false;
  for (const j of turns) {
    if (j.started_at == null) continue;
    minS = Math.min(minS, new Date(j.started_at).getTime());
    if (j.ended_at != null) maxE = Math.max(maxE, new Date(j.ended_at).getTime());
    else running = true;
  }
  if (minS === Infinity) return null;
  const end = running ? Date.now() : maxE;
  if (end === -Infinity) return null;
  return Math.max(0, end - minS);
}

function Stat({ label, value, accent }: { label: string; value: string; accent?: string }) {
  return (
    <span className="inline-flex items-baseline gap-1 font-mono">
      <span className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold">{label}</span>
      <span className={`text-[0.8rem] font-bold ${accent ?? 'text-ink'}`}>{value}</span>
    </span>
  );
}

export function TraceOverviewBar({
  overview,
  turnTraces,
  loadingTurns,
  highlight,
  onHighlight,
  selectedSpanId,
  selectedStepId,
  onSelectSpan,
  onSelectStep,
}: {
  overview: TraceOverview;
  turnTraces: Map<string, TurnTrace>;
  loadingTurns: Set<string>;
  /** Legend group to highlight; non-matching cells dim. `null` = no highlight. */
  highlight: TraceGroup | null;
  onHighlight: (g: TraceGroup | null) => void;
  selectedSpanId: string | null;
  selectedStepId: string | null;
  onSelectSpan: (turnId: string, spanId: string) => void;
  onSelectStep: (turnId: string, stepId: string) => void;
}) {
  const [open, setOpen] = useState(true);

  const { cells, stepCount, spanCount, toolCounts } = useMemo(
    () => buildAggregate(overview, turnTraces),
    [overview, turnTraces],
  );

  const tokens = useMemo(() => {
    let input = 0;
    let output = 0;
    for (const j of overview.turns) {
      const t = summaryTokens(j);
      input += t.inputTotal;
      output += t.output;
    }
    return { input, output };
  }, [overview]);

  const dur = overallDuration(overview.turns);
  const failCells = cells.filter((c) => c.failed);

  const jump = (c: Cell) => {
    if (c.spanId != null) onSelectSpan(c.turnId, c.spanId);
    else onSelectStep(c.turnId, c.stepId);
    scrollToAnchor(`[data-step-id="${c.stepId}"]`, 'center');
  };

  return (
    <div className="shrink-0 border-b-[3px] border-black bg-canvas px-5 py-2">
      <div className="flex items-center gap-3 flex-wrap">
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="flex items-center gap-1 text-[0.7rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
        >
          {open ? <RiArrowDownSLine /> : <RiArrowRightSLine />}
          Trace overview
        </button>
        <Stat label="turns" value={String(overview.turns.length)} />
        <Stat label="steps" value={String(stepCount)} />
        <Stat label="spans" value={String(spanCount)} />
        <Stat label="↑" value={formatTok(tokens.input)} />
        <Stat label="↓" value={formatTok(tokens.output)} />
        {dur !== null && <Stat label="dur" value={formatDuration(dur)} />}
        {failCells.length > 0 && <Stat label="fail" value={String(failCells.length)} accent="text-err" />}
        {loadingTurns.size > 0 && (
          <span className="text-[0.62rem] font-mono text-ink-soft italic">loading {loadingTurns.size}…</span>
        )}
      </div>

      {open && cells.length > 0 && (
        <div className="mt-2 flex items-start gap-2">
          <span className="text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft shrink-0 pt-1 w-14">sequence</span>
          <div className="flex flex-wrap gap-0.5 max-h-20 overflow-y-auto">
            {cells.map((c) => {
              const selected =
                c.spanId != null ? c.spanId === selectedSpanId : c.stepId === selectedStepId;
              return (
                <button
                  key={c.key}
                  type="button"
                  title={`${c.label}${c.failed ? ' · failed' : ''}`}
                  onClick={() => jump(c)}
                  className={`h-3.5 w-2.5 rounded-[2px] border border-black/40 cursor-pointer transition-opacity ${
                    c.cellClass
                  } ${selected ? 'ring-2 ring-black ring-offset-1' : ''} ${
                    highlight != null && c.group !== highlight ? 'opacity-20' : ''
                  }`}
                />
              );
            })}
          </div>
          {failCells.length > 0 && (
            <button
              type="button"
              title="Jump to the first failure"
              onClick={() => jump(failCells[0])}
              className="shrink-0 text-[0.65rem] font-bold text-err hover:underline cursor-pointer pt-0.5"
            >
              {failCells.length} fail →
            </button>
          )}
        </div>
      )}

      {open && toolCounts.length > 0 && (
        <div className="mt-1.5 flex items-start gap-2 flex-wrap">
          <span className="text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft shrink-0 pt-0.5 w-14">tools</span>
          <div className="flex flex-wrap gap-1">
            {toolCounts.map(([name, count]) => (
              <span
                key={name}
                className="inline-flex items-center gap-1 border border-black/30 rounded px-1 py-px text-[0.65rem] font-mono bg-white"
              >
                <span className="text-ink font-bold">{name}</span>
                <span className="text-ink-soft">{count}</span>
              </span>
            ))}
          </div>
        </div>
      )}

      {open && (
        <div className="mt-1.5 flex items-center gap-x-3 gap-y-1 flex-wrap text-[0.6rem] text-ink-soft font-mono">
          <span className="font-bold uppercase tracking-wider w-14 shrink-0">legend</span>
          {TRACE_LEGEND.map((l) => {
            const on = highlight === l.group;
            return (
              <button
                key={l.group}
                type="button"
                onClick={() => onHighlight(on ? null : l.group)}
                title={on ? `Showing all — click to clear` : `Highlight ${l.label} nodes`}
                className={`inline-flex items-center gap-1 rounded px-1 cursor-pointer transition-colors ${
                  on ? 'bg-ink text-canvas font-bold' : 'hover:bg-black/5'
                }`}
              >
                <span className={`inline-block h-2.5 w-2.5 rounded-[2px] border border-black/40 ${l.cell}`} />
                {l.label}
              </button>
            );
          })}
          {highlight != null && (
            <button
              type="button"
              onClick={() => onHighlight(null)}
              className="font-bold uppercase tracking-wider text-brand-hover hover:underline cursor-pointer"
            >
              clear
            </button>
          )}
        </div>
      )}
    </div>
  );
}
