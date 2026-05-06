import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  RiArrowLeftLine,
  RiArchiveLine,
  RiBookmark3Line,
  RiBrainLine,
  RiCornerDownRightLine,
  RiCpuLine,
  RiLoader4Line,
  RiRefreshLine,
  RiSave3Line,
  RiSearchEyeLine,
  RiTeamLine,
  RiToolsLine,
  RiArrowRightLine,
} from 'react-icons/ri';
import type { IconType } from 'react-icons';
import { IconButton } from '../components/IconButton';
import { Button } from '../components/Button';
import { useAdminClient, useAuth } from '../api/auth';
import { getMockSessionReplay } from '../api/mock';
import { useMockMode } from '../api/mock';
import type {
  LifecycleState,
  ReplayJob,
  ReplayStep,
  SecretKind,
  SessionReplay,
  Span,
  SpanEvent,
  SpanKindTag,
  Step,
  StepKindTag,
} from '../types/trace';
import { isTerminal } from '../types/trace';
import { MessageList } from '../components/trace/MessageList';
import { renderWithSanitizeChips, SanitizeChip } from '../components/trace/SanitizeChip';

const POLL_ACTIVE_MS = 2_000;
const POLL_TERMINAL_MS = 10_000;

// ── Visual mapping per StepKind ──────────────────────────────────────

interface KindVisual {
  icon: IconType;
  accent: string; // tailwind text color
  bg: string; // tailwind bg color (for the icon disc)
  label: string;
}

const STEP_VISUALS: Record<StepKindTag, KindVisual> = {
  llm_iteration: {
    icon: RiBrainLine,
    accent: 'text-brand',
    bg: 'bg-brand/10',
    label: 'LLM iteration',
  },
  compression: {
    icon: RiArchiveLine,
    accent: 'text-ink-soft',
    bg: 'bg-gray-100',
    label: 'Compression',
  },
  memory_recall: {
    icon: RiSearchEyeLine,
    accent: 'text-info',
    bg: 'bg-info/10',
    label: 'Memory recall',
  },
  memory_write: {
    icon: RiSave3Line,
    accent: 'text-ok',
    bg: 'bg-ok/10',
    label: 'Memory write',
  },
  skill_selection: {
    icon: RiBookmark3Line,
    accent: 'text-ok',
    bg: 'bg-ok/10',
    label: 'Skill selection',
  },
  subagent: {
    icon: RiTeamLine,
    accent: 'text-brand',
    bg: 'bg-brand/10',
    label: 'Subagent',
  },
};

const SPAN_VISUALS: Record<SpanKindTag, KindVisual> = {
  llm_call: {
    icon: RiBrainLine,
    accent: 'text-brand',
    bg: 'bg-brand/10',
    label: 'LLM call',
  },
  tool_call: {
    icon: RiToolsLine,
    accent: 'text-warn',
    bg: 'bg-warn/10',
    label: 'Tool call',
  },
  subagent_stub: {
    icon: RiTeamLine,
    accent: 'text-brand',
    bg: 'bg-brand/10',
    label: 'Subagent stub',
  },
};

// ── Utilities ────────────────────────────────────────────────────────

function durationMs(span: { started_at: string; ended_at?: string | null }): number | null {
  if (!span.ended_at) return null;
  return Math.max(0, new Date(span.ended_at).getTime() - new Date(span.started_at).getTime());
}

function formatDuration(ms: number | null): string {
  if (ms === null) return '…';
  if (ms < 1_000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1_000).toFixed(2)} s`;
  return `${(ms / 60_000).toFixed(2)} min`;
}

function formatTime(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}

function outcomeColor(state: LifecycleState): string {
  switch (state.outcome) {
    case 'pending':
      return 'text-info';
    case 'ok':
      return 'text-ok';
    case 'failed':
      return 'text-err';
    case 'cancelled':
      return 'text-ink-soft';
  }
}

function outcomeLabel(state: LifecycleState): string {
  switch (state.outcome) {
    case 'pending':
      return 'pending';
    case 'ok':
      return 'ok';
    case 'failed':
      return 'failed';
    case 'cancelled':
      return `cancelled (${state.reason})`;
  }
}

function OutcomeBadge({ state }: { state: LifecycleState }) {
  const cls = outcomeColor(state);
  return (
    <span className={`inline-flex items-center text-[0.7rem] font-bold uppercase tracking-wider ${cls}`}>
      {state.outcome === 'pending' && (
        <span className="inline-block w-2 h-2 rounded-full bg-info animate-pulse mr-1.5" />
      )}
      {outcomeLabel(state)}
    </span>
  );
}

// ── Step summary text per StepKind ───────────────────────────────────

function stepSummaryText(step: Step, spans: Span[]): string {
  const k = step.kind.kind;
  switch (k) {
    case 'llm_iteration': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      if (llm && llm.kind.kind === 'llm_call') {
        const model = llm.kind.begin.model_id;
        const tools = spans.filter((s) => s.kind.kind === 'tool_call').length;
        return tools > 0 ? `${model} • ${tools} tool ${tools === 1 ? 'call' : 'calls'}` : model;
      }
      return 'llm iteration';
    }
    case 'compression': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      if (llm && llm.kind.kind === 'llm_call' && llm.kind.result) {
        const inT = llm.kind.result.input_tokens ?? 0;
        const outT = llm.kind.result.output_tokens ?? 0;
        return `${inT.toLocaleString()} → ${outT.toLocaleString()} tokens`;
      }
      return 'compression';
    }
    case 'memory_recall': {
      const tool = spans.find((s) => s.kind.kind === 'tool_call');
      if (tool && tool.kind.kind === 'tool_call') {
        return `recall via ${tool.kind.begin.tool_name}`;
      }
      return 'memory recall';
    }
    case 'memory_write':
      return `wrote ${spans.length} memor${spans.length === 1 ? 'y' : 'ies'}`;
    case 'skill_selection': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      if (llm && llm.kind.kind === 'llm_call' && llm.kind.result?.output_content) {
        return llm.kind.result.output_content.slice(0, 80);
      }
      return 'skill selection';
    }
    case 'subagent':
      return `child ${step.kind.child_session_id}`;
  }
}

// ── Span list with pairing connector + parallel chip ─────────────────

function SpanRow({
  span,
  selected,
  onSelect,
}: {
  span: Span;
  selected: boolean;
  onSelect: (id: string) => void;
}) {
  const visual = SPAN_VISUALS[span.kind.kind];
  const Icon = visual.icon;
  const ms = durationMs(span);

  let title = '';
  let subtitle = '';
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
  } else if (span.kind.kind === 'tool_call') {
    title = span.kind.begin.tool_name;
    if (!span.kind.result) {
      subtitle = 'in flight';
    } else if (span.kind.result.success) {
      subtitle = 'success';
    } else {
      // Surface the structured outcome reason when present — the row
      // is otherwise the only place a glanceable error sits.
      subtitle =
        span.outcome.outcome === 'failed' && span.outcome.reason
          ? `failed: ${span.outcome.reason}`
          : 'failed';
    }
  } else {
    title = `subagent → ${span.kind.child_session_id}`;
    subtitle = 'wait window';
  }

  return (
    <button
      type="button"
      onClick={() => onSelect(span.id)}
      className={`w-full text-left flex items-center gap-3 px-3 py-2 border-2 border-black rounded-md transition-all ${
        selected
          ? 'bg-brand/10 border-brand shadow-brutal-xs'
          : 'bg-white hover:bg-gray-50 hover:shadow-brutal-xs'
      }`}
    >
      <div
        className={`w-8 h-8 rounded-full border-2 border-black flex items-center justify-center shrink-0 ${visual.bg}`}
      >
        <Icon className={`${visual.accent} text-base`} />
      </div>
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 min-w-0">
          <span className="font-bold uppercase tracking-wide text-[0.85rem] truncate">{title}</span>
          {span.parallel_group && (
            <span className="text-[0.7rem] font-bold uppercase tracking-wider text-warn border-2 border-warn rounded px-1">
              ‖ parallel
            </span>
          )}
        </div>
        <div className="flex items-center gap-3 text-[0.75rem] text-ink-soft font-mono mt-0.5">
          <span>{visual.label}</span>
          <span>•</span>
          <span>{subtitle}</span>
        </div>
      </div>
      <div className="shrink-0 text-right">
        <div className="text-[0.75rem] font-mono text-ink">{formatDuration(ms)}</div>
        <div className="mt-0.5">
          <OutcomeBadge state={span.outcome} />
        </div>
      </div>
    </button>
  );
}

function StepBlock({
  rs,
  selectedSpanId,
  onSelect,
}: {
  rs: ReplayStep;
  selectedSpanId: string | null;
  onSelect: (id: string) => void;
}) {
  const { step, spans } = rs;
  const visual = STEP_VISUALS[step.kind.kind];
  const Icon = visual.icon;
  const summary = stepSummaryText(step, spans);
  const ms = durationMs(step);

  // Sibling spans, sorted by started_at (the wire already does so but
  // be defensive).
  const sortedSpans = useMemo(
    () =>
      [...spans].sort(
        (a, b) => new Date(a.started_at).getTime() - new Date(b.started_at).getTime(),
      ),
    [spans],
  );

  // Group consecutive parallel-group siblings to render the "‖" rail.
  return (
    <div className="bg-white border-[3px] border-black rounded-md shadow-brutal flex flex-col">
      <div className="flex items-center gap-3 px-4 py-3 border-b-2 border-black">
        <div
          className={`w-9 h-9 rounded-full border-2 border-black flex items-center justify-center shrink-0 ${visual.bg}`}
        >
          <Icon className={`${visual.accent} text-lg`} />
        </div>
        <div className="flex-1 min-w-0">
          <div className="flex items-center gap-2">
            <span className="font-bold uppercase tracking-wider text-[0.85rem]">{visual.label}</span>
            <OutcomeBadge state={step.outcome} />
          </div>
          <div className="text-[0.85rem] text-ink-soft font-mono truncate">{summary}</div>
        </div>
        <div className="shrink-0 text-right">
          <div className="text-[0.75rem] text-ink-soft font-mono">{formatTime(step.started_at)}</div>
          <div className="text-[0.75rem] text-ink font-mono">{formatDuration(ms)}</div>
        </div>
      </div>
      <div className="px-4 py-3 bg-canvas flex flex-col gap-2">
        {sortedSpans.length === 0 && (
          <div className="text-ink-soft text-[0.85rem] italic">No spans yet…</div>
        )}
        {sortedSpans.map((span) => (
          <SpanRow
            key={span.id}
            span={span}
            selected={selectedSpanId === span.id}
            onSelect={onSelect}
          />
        ))}
      </div>
    </div>
  );
}

// ── Right-hand detail panel (per-kind) ───────────────────────────────

function SanitizeKindHint(events: SpanEvent[] | undefined): SecretKind | undefined {
  for (const e of events ?? []) {
    if (e.kind.kind === 'sanitize_hit' && e.kind.kinds.length > 0) {
      return e.kind.kinds[0];
    }
  }
  return undefined;
}

function LlmCallDetail({ span }: { span: Span }) {
  if (span.kind.kind !== 'llm_call') return null;
  const { begin, result } = span.kind;
  const hint = SanitizeKindHint(span.events);

  return (
    <div className="space-y-6">
      <section>
        <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
          Input messages
        </h4>
        <MessageList messages={begin.input_messages} kindHint={hint} />
      </section>

      {result && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Output
          </h4>
          {result.output_content && (
            <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
              {renderWithSanitizeChips(result.output_content, hint)}
            </pre>
          )}
          {result.thinking && (
            <details className="mt-2">
              <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
                Thinking ({result.thinking.length.toLocaleString()} chars)
              </summary>
              <pre className="mt-1 whitespace-pre-wrap break-words font-mono text-[0.8rem] italic bg-gray-50 border-2 border-black rounded-md p-3">
                {result.thinking}
              </pre>
            </details>
          )}
          {result.tool_calls && result.tool_calls.length > 0 && (
            <div className="mt-3 space-y-2">
              <div className="text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
                Emitted tool calls
              </div>
              {result.tool_calls.map((tc) => (
                <div
                  key={tc.id}
                  className="border-2 border-black rounded-md p-2 bg-canvas font-mono text-[0.8rem]"
                >
                  <div className="text-ink-soft">
                    {tc.id} → <span className="text-brand font-bold">{tc.name}</span>
                  </div>
                  <pre className="mt-1 whitespace-pre-wrap break-all">
                    {JSON.stringify(tc.arguments, null, 2)}
                  </pre>
                </div>
              ))}
            </div>
          )}
        </section>
      )}
    </div>
  );
}

function ToolCallDetail({
  span,
  onJumpToLlm,
}: {
  span: Span;
  onJumpToLlm: (llmSpanId: string) => void;
}) {
  if (span.kind.kind !== 'tool_call') return null;
  const { begin, result } = span.kind;
  const hint = SanitizeKindHint(span.events);
  const failureReason =
    span.outcome.outcome === 'failed' ? span.outcome.reason : null;

  return (
    <div className="space-y-6">
      <section>
        <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
          Tool
        </h4>
        <div className="font-mono text-[0.95rem] font-bold">{begin.tool_name}</div>
        {begin.triggered_by && (
          <button
            type="button"
            onClick={() => onJumpToLlm(begin.triggered_by!.llm_span_id)}
            className="mt-2 inline-flex items-center gap-1 text-[0.75rem] uppercase font-bold tracking-wider text-brand hover:underline cursor-pointer"
          >
            <RiCornerDownRightLine />
            Triggered by LLM span ({begin.triggered_by.tool_use_id})
          </button>
        )}
      </section>

      {failureReason && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-err pb-1 text-err">
            Failure reason
          </h4>
          <div className="font-mono text-[0.85rem] bg-err/5 border-2 border-err rounded-md p-3 whitespace-pre-wrap break-words text-err">
            {failureReason}
          </div>
        </section>
      )}

      <section>
        <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
          Params
        </h4>
        <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
          {renderWithSanitizeChips(JSON.stringify(begin.params, null, 2), hint)}
        </pre>
      </section>

      {result && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Output {result.success ? '(success)' : '(failed)'}
          </h4>
          <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
            {renderWithSanitizeChips(
              typeof result.output === 'string'
                ? result.output
                : JSON.stringify(result.output, null, 2),
              hint,
            )}
          </pre>
        </section>
      )}
    </div>
  );
}

function SubagentStubDetail({
  span,
  onDrillIn,
}: {
  span: Span;
  onDrillIn: (sessionId: string) => void;
}) {
  if (span.kind.kind !== 'subagent_stub') return null;
  return (
    <div className="space-y-4">
      <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
        Subagent
      </h4>
      <p className="text-[0.85rem] text-ink-soft">
        This stub bounds the parent's wait window. The actual work runs in the child session.
      </p>
      <Button
        variant="primary"
        onClick={() => onDrillIn(span.kind.kind === 'subagent_stub' ? span.kind.child_session_id : '')}
        className="!py-2 !px-4 !text-[0.85rem] gap-2"
      >
        Open child session <RiArrowRightLine />
      </Button>
    </div>
  );
}

function MetaTab({ span }: { span: Span }) {
  const ms = durationMs(span);
  const visual = SPAN_VISUALS[span.kind.kind];
  const baseRows: [string, React.ReactNode][] = [
    ['Span ID', <code className="break-all">{span.id}</code>],
    ['Step ID', <code className="break-all">{span.step_id}</code>],
    ['Kind', visual.label],
    ['Status', <OutcomeBadge state={span.outcome} />],
    ['Duration', formatDuration(ms)],
    ['Started', new Date(span.started_at).toLocaleString()],
    ['Ended', span.ended_at ? new Date(span.ended_at).toLocaleString() : '—'],
  ];
  if (span.parallel_group) {
    baseRows.push(['Parallel group', <code className="break-all">{span.parallel_group}</code>]);
  }
  if (span.kind.kind === 'llm_call') {
    baseRows.push(
      ['Model', span.kind.begin.model_id],
      ['Provider', span.kind.begin.provider],
      ['Provider config', <code className="break-all">{span.kind.begin.provider_config_hash}</code>],
    );
    if (span.kind.begin.temperature !== null && span.kind.begin.temperature !== undefined) {
      baseRows.push(['Temperature', span.kind.begin.temperature.toString()]);
    }
    if (span.kind.result) {
      baseRows.push(
        ['Input tokens', (span.kind.result.input_tokens ?? 0).toLocaleString()],
        ['Output tokens', (span.kind.result.output_tokens ?? 0).toLocaleString()],
      );
      const cached = span.kind.result.cached_input_tokens ?? 0;
      const cacheCreate = span.kind.result.cache_creation_input_tokens ?? 0;
      if (cached > 0 || cacheCreate > 0) {
        baseRows.push(['Cache reads', cached.toLocaleString()]);
        if (cacheCreate > 0) {
          baseRows.push(['Cache writes', cacheCreate.toLocaleString()]);
        }
      }
    }
  } else if (span.kind.kind === 'tool_call') {
    baseRows.push(
      ['Tool name', span.kind.begin.tool_name],
      ['Tool artifact', <code className="break-all">{span.kind.begin.tool_artifact_hash}</code>],
    );
    if (span.kind.begin.triggered_by) {
      baseRows.push([
        'Triggered by',
        <code className="break-all">
          {span.kind.begin.triggered_by.llm_span_id} ({span.kind.begin.triggered_by.tool_use_id})
        </code>,
      ]);
    }
  } else if (span.kind.kind === 'subagent_stub') {
    baseRows.push(['Child session', <code className="break-all">{span.kind.child_session_id}</code>]);
  }
  return (
    <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-3 font-mono text-[0.85rem]">
      {baseRows.map(([k, v], i) => (
        <div key={`${k}-${i}`} className="contents">
          <dt className="font-bold text-ink-soft">{k}</dt>
          <dd className="break-all">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

function EventsTab({ events }: { events: SpanEvent[] }) {
  if (events.length === 0) {
    return <div className="text-ink-soft text-[0.85rem]">No events.</div>;
  }
  return (
    <div className="space-y-3">
      {events.map((e) => (
        <div
          key={`${e.span_id}-${e.seq}`}
          className="border-2 border-black rounded-md p-3 bg-canvas"
        >
          <div className="flex items-center justify-between mb-2">
            <span className="font-bold uppercase tracking-wider text-[0.75rem]">
              {e.kind.kind === 'sanitize_hit' ? 'sanitize hit' : 'approval'}
            </span>
            <span className="text-ink-soft font-mono text-[0.75rem]">{formatTime(e.at)}</span>
          </div>
          {e.kind.kind === 'sanitize_hit' ? (
            <div className="space-y-2">
              <div className="text-[0.85rem] font-mono">
                {e.kind.hits_count} {e.kind.hits_count === 1 ? 'hit' : 'hits'}
              </div>
              <div className="flex flex-wrap gap-1">
                {e.kind.kinds.map((k, i) => (
                  <SanitizeChip key={i} kind={k} />
                ))}
              </div>
              {e.kind.placeholder_ids.length > 0 && (
                <details>
                  <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
                    Placeholder IDs ({e.kind.placeholder_ids.length})
                  </summary>
                  <ul className="mt-1 font-mono text-[0.75rem] text-ink-soft space-y-0.5">
                    {e.kind.placeholder_ids.map((p, i) => (
                      <li key={i} className="break-all">
                        {p}
                      </li>
                    ))}
                  </ul>
                </details>
              )}
            </div>
          ) : (
            <div className="space-y-1 font-mono text-[0.85rem]">
              <div>
                Decision:{' '}
                <span
                  className={`font-bold uppercase ${
                    e.kind.decision === 'approve'
                      ? 'text-ok'
                      : e.kind.decision === 'approve_always'
                        ? 'text-ok'
                        : 'text-err'
                  }`}
                >
                  {e.kind.decision.replace('_', ' ')}
                </span>
              </div>
              <div className="text-ink-soft break-all">
                {e.kind.resource.kind}:{' '}
                {e.kind.resource.kind === 'read_file' || e.kind.resource.kind === 'write_file'
                  ? e.kind.resource.path
                  : e.kind.resource.kind === 'http'
                    ? e.kind.resource.host
                    : e.kind.resource.command}
              </div>
            </div>
          )}
        </div>
      ))}
    </div>
  );
}

function SpanDetailPanel({
  span,
  tab,
  onTabChange,
  onJumpToLlm,
  onDrillIn,
}: {
  span: Span;
  tab: 'io' | 'meta' | 'events';
  onTabChange: (t: 'io' | 'meta' | 'events') => void;
  onJumpToLlm: (id: string) => void;
  onDrillIn: (id: string) => void;
}) {
  const visual = SPAN_VISUALS[span.kind.kind];
  const Icon = visual.icon;
  const events = span.events ?? [];
  const eventCount = events.length;
  const ms = durationMs(span);

  return (
    <div className="w-[480px] shrink-0 border-l-[3px] border-black bg-white flex flex-col z-20 shadow-[-4px_0_0_0_rgba(0,0,0,0.1)]">
      <div className="flex flex-col border-b-[3px] border-black bg-canvas">
        <div className="flex items-center p-4 pb-2">
          <div className="flex items-center gap-3 min-w-0">
            <div
              className={`w-10 h-10 rounded-full border-2 border-black flex items-center justify-center shrink-0 ${visual.bg}`}
            >
              <Icon className={`${visual.accent} text-xl`} />
            </div>
            <div className="min-w-0">
              <h3 className="font-bold uppercase tracking-wider leading-tight text-[1rem] truncate">
                {visual.label}
              </h3>
              <div className="text-ink-soft text-[0.8rem] font-mono">
                {span.kind.kind} • {formatDuration(ms)}
              </div>
            </div>
          </div>
        </div>
        <div className="flex px-4 gap-6 relative top-[3px]">
          {(['io', 'meta', 'events'] as const).map((t) => {
            // Hide events tab when there are zero events.
            if (t === 'events' && eventCount === 0) return null;
            const active = t === tab;
            return (
              <button
                type="button"
                key={t}
                onClick={() => onTabChange(t)}
                className={`pb-2 font-bold uppercase tracking-wider text-[0.8rem] border-b-[3px] transition-colors cursor-pointer ${
                  active ? 'border-brand text-ink' : 'border-transparent text-ink-soft hover:text-ink'
                }`}
              >
                {t === 'io' ? 'I/O Data' : t === 'meta' ? 'Metadata' : `Events (${eventCount})`}
              </button>
            );
          })}
        </div>
      </div>

      <div className="flex-1 overflow-y-scroll p-5">
        {tab === 'io' &&
          (span.kind.kind === 'llm_call' ? (
            <LlmCallDetail span={span} />
          ) : span.kind.kind === 'tool_call' ? (
            <ToolCallDetail span={span} onJumpToLlm={onJumpToLlm} />
          ) : (
            <SubagentStubDetail span={span} onDrillIn={onDrillIn} />
          ))}
        {tab === 'meta' && <MetaTab span={span} />}
        {tab === 'events' && <EventsTab events={events} />}
      </div>
    </div>
  );
}

// ── Job tokens helper ───────────────────────────────────────────────

function jobTokens(job: ReplayJob): {
  input: number;
  output: number;
  cached: number;
  cacheCreate: number;
} {
  let input = 0;
  let output = 0;
  let cached = 0;
  let cacheCreate = 0;
  for (const rs of job.steps) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call' && span.kind.result) {
        input += span.kind.result.input_tokens ?? 0;
        output += span.kind.result.output_tokens ?? 0;
        cached += span.kind.result.cached_input_tokens ?? 0;
        cacheCreate += span.kind.result.cache_creation_input_tokens ?? 0;
      }
    }
  }
  return { input, output, cached, cacheCreate };
}

// Derive the user-facing input that kicked off the job: the last user
// message in the *first* LLM call's input_messages (the chat-history
// passed in already contains all prior turns; the most recent user
// message is the prompt for this iteration).
function jobInputText(job: ReplayJob): string | null {
  for (const rs of job.steps) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call') {
        const messages = span.kind.begin.input_messages;
        for (let i = messages.length - 1; i >= 0; i--) {
          if (messages[i].role === 'user') {
            const parts: string[] = [];
            for (const block of messages[i].content) {
              if ('Text' in block) parts.push(block.Text);
              else if ('ToolResult' in block)
                parts.push(`[tool_result ${block.ToolResult.tool_use_id}]`);
              else if ('Image' in block) parts.push(`[image ${block.Image.mime_type}]`);
              else if ('Audio' in block) parts.push(`[audio ${block.Audio.mime_type}]`);
              else if ('File' in block) parts.push(`[file ${block.File.filename}]`);
            }
            return parts.length > 0 ? parts.join('\n') : null;
          }
        }
        return null;
      }
    }
  }
  return null;
}

// Derive the final output text of the job: the most recent LLM call's
// output_content (walking jobs back-to-front).
function jobOutputText(job: ReplayJob): string | null {
  for (let i = job.steps.length - 1; i >= 0; i--) {
    const rs = job.steps[i];
    for (let j = rs.spans.length - 1; j >= 0; j--) {
      const span = rs.spans[j];
      if (span.kind.kind === 'llm_call' && span.kind.result?.output_content) {
        return span.kind.result.output_content;
      }
    }
  }
  return null;
}

function formatTok(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

// ── Job sidebar (multi-job picker) ─────────────────────────────────

function JobSidebar({
  jobs,
  active,
  onChange,
}: {
  jobs: ReplayJob[];
  active: string;
  onChange: (id: string) => void;
}) {
  if (jobs.length <= 1) return null;
  return (
    <div className="w-[120px] shrink-0 border-r-[3px] border-black bg-white flex flex-col z-10 overflow-y-auto">
      <div className="px-2 py-1.5 border-b-2 border-black font-bold uppercase tracking-wider text-[0.65rem] text-ink-soft bg-canvas">
        Jobs · {jobs.length}
      </div>
      <div className="flex flex-col">
        {jobs.map((j, i) => {
          const isActive = j.job_id === active;
          const { input, output } = jobTokens(j);
          return (
            <button
              type="button"
              key={j.job_id}
              onClick={() => onChange(j.job_id)}
              className={`px-2 py-1.5 text-left border-b border-black/20 cursor-pointer transition-colors border-l-[3px] ${
                isActive
                  ? 'bg-brand/10 border-l-brand'
                  : 'bg-white hover:bg-gray-50 border-l-transparent'
              }`}
            >
              <div className="font-bold text-[0.85rem] leading-tight">#{i + 1}</div>
              <div className="font-mono text-[0.65rem] text-ink-soft mt-0.5 truncate">
                {j.job_status_kind}
              </div>
              <div className="font-mono text-[0.65rem] text-ink-soft mt-0.5">
                ↓{formatTok(input)} ↑{formatTok(output)}
              </div>
            </button>
          );
        })}
      </div>
    </div>
  );
}

// ── SelfImprovement cross-link ────────────────────────────────────────
//
// Renders a small badge in the JobDetailsPanel header when the
// displayed job either spawned a self_improvement child or *is* itself a
// self_improvement system job. Implements the `↘ SelfImprovement` /
// `↖ Triggered by user-chat job` cross-link from
// `docs/modules/self-improvement.md` Q12. Lazy-fetches via the new
// `/v1/jobs/{id}/children` and `/v1/jobs/{id}` admin endpoints.

interface SelfImprovementLinkInfo {
  /// SelfImprovement child of this user-chat job, if any.
  childSelfImprovementJobId?: string;
  /// Parent (originating) job, if this *is* a self_improvement job.
  parentJobIdIfSelfImprovement?: string;
  parentSessionId?: string;
}

function SelfImprovementCrossLink({ jobId }: { jobId: string }) {
  const isMock = useMockMode();
  const client = useAdminClient();
  const navigate = useNavigate();
  const [info, setInfo] = useState<SelfImprovementLinkInfo | null>(null);

  useEffect(() => {
    if (isMock || !jobId) {
      setInfo(null);
      return;
    }
    let cancelled = false;
    async function load() {
      try {
        const [{ data: jobResp }, { data: childResp }] = await Promise.all([
          client.GET('/v1/jobs/{id}', { params: { path: { id: jobId } } }),
          client.GET('/v1/jobs/{id}/children', { params: { path: { id: jobId } } }),
        ]);
        if (cancelled) return;
        const result: SelfImprovementLinkInfo = {};
        // Find a self_improvement child: kind=system + JobInput.reason=self_improvement.
        const items =
          (childResp as { items?: Array<{ job_id?: string; kind?: string; input?: { kind?: string; reason?: string } }> })?.items ?? [];
        const distChild = items.find(
          (j) => j.kind === 'system' && j.input?.reason === 'self_improvement',
        );
        if (distChild?.job_id) result.childSelfImprovementJobId = distChild.job_id;
        // If the displayed job *is* itself a self_improvement job, link back.
        const job = jobResp as
          | { kind?: string; input?: { kind?: string; reason?: string }; parent_job_id?: string | null }
          | undefined;
        if (job?.kind === 'system' && job.input?.reason === 'self_improvement' && job.parent_job_id) {
          result.parentJobIdIfSelfImprovement = job.parent_job_id;
          // We don't have parent's session id; the trace endpoint is
          // session-scoped, so the link below uses /traces/<session_id>?job=<jid>
          // — but we'd need to fetch the parent job first to learn its
          // session. Skip the parentSessionId lookup for v1; render a
          // disabled-looking pill showing the parent job id only.
        }
        setInfo(result);
      } catch {
        setInfo(null);
      }
    }
    void load();
    return () => {
      cancelled = true;
    };
  }, [client, isMock, jobId]);

  if (!info || (!info.childSelfImprovementJobId && !info.parentJobIdIfSelfImprovement)) {
    return null;
  }

  return (
    <div className="mt-3 flex flex-wrap gap-2 text-[0.75rem] font-mono">
      {info.childSelfImprovementJobId && (
        <button
          type="button"
          onClick={() =>
            navigate(`/traces?job=${encodeURIComponent(info.childSelfImprovementJobId!)}`)
          }
          className="px-2 py-1 border-2 border-black bg-white hover:bg-canvas uppercase tracking-wider text-[0.7rem]"
          title={`self_improvement child job ${info.childSelfImprovementJobId}`}
        >
          ↘ self_improvement
        </button>
      )}
      {info.parentJobIdIfSelfImprovement && (
        <span
          className="px-2 py-1 border-2 border-black bg-canvas uppercase tracking-wider text-[0.7rem]"
          title={`triggered by user-chat job ${info.parentJobIdIfSelfImprovement}`}
        >
          ↖ triggered by {info.parentJobIdIfSelfImprovement.slice(0, 8)}…
        </span>
      )}
    </div>
  );
}

// ── Right panel default: job-level summary ─────────────────────────

function JobSummaryPanel({
  job,
  jobIndex,
  totalJobs,
}: {
  job: ReplayJob | undefined;
  jobIndex: number;
  totalJobs: number;
}) {
  if (!job) {
    return (
      <div className="w-[480px] shrink-0 border-l-[3px] border-black bg-white flex flex-col z-20 shadow-[-4px_0_0_0_rgba(0,0,0,0.1)]">
        <div className="p-5 text-ink-soft italic text-[0.85rem]">No job available.</div>
      </div>
    );
  }
  const { input, output, cached, cacheCreate } = jobTokens(job);
  const total = input + output;
  const inputText = jobInputText(job);
  const outputText = jobOutputText(job);
  let llmCount = 0;
  let toolCount = 0;
  for (const rs of job.steps) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call') llmCount += 1;
      else if (span.kind.kind === 'tool_call') toolCount += 1;
    }
  }
  const stepCount = job.steps.length;

  return (
    <div className="w-[480px] shrink-0 border-l-[3px] border-black bg-white flex flex-col z-20 shadow-[-4px_0_0_0_rgba(0,0,0,0.1)]">
      <div className="border-b-[3px] border-black bg-canvas p-4">
        <div className="flex items-center gap-3 min-w-0">
          <div className="w-10 h-10 rounded-full border-2 border-black flex items-center justify-center shrink-0 bg-brand/10">
            <RiCpuLine className="text-brand text-xl" />
          </div>
          <div className="min-w-0">
            <h3 className="font-bold uppercase tracking-wider leading-tight text-[1rem] truncate">
              {totalJobs > 1 ? `Job #${jobIndex + 1}` : 'Job Overview'}
            </h3>
            <div className="text-ink-soft text-[0.8rem] font-mono truncate">
              {job.job_status_kind} • {stepCount} {stepCount === 1 ? 'step' : 'steps'}
            </div>
          </div>
        </div>
        <SelfImprovementCrossLink jobId={job.job_id} />
      </div>

      <div className="flex-1 overflow-y-scroll p-5 space-y-6">
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Input
          </h4>
          {inputText ? (
            <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3 max-h-72 overflow-y-auto">
              {inputText}
            </pre>
          ) : (
            <div className="text-ink-soft text-[0.8rem] italic">No user input recorded.</div>
          )}
        </section>

        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Output
          </h4>
          {outputText ? (
            <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3 max-h-72 overflow-y-auto">
              {outputText}
            </pre>
          ) : (
            <div className="text-ink-soft text-[0.8rem] italic">
              {job.job_status_kind === 'completed' ? 'No output text.' : 'Awaiting final output…'}
            </div>
          )}
        </section>

        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Activity
          </h4>
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 font-mono text-[0.85rem]">
            <dt className="font-bold text-ink-soft">Job ID</dt>
            <dd className="break-all">
              <code>{job.job_id}</code>
            </dd>
            <dt className="font-bold text-ink-soft">Status</dt>
            <dd>{job.job_status_kind}</dd>
            <dt className="font-bold text-ink-soft">Steps</dt>
            <dd>{stepCount}</dd>
            <dt className="font-bold text-ink-soft">LLM calls</dt>
            <dd>{llmCount}</dd>
            <dt className="font-bold text-ink-soft">Tool calls</dt>
            <dd>{toolCount}</dd>
            <dt className="font-bold text-ink-soft">Input tokens</dt>
            <dd>{input.toLocaleString()}</dd>
            <dt className="font-bold text-ink-soft">Output tokens</dt>
            <dd>{output.toLocaleString()}</dd>
            {(cached > 0 || cacheCreate > 0) && (
              <>
                <dt className="font-bold text-ink-soft">Cache reads</dt>
                <dd>{cached.toLocaleString()}</dd>
                {cacheCreate > 0 && (
                  <>
                    <dt className="font-bold text-ink-soft">Cache writes</dt>
                    <dd>{cacheCreate.toLocaleString()}</dd>
                  </>
                )}
              </>
            )}
            <dt className="font-bold text-ink-soft">Total tokens</dt>
            <dd className="text-brand font-bold">{total.toLocaleString()}</dd>
          </dl>
        </section>

        <section className="text-[0.8rem] text-ink-soft italic">
          Select a span on the left to inspect its inputs, outputs, and metadata.
        </section>
      </div>
    </div>
  );
}

// ── Page ─────────────────────────────────────────────────────────────

function deriveSpanIndex(replay: SessionReplay | null): Map<string, Span> {
  const m = new Map<string, Span>();
  if (!replay) return m;
  for (const job of replay.jobs) {
    for (const rs of job.steps) {
      for (const span of rs.spans) {
        m.set(span.id, span);
      }
    }
  }
  return m;
}

function findStepIdForSpan(replay: SessionReplay | null, spanId: string): string | null {
  if (!replay) return null;
  for (const job of replay.jobs) {
    for (const rs of job.steps) {
      if (rs.spans.some((s) => s.id === spanId)) return rs.step.id;
    }
  }
  return null;
}

function findJobIdForSpan(replay: SessionReplay | null, spanId: string): string | null {
  if (!replay) return null;
  for (const job of replay.jobs) {
    for (const rs of job.steps) {
      if (rs.spans.some((s) => s.id === spanId)) return job.job_id;
    }
  }
  return null;
}

function anyJobNonTerminal(replay: SessionReplay | null): boolean {
  if (!replay) return false;
  return replay.jobs.some(
    (j) => j.job_status_kind === 'pending' || j.job_status_kind === 'in_progress' || j.job_status_kind === 'stuck',
  );
}

function anySpanPending(replay: SessionReplay | null): boolean {
  if (!replay) return false;
  for (const job of replay.jobs) {
    for (const rs of job.steps) {
      if (!isTerminal(rs.step.outcome)) return true;
      for (const s of rs.spans) {
        if (!isTerminal(s.outcome)) return true;
      }
    }
  }
  return false;
}

export function TraceSessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const isMock = useMockMode();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [replay, setReplay] = useState<SessionReplay | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  const sessionId = id ?? '';
  const jobIdParam = searchParams.get('job');
  const spanIdParam = searchParams.get('span');
  const tabParam = (searchParams.get('tab') as 'io' | 'meta' | 'events' | null) ?? 'io';

  // Fetch / refetch.
  useEffect(() => {
    let cancelled = false;
    async function fetchReplay() {
      if (!sessionId) return;
      if (isMock) {
        setReplay(getMockSessionReplay(sessionId));
        setError(null);
        setLoading(false);
        return;
      }
      setLoading(true);
      setError(null);
      try {
        const { data, error: apiError, response } = await client.GET('/v1/traces/{session_id}', {
          params: { path: { session_id: sessionId } },
        });
        if (cancelled) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          setError((apiError as { error?: string })?.error || `HTTP Error ${response.status}`);
          return;
        }
        // Endpoint is untyped JSON server-side; cast to mirrored shape.
        setReplay(data as unknown as SessionReplay);
      } catch (e) {
        if (cancelled) return;
        setError(
          e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway',
        );
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    void fetchReplay();
    return () => {
      cancelled = true;
    };
  }, [client, isMock, logout, sessionId, refreshKey]);

  // Smart polling — visibility-aware. 2s while non-terminal, 10s once
  // terminal. Stops entirely if every span is terminal AND every job is
  // terminal.
  const polling = useMemo(() => anyJobNonTerminal(replay) || anySpanPending(replay), [replay]);
  useEffect(() => {
    if (isMock) return;
    const cadence = polling ? POLL_ACTIVE_MS : POLL_TERMINAL_MS;
    if (!polling && replay !== null) return; // fully terminal, stop
    const tick = () => {
      if (document.visibilityState === 'visible') {
        setRefreshKey((k) => k + 1);
      }
    };
    const t = window.setInterval(tick, cadence);
    return () => window.clearInterval(t);
  }, [isMock, polling, replay]);

  const spanIndex = useMemo(() => deriveSpanIndex(replay), [replay]);

  const updateUrl = useCallback(
    (next: Partial<{ job: string | null; span: string | null; tab: string | null }>) => {
      const sp = new URLSearchParams(searchParams);
      for (const [k, v] of Object.entries(next)) {
        if (v == null || v === '') sp.delete(k);
        else sp.set(k, v);
      }
      setSearchParams(sp, { replace: true });
    },
    [searchParams, setSearchParams],
  );

  const handleSelectSpan = useCallback(
    (spanId: string) => {
      updateUrl({ span: spanId, tab: tabParam });
    },
    [tabParam, updateUrl],
  );

  const handleJumpToLlm = useCallback(
    (llmSpanId: string) => {
      updateUrl({ span: llmSpanId });
      // Scroll the step into view.
      const stepId = findStepIdForSpan(replay, llmSpanId);
      const jobId = findJobIdForSpan(replay, llmSpanId);
      if (jobId) updateUrl({ job: jobId, span: llmSpanId });
      if (stepId) {
        // setTimeout so the panel re-renders with the new selection first.
        setTimeout(() => {
          document
            .querySelector(`[data-step-id="${stepId}"]`)
            ?.scrollIntoView({ behavior: 'smooth', block: 'center' });
        }, 16);
      }
    },
    [replay, updateUrl],
  );

  const handleDrillIntoChild = useCallback(
    (childSessionId: string) => {
      navigate(`/traces/${encodeURIComponent(childSessionId)}`);
    },
    [navigate],
  );

  if (loading && !replay) {
    return (
      <div className="p-5 text-ink-soft text-[0.95rem] flex items-center gap-2">
        <RiLoader4Line className="animate-spin" /> Loading trace…
      </div>
    );
  }
  if (error && !replay) {
    return (
      <div className="p-5">
        <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      </div>
    );
  }
  if (!replay) {
    return <div className="p-5 text-ink-soft">No trace.</div>;
  }

  const activeJobId =
    jobIdParam && replay.jobs.some((j) => j.job_id === jobIdParam)
      ? jobIdParam
      : (replay.jobs[0]?.job_id ?? '');
  const activeJob = replay.jobs.find((j) => j.job_id === activeJobId) ?? replay.jobs[0];
  const selectedSpan = spanIdParam ? (spanIndex.get(spanIdParam) ?? null) : null;

  return (
    <div className="flex flex-col h-full overflow-hidden bg-canvas">
      <div className="p-5 shrink-0 flex items-center gap-4 bg-white border-b-[3px] border-black z-10">
        <IconButton onClick={() => navigate(-1)} aria-label="Go back">
          <RiArrowLeftLine />
        </IconButton>
        <div className="flex-1 min-w-0">
          <h2 className="text-[1.4rem] font-bold uppercase -tracking-[0.05em] leading-tight">
            TRACE DETAILS
          </h2>
          <div className="text-ink-soft text-[0.85rem] font-mono truncate flex items-center gap-3 flex-wrap">
            <span className="inline-flex items-center gap-1">
              <RiCpuLine /> Session: {replay.session_id}
            </span>
            <span>
              {replay.jobs.length} {replay.jobs.length === 1 ? 'job' : 'jobs'}
            </span>
            {activeJob && (
              <span className="inline-flex items-center gap-2">
                <span className="text-ink-soft uppercase text-[0.7rem] font-bold tracking-wider">
                  {replay.jobs.length === 1 ? 'Tokens' : `Job #${replay.jobs.indexOf(activeJob) + 1}`}
                </span>
                {(() => {
                  const t = jobTokens(activeJob);
                  return (
                    <>
                      <span className="text-ink">↓ {t.input.toLocaleString()}</span>
                      <span className="text-ink">↑ {t.output.toLocaleString()}</span>
                    </>
                  );
                })()}
              </span>
            )}
            {polling && !isMock && (
              <span className="text-info inline-flex items-center gap-1 font-bold">
                <RiLoader4Line className="animate-spin" /> live
              </span>
            )}
          </div>
        </div>
        <Button
          onClick={() => setRefreshKey((k) => k + 1)}
          disabled={loading || isMock}
          className="!py-2 !px-3 !text-[0.85rem] h-9 gap-1.5"
        >
          <RiRefreshLine className="text-lg shrink-0" /> Refresh
        </Button>
      </div>

      <div className="flex-1 flex overflow-hidden min-h-0">
        <JobSidebar
          jobs={replay.jobs}
          active={activeJobId}
          onChange={(j) => updateUrl({ job: j, span: null })}
        />

        <div className="flex-1 overflow-y-scroll p-5">
          <div className="max-w-4xl mx-auto space-y-4 pb-10">
            {activeJob?.steps.map((rs) => (
              <div key={rs.step.id} data-step-id={rs.step.id}>
                <StepBlock rs={rs} selectedSpanId={spanIdParam} onSelect={handleSelectSpan} />
              </div>
            ))}
            {(!activeJob || activeJob.steps.length === 0) && (
              <div className="text-ink-soft text-[0.95rem] italic">No steps yet for this job.</div>
            )}
          </div>
        </div>

        {selectedSpan ? (
          <SpanDetailPanel
            span={selectedSpan}
            tab={tabParam}
            onTabChange={(t) => updateUrl({ tab: t })}
            onJumpToLlm={handleJumpToLlm}
            onDrillIn={handleDrillIntoChild}
          />
        ) : (
          <JobSummaryPanel
            job={activeJob}
            jobIndex={activeJob ? replay.jobs.indexOf(activeJob) : 0}
            totalJobs={replay.jobs.length}
          />
        )}
      </div>
    </div>
  );
}
