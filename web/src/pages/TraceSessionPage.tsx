import { useCallback, useEffect, useMemo, useState } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  RiArrowLeftLine,
  RiArchiveLine,
  RiBookmark3Line,
  RiBroadcastLine,
  RiBrainLine,
  RiCornerDownLeftLine,
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
import { getMockJobTrace, getMockTraceOverview } from '../api/mock';
import { useMockMode } from '../api/mock';
import type {
  ChatMessage,
  JobTrace,
  LifecycleState,
  ReplayStep,
  SecretKind,
  SessionMessageRow,
  Span,
  SpanEvent,
  SpanKindTag,
  Step,
  StepKindTag,
  ToolEventPayload,
  TraceJobSummary,
  TraceOverview,
} from '../types/trace';
import { isTerminal, resolveInputMessages } from '../types/trace';
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
  progress_observer: {
    icon: RiBroadcastLine,
    accent: 'text-info',
    bg: 'bg-info/10',
    label: 'Progress observer',
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

// A kind the frontend doesn't know yet (wire drift, or a new Rust variant
// shipped ahead of this map) must degrade to a generic row — never let a
// `STEP_VISUALS[kind].icon` on `undefined` throw and white-screen the whole
// trace view. The raw tag becomes the label so it's still legible.
const FALLBACK_VISUAL: KindVisual = {
  icon: RiCpuLine,
  accent: 'text-ink-soft',
  bg: 'bg-gray-100',
  label: 'step',
};

function stepVisual(kind: StepKindTag): KindVisual {
  return STEP_VISUALS[kind] ?? { ...FALLBACK_VISUAL, label: kind };
}

function spanVisual(kind: SpanKindTag): KindVisual {
  return SPAN_VISUALS[kind] ?? { ...FALLBACK_VISUAL, label: kind };
}

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
    case 'progress_observer': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      if (llm && llm.kind.kind === 'llm_call' && llm.kind.result?.output_content) {
        return llm.kind.result.output_content.slice(0, 80);
      }
      return 'progress update';
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
  const visual = spanVisual(span.kind.kind);
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
        <div className="flex items-center gap-3 text-[0.75rem] text-ink-soft font-mono mt-0.5 min-w-0">
          <span className="shrink-0">{visual.label}</span>
          <span className="shrink-0">•</span>
          <span className="truncate">{subtitle}</span>
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
  const visual = stepVisual(step.kind.kind);
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

function LlmCallDetail({
  span,
  messageLog,
}: {
  span: Span;
  messageLog: SessionMessageRow[];
}) {
  if (span.kind.kind !== 'llm_call') return null;
  const { begin, result } = span.kind;
  const hint = SanitizeKindHint(span.events);
  const failureReason =
    span.outcome.outcome === 'failed' ? span.outcome.reason : null;
  const inputMessages = resolveInputMessages(
    begin.input_messages,
    messageLog,
    span.started_at,
  );

  return (
    <div className="space-y-6">
      {failureReason && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-err pb-1 text-err">
            Failure reason
          </h4>
          <div className="font-mono text-[0.85rem] bg-err/5 border-2 border-err rounded-md p-3 whitespace-pre-wrap break-all text-err">
            {failureReason}
          </div>
        </section>
      )}

      <section>
        <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
          Input messages
        </h4>
        <MessageList messages={inputMessages} kindHint={hint} />
      </section>

      {result && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Output
          </h4>
          {result.output_content && (
            <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
              {renderWithSanitizeChips(result.output_content, hint)}
            </pre>
          )}
          {result.thinking && (
            <details className="mt-2">
              <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
                Thinking ({result.thinking.length.toLocaleString()} chars)
              </summary>
              <pre className="mt-1 whitespace-pre-wrap break-all font-mono text-[0.8rem] italic bg-gray-50 border-2 border-black rounded-md p-3">
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
          <div className="font-mono text-[0.85rem] bg-err/5 border-2 border-err rounded-md p-3 whitespace-pre-wrap break-all text-err">
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
          <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
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
  const visual = spanVisual(span.kind.kind);
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

function ToolEventRow({
  action,
  payload,
}: {
  action: string;
  payload: ToolEventPayload;
}) {
  if (payload.type === 'phase') {
    return (
      <div className="flex items-baseline justify-between font-mono text-[0.85rem] gap-3">
        <span className="break-all">{action}</span>
        <span className="font-bold tabular-nums shrink-0">{payload.duration_ms} ms</span>
      </div>
    );
  }
  if (payload.type === 'http_fetch') {
    return (
      <div className="space-y-1 font-mono text-[0.85rem]">
        <div className="flex items-baseline justify-between gap-3">
          <span className="break-all">{action}</span>
          <span className="font-bold tabular-nums shrink-0">
            {payload.status} · {payload.bytes} B
          </span>
        </div>
        {payload.content_type && (
          <div className="text-ink-soft break-all">{payload.content_type}</div>
        )}
        {payload.body_preview && (
          <details>
            <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
              Body preview ({payload.body_preview.length} chars)
            </summary>
            <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">
              {payload.body_preview}
            </pre>
          </details>
        )}
      </div>
    );
  }
  return (
    <div className="space-y-1 font-mono text-[0.85rem]">
      <div className="flex items-baseline justify-between gap-3">
        <span className="break-all">{action}</span>
        <span className="font-bold shrink-0">{payload.model}</span>
      </div>
      <details>
        <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
          Input ({payload.input.length} chars)
        </summary>
        <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">
          {payload.input}
        </pre>
      </details>
      <details>
        <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
          Output ({payload.output.length} chars)
        </summary>
        <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">
          {payload.output}
        </pre>
      </details>
    </div>
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
              {e.kind.kind === 'sanitize_hit'
                ? 'sanitize hit'
                : e.kind.kind === 'approval'
                  ? 'approval'
                  : e.kind.payload.type === 'phase'
                    ? 'timer'
                    : e.kind.payload.type === 'http_fetch'
                      ? 'http fetch'
                      : 'llm call'}
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
          ) : e.kind.kind === 'approval' ? (
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
          ) : (
            <ToolEventRow action={e.kind.action} payload={e.kind.payload} />
          )}
        </div>
      ))}
    </div>
  );
}

function SpanDetailPanel({
  span,
  messageLog,
  tab,
  onTabChange,
  onJumpToLlm,
  onDrillIn,
}: {
  span: Span;
  messageLog: SessionMessageRow[];
  tab: 'io' | 'meta' | 'events';
  onTabChange: (t: 'io' | 'meta' | 'events') => void;
  onJumpToLlm: (id: string) => void;
  onDrillIn: (id: string) => void;
}) {
  const visual = spanVisual(span.kind.kind);
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
            <LlmCallDetail span={span} messageLog={messageLog} />
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

// ── Job helpers (summary-level vs trace-level) ──────────────────────

// Total job execution time. Prefers the backend's `started_at`/`ended_at`
// (covers setup, teardown, and gaps that fall outside any step) and
// falls back to deriving from step timestamps for older traces where
// the wire didn't carry job-level times. For in-flight jobs we use
// `now` as the upper bound so the counter keeps ticking; callers
// re-render on each poll. Accepts an optional loaded trace so we can
// fall back when the summary is missing `started_at`.
function jobDurationMs(
  job: { started_at?: string | null; ended_at?: string | null },
  trace: JobTrace | undefined,
): number | null {
  if (job.started_at) {
    const start = new Date(job.started_at).getTime();
    const end = job.ended_at ? new Date(job.ended_at).getTime() : Date.now();
    return Math.max(0, end - start);
  }
  if (!trace) return null;
  let minStart = Infinity;
  let maxEnd = -Infinity;
  let inFlight = false;
  for (const rs of trace.steps) {
    const start = new Date(rs.step.started_at).getTime();
    if (start < minStart) minStart = start;
    if (rs.step.ended_at) {
      const end = new Date(rs.step.ended_at).getTime();
      if (end > maxEnd) maxEnd = end;
    } else {
      inFlight = true;
    }
  }
  if (minStart === Infinity) return null;
  const end = inFlight ? Date.now() : maxEnd;
  if (end === -Infinity) return null;
  return Math.max(0, end - minStart);
}

// Time the job sat in queue before execution started. Only meaningful
// when both timestamps are present; returns null for legacy traces or
// jobs that never started.
function jobQueuedMs(job: { created_at?: string | null; started_at?: string | null }): number | null {
  if (!job.created_at || !job.started_at) return null;
  return Math.max(
    0,
    new Date(job.started_at).getTime() - new Date(job.created_at).getTime(),
  );
}

interface JobTokenTotals {
  input: number;
  output: number;
  cached: number;
  cacheCreate: number;
  inputTotal: number;
}

function summaryTokens(summary: TraceJobSummary): JobTokenTotals {
  return {
    input: summary.input_tokens,
    output: summary.output_tokens,
    cached: summary.cached_input_tokens,
    cacheCreate: summary.cache_creation_input_tokens,
    inputTotal:
      summary.input_tokens + summary.cached_input_tokens + summary.cache_creation_input_tokens,
  };
}

// Derive token totals from a loaded JobTrace's spans. Used when we
// have the full step/span tree (active job that finished loading).
// Otherwise prefer `summaryTokens` so the sidebar stays populated for
// jobs the user hasn't drilled into yet.
function traceTokens(trace: JobTrace): JobTokenTotals {
  let input = 0;
  let output = 0;
  let cached = 0;
  let cacheCreate = 0;
  for (const rs of trace.steps) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call' && span.kind.result) {
        input += span.kind.result.input_tokens ?? 0;
        output += span.kind.result.output_tokens ?? 0;
        cached += span.kind.result.cached_input_tokens ?? 0;
        cacheCreate += span.kind.result.cache_creation_input_tokens ?? 0;
      }
    }
  }
  return { input, output, cached, cacheCreate, inputTotal: input + cached + cacheCreate };
}

// Derive the user-facing input that kicked off the job: the last
// message in the *first* LLM call's input_messages whose `source` is
// 'user'. The agent injects several `Role::User` messages of its own
// (skill reminders, system-reminders) so role alone isn't enough to
// identify the genuine prompt. Requires the message log because the
// LLM call may reference it via `LlmCallInputs::Persisted`.
function jobInputText(trace: JobTrace, messageLog: SessionMessageRow[]): string | null {
  for (const rs of trace.steps) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call') {
        const messages: ChatMessage[] = resolveInputMessages(
          span.kind.begin.input_messages,
          messageLog,
          span.started_at,
        );
        for (let i = messages.length - 1; i >= 0; i--) {
          if (messages[i].source === 'user') {
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
function jobOutputText(trace: JobTrace): string | null {
  for (let i = trace.steps.length - 1; i >= 0; i--) {
    const rs = trace.steps[i];
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
  interjectionCounts,
  onChange,
}: {
  jobs: TraceJobSummary[];
  active: string;
  interjectionCounts: Map<string, number>;
  onChange: (id: string) => void;
}) {
  return (
    <div className="w-[120px] shrink-0 border-r-[3px] border-black bg-white flex flex-col z-10 overflow-y-auto">
      <div className="px-2 py-1.5 border-b-2 border-black font-bold uppercase tracking-wider text-[0.65rem] text-ink-soft bg-canvas">
        Jobs · {jobs.length}
      </div>
      <div className="flex flex-col">
        {jobs.map((j, i) => {
          const isActive = j.job_id === active;
          const { inputTotal, output } = summaryTokens(j);
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
                ↑{formatTok(inputTotal)} ↓{formatTok(output)}
              </div>
              {(interjectionCounts.get(j.job_id) ?? 0) > 0 && (
                <div
                  title="This job folded in mid-turn user message(s) (steering)"
                  className="mt-1 inline-flex items-center gap-0.5 border-2 border-black rounded bg-warn/15 px-1 py-px text-[0.6rem] font-bold uppercase tracking-wider text-warn"
                >
                  <RiCornerDownLeftLine className="text-[0.7rem]" />
                  {interjectionCounts.get(j.job_id)}
                </div>
              )}
            </button>
          );
        })}
      </div>
    </div>
  );
}

// ── Right panel default: job-level summary ─────────────────────────

function JobSummaryPanel({
  summary,
  trace,
  traceLoading,
  messageLog,
  jobIndex,
  totalJobs,
  interjectionCount,
}: {
  summary: TraceJobSummary | undefined;
  trace: JobTrace | undefined;
  traceLoading: boolean;
  messageLog: SessionMessageRow[];
  jobIndex: number;
  totalJobs: number;
  interjectionCount: number;
}) {
  if (!summary) {
    return (
      <div className="w-[480px] shrink-0 border-l-[3px] border-black bg-white flex flex-col z-20 shadow-[-4px_0_0_0_rgba(0,0,0,0.1)]">
        <div className="p-5 text-ink-soft italic text-[0.85rem]">No job available.</div>
      </div>
    );
  }
  // Tokens come from the summary (always present); span-derived figures
  // (llm/tool counts, input/output text) need the loaded trace.
  const { input, output, cached, cacheCreate, inputTotal } = trace
    ? traceTokens(trace)
    : summaryTokens(summary);
  const total = inputTotal + output;
  const inputText = trace ? jobInputText(trace, messageLog) : null;
  const outputText = trace ? jobOutputText(trace) : null;
  let llmCount = 0;
  let toolCount = 0;
  if (trace) {
    for (const rs of trace.steps) {
      for (const span of rs.spans) {
        if (span.kind.kind === 'llm_call') llmCount += 1;
        else if (span.kind.kind === 'tool_call') toolCount += 1;
      }
    }
  }
  const stepCount = trace?.steps.length ?? 0;
  const durMs = jobDurationMs(summary, trace);
  const queuedMs = jobQueuedMs(summary);

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
              {summary.job_status_kind}
              {trace
                ? ` • ${stepCount} ${stepCount === 1 ? 'step' : 'steps'} • ${formatDuration(durMs)}`
                : traceLoading
                  ? ' • loading…'
                  : ` • ${formatDuration(durMs)}`}
            </div>
            {interjectionCount > 0 && (
              <div
                title="This job folded in mid-turn user message(s) (steering)"
                className="mt-1 inline-flex items-center gap-1 border-2 border-black rounded bg-warn/15 px-1.5 py-0.5 text-[0.65rem] font-bold uppercase tracking-wider text-warn"
              >
                <RiCornerDownLeftLine className="text-[0.8rem]" />
                {interjectionCount} interjection{interjectionCount === 1 ? '' : 's'}
              </div>
            )}
          </div>
        </div>
      </div>

      <div className="flex-1 overflow-y-scroll p-5 space-y-6">
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Input
          </h4>
          {inputText ? (
            <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3 max-h-72 overflow-y-auto">
              {inputText}
            </pre>
          ) : traceLoading ? (
            <div className="text-ink-soft text-[0.8rem] italic flex items-center gap-2">
              <RiLoader4Line className="animate-spin" /> Loading job…
            </div>
          ) : (
            <div className="text-ink-soft text-[0.8rem] italic">No user input recorded.</div>
          )}
        </section>

        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Output
          </h4>
          {outputText ? (
            <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3 max-h-72 overflow-y-auto">
              {outputText}
            </pre>
          ) : traceLoading ? (
            <div className="text-ink-soft text-[0.8rem] italic flex items-center gap-2">
              <RiLoader4Line className="animate-spin" /> Loading job…
            </div>
          ) : (
            <div className="text-ink-soft text-[0.8rem] italic">
              {summary.job_status_kind === 'completed'
                ? 'No output text.'
                : 'Awaiting final output…'}
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
              <code>{summary.job_id}</code>
            </dd>
            <dt className="font-bold text-ink-soft">Status</dt>
            <dd>{summary.job_status_kind}</dd>
            <dt className="font-bold text-ink-soft">Duration</dt>
            <dd>{formatDuration(durMs)}</dd>
            {queuedMs !== null && queuedMs > 0 && (
              <>
                <dt className="font-bold text-ink-soft">Queued</dt>
                <dd>{formatDuration(queuedMs)}</dd>
              </>
            )}
            <dt className="font-bold text-ink-soft">Steps</dt>
            <dd>{stepCount}</dd>
            <dt className="font-bold text-ink-soft">LLM calls</dt>
            <dd>{llmCount}</dd>
            <dt className="font-bold text-ink-soft">Tool calls</dt>
            <dd>{toolCount}</dd>
            {interjectionCount > 0 && (
              <>
                <dt className="font-bold text-ink-soft">Interjections</dt>
                <dd className="text-warn font-bold">{interjectionCount}</dd>
              </>
            )}
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

function deriveSpanIndex(trace: JobTrace | undefined): Map<string, Span> {
  const m = new Map<string, Span>();
  if (!trace) return m;
  for (const rs of trace.steps) {
    for (const span of rs.spans) {
      m.set(span.id, span);
    }
  }
  return m;
}

function findStepIdForSpan(trace: JobTrace | undefined, spanId: string): string | null {
  if (!trace) return null;
  for (const rs of trace.steps) {
    if (rs.spans.some((s) => s.id === spanId)) return rs.step.id;
  }
  return null;
}

function isJobLive(status: string): boolean {
  return status === 'pending' || status === 'in_progress' || status === 'stuck';
}

function traceHasPendingSpan(trace: JobTrace | undefined): boolean {
  if (!trace) return false;
  for (const rs of trace.steps) {
    if (!isTerminal(rs.step.outcome)) return true;
    for (const s of rs.spans) {
      if (!isTerminal(s.outcome)) return true;
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

  const [overview, setOverview] = useState<TraceOverview | null>(null);
  const [jobTraces, setJobTraces] = useState<Map<string, JobTrace>>(() => new Map());
  const [overviewLoading, setOverviewLoading] = useState(false);
  const [activeJobLoading, setActiveJobLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [overviewRefreshKey, setOverviewRefreshKey] = useState(0);
  const [jobRefreshKey, setJobRefreshKey] = useState(0);

  const sessionId = id ?? '';
  const jobIdParam = searchParams.get('job');
  const spanIdParam = searchParams.get('span');
  const tabParam = (searchParams.get('tab') as 'io' | 'meta' | 'events' | null) ?? 'io';

  // Fetch the cheap overview (session messages + job summaries).
  // Reset per-session caches when the session id changes.
  useEffect(() => {
    let cancelled = false;
    async function fetchOverview() {
      if (!sessionId) return;
      if (isMock) {
        setOverview(getMockTraceOverview(sessionId));
        setError(null);
        setOverviewLoading(false);
        return;
      }
      setOverviewLoading(true);
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
        setOverview(data as unknown as TraceOverview);
      } catch (e) {
        if (cancelled) return;
        setError(
          e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway',
        );
      } finally {
        if (!cancelled) setOverviewLoading(false);
      }
    }
    void fetchOverview();
    return () => {
      cancelled = true;
    };
  }, [client, isMock, logout, sessionId, overviewRefreshKey]);

  // Reset cached per-job traces whenever the session changes so
  // navigating to another session doesn't surface a stale tree.
  useEffect(() => {
    setJobTraces(new Map());
  }, [sessionId]);

  // Derive the active job id from URL ∩ overview. Default to the
  // oldest job (sidebar's `#1`) when no URL hint resolves.
  const activeJobId =
    jobIdParam && overview?.jobs.some((j) => j.job_id === jobIdParam)
      ? jobIdParam
      : (overview?.jobs[0]?.job_id ?? '');
  const activeJobSummary = overview?.jobs.find((j) => j.job_id === activeJobId);
  const activeJobTrace = activeJobId ? jobTraces.get(activeJobId) : undefined;
  const messageLog = overview?.session_messages ?? [];

  // Map each job to the count of mid-turn user interjections folded into
  // it. A `user_interjection` row is only persisted mid-drain, so it
  // belongs to the job whose [started_at, ended_at) window contains its
  // `created_at` — jobs run sequentially, so the assignment is unambiguous.
  const interjectionCountByJob = useMemo(() => {
    const counts = new Map<string, number>();
    const interjections = (overview?.session_messages ?? []).filter(
      (r) => r.message.source === 'user_interjection',
    );
    if (interjections.length === 0) return counts;
    for (const job of overview?.jobs ?? []) {
      const startIso = job.started_at ?? job.created_at;
      if (!startIso) continue;
      const start = new Date(startIso).getTime();
      const end = job.ended_at ? new Date(job.ended_at).getTime() : Number.POSITIVE_INFINITY;
      let n = 0;
      for (const r of interjections) {
        const t = new Date(r.created_at).getTime();
        if (t >= start && t < end) n += 1;
      }
      if (n > 0) counts.set(job.job_id, n);
    }
    return counts;
  }, [overview]);

  // Fetch the active job's step/span tree on demand. Re-fires when the
  // user picks a different sidebar entry or the polling tick bumps
  // `jobRefreshKey`. Caches per job id; bumping the key re-fetches the
  // active job only.
  useEffect(() => {
    if (!activeJobId) return;
    let cancelled = false;
    async function fetchJobTrace() {
      if (isMock) {
        const mock = getMockJobTrace(sessionId, activeJobId);
        if (mock) {
          setJobTraces((prev) => {
            const next = new Map(prev);
            next.set(activeJobId, mock);
            return next;
          });
        }
        setActiveJobLoading(false);
        return;
      }
      setActiveJobLoading(true);
      try {
        const { data, error: apiError, response } = await client.GET(
          '/v1/traces/{session_id}/jobs/{job_id}',
          { params: { path: { session_id: sessionId, job_id: activeJobId } } },
        );
        if (cancelled) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          // Don't blow away the overview-level page; show inline panel
          // hints instead. A 404 here is harmless (job deleted between
          // overview and detail fetches).
          return;
        }
        const trace = data as unknown as JobTrace;
        setJobTraces((prev) => {
          const next = new Map(prev);
          next.set(activeJobId, trace);
          return next;
        });
      } catch {
        // Network errors on the per-job fetch are non-fatal — keep the
        // overview visible and let the next poll retry.
      } finally {
        if (!cancelled) setActiveJobLoading(false);
      }
    }
    void fetchJobTrace();
    return () => {
      cancelled = true;
    };
  }, [client, isMock, logout, sessionId, activeJobId, jobRefreshKey]);

  // Polling — visibility-aware, two-tier.
  //   * Active job non-terminal → bump `jobRefreshKey` every 2s.
  //   * Any *other* job non-terminal → bump `overviewRefreshKey` every
  //     10s so the sidebar reflects state changes elsewhere.
  // If everything is fully terminal, no polling at all.
  const activeIsLive =
    !!activeJobSummary &&
    (isJobLive(activeJobSummary.job_status_kind) || traceHasPendingSpan(activeJobTrace));
  const anyJobLive = overview?.jobs.some((j) => isJobLive(j.job_status_kind)) ?? false;
  const polling = activeIsLive || anyJobLive;

  useEffect(() => {
    if (isMock || !polling) return;
    const tick = () => {
      if (document.visibilityState !== 'visible') return;
      if (activeIsLive) setJobRefreshKey((k) => k + 1);
      setOverviewRefreshKey((k) => k + 1);
    };
    const cadence = activeIsLive ? POLL_ACTIVE_MS : POLL_TERMINAL_MS;
    const t = window.setInterval(tick, cadence);
    return () => window.clearInterval(t);
  }, [isMock, polling, activeIsLive]);

  const spanIndex = useMemo(() => deriveSpanIndex(activeJobTrace), [activeJobTrace]);

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
      // Stays within the active job — `tool_call` `triggered_by` only
      // ever points at an LLM span in the same step.
      updateUrl({ span: llmSpanId });
      const stepId = findStepIdForSpan(activeJobTrace, llmSpanId);
      if (stepId) {
        // setTimeout so the panel re-renders with the new selection first.
        setTimeout(() => {
          document
            .querySelector(`[data-step-id="${stepId}"]`)
            ?.scrollIntoView({ behavior: 'smooth', block: 'center' });
        }, 16);
      }
    },
    [activeJobTrace, updateUrl],
  );

  const handleDrillIntoChild = useCallback(
    (childSessionId: string) => {
      navigate(`/traces/${encodeURIComponent(childSessionId)}`);
    },
    [navigate],
  );

  const handleManualRefresh = useCallback(() => {
    setOverviewRefreshKey((k) => k + 1);
    setJobRefreshKey((k) => k + 1);
  }, []);

  if (overviewLoading && !overview) {
    return (
      <div className="p-5 text-ink-soft text-[0.95rem] flex items-center gap-2">
        <RiLoader4Line className="animate-spin" /> Loading trace…
      </div>
    );
  }
  if (error && !overview) {
    return (
      <div className="p-5">
        <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      </div>
    );
  }
  if (!overview) {
    return <div className="p-5 text-ink-soft">No trace.</div>;
  }

  const selectedSpan = spanIdParam ? (spanIndex.get(spanIdParam) ?? null) : null;
  const activeJobIndex = activeJobSummary ? overview.jobs.indexOf(activeJobSummary) : 0;

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
              <RiCpuLine /> Session: {overview.session_id}
            </span>
            <span>
              {overview.jobs.length} {overview.jobs.length === 1 ? 'job' : 'jobs'}
            </span>
            {activeJobSummary && (
              <span className="inline-flex items-center gap-2">
                <span className="text-ink-soft uppercase text-[0.7rem] font-bold tracking-wider">
                  {overview.jobs.length === 1 ? 'Tokens' : `Job #${activeJobIndex + 1}`}
                </span>
                {(() => {
                  const t = activeJobTrace
                    ? traceTokens(activeJobTrace)
                    : summaryTokens(activeJobSummary);
                  const d = jobDurationMs(activeJobSummary, activeJobTrace);
                  return (
                    <>
                      <span className="text-ink">↑ {t.inputTotal.toLocaleString()}</span>
                      <span className="text-ink">↓ {t.output.toLocaleString()}</span>
                      <span className="text-ink-soft">• {formatDuration(d)}</span>
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
          onClick={handleManualRefresh}
          disabled={overviewLoading || activeJobLoading || isMock}
          className="!py-2 !px-3 !text-[0.85rem] h-9 gap-1.5"
        >
          <RiRefreshLine className="text-lg shrink-0" /> Refresh
        </Button>
      </div>

      <div className="flex-1 flex overflow-hidden min-h-0">
        <JobSidebar
          jobs={overview.jobs}
          active={activeJobId}
          interjectionCounts={interjectionCountByJob}
          onChange={(j) => updateUrl({ job: j, span: null })}
        />

        <div className="flex-1 overflow-y-scroll p-5">
          <div className="max-w-4xl mx-auto space-y-4 pb-10">
            {activeJobTrace?.steps.map((rs) => (
              <div key={rs.step.id} data-step-id={rs.step.id}>
                <StepBlock rs={rs} selectedSpanId={spanIdParam} onSelect={handleSelectSpan} />
              </div>
            ))}
            {activeJobLoading && !activeJobTrace && (
              <div className="text-ink-soft text-[0.95rem] italic flex items-center gap-2">
                <RiLoader4Line className="animate-spin" /> Loading job…
              </div>
            )}
            {activeJobTrace && activeJobTrace.steps.length === 0 && (
              messageLog.length > 0 ? (
                // External-agent (claude/codex) jobs record no step/span
                // tree — their internal loop is opaque. Surface the
                // persisted session transcript instead so the run is
                // still inspectable.
                <div className="space-y-2">
                  <div className="text-ink-soft text-[0.7rem] font-bold uppercase tracking-wider">
                    Session transcript
                  </div>
                  <MessageList messages={messageLog.map((r) => r.message)} foldHistory={false} />
                </div>
              ) : (
                <div className="text-ink-soft text-[0.95rem] italic">No steps yet for this job.</div>
              )
            )}
          </div>
        </div>

        {selectedSpan ? (
          <SpanDetailPanel
            span={selectedSpan}
            messageLog={messageLog}
            tab={tabParam}
            onTabChange={(t) => updateUrl({ tab: t })}
            onJumpToLlm={handleJumpToLlm}
            onDrillIn={handleDrillIntoChild}
          />
        ) : (
          <JobSummaryPanel
            summary={activeJobSummary}
            trace={activeJobTrace}
            traceLoading={activeJobLoading}
            messageLog={messageLog}
            jobIndex={activeJobIndex}
            totalJobs={overview.jobs.length}
            interjectionCount={interjectionCountByJob.get(activeJobId) ?? 0}
          />
        )}
      </div>
    </div>
  );
}
