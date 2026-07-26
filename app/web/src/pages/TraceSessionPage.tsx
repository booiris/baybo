import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { useNavigate, useParams, useSearchParams } from 'react-router-dom';
import {
  RiArrowLeftLine,
  RiArrowRightLine,
  RiCornerDownLeftLine,
  RiCornerDownRightLine,
  RiCpuLine,
  RiLoader4Line,
  RiRefreshLine,
} from 'react-icons/ri';
import { IconButton } from '../components/IconButton';
import { Button } from '../components/Button';
import { useAdminClient, useAuth } from '../api/auth';
import { getMockJobTrace, getMockTraceOverview, useMockMode } from '../api/mock';
import type {
  JobStatusKind,
  LlmCallInputs,
  JobTrace,
  ReplayStep,
  SecretKind,
  SessionMessageRow,
  Span,
  SpanEvent,
  ToolEventPayload,
  TraceJobSummary,
  TraceOverview,
} from '../types/trace';
import { resolveInputMessages, resolveToolCallOutput } from '../types/trace';
import { MessageList } from '../components/trace/MessageList';
import { renderWithSanitizeChips, SanitizeChip } from '../components/trace/SanitizeChip';
import { TraceTree } from '../components/trace/TraceTree';
import { TraceOverviewBar } from '../components/trace/TraceOverviewBar';
import type { TraceGroup } from '../components/trace/traceFormat';
import { JobAnchors } from '../components/trace/JobAnchors';
import {
  contentText,
  durationMs,
  formatDuration,
  formatTime,
  jobDurationMs,
  jobInputText,
  jobOutputText,
  jobQueuedMs,
  OutcomeBadge,
  spanVisual,
  stepSummaryText,
  stepVisual,
  summaryTokens,
  traceTokens,
} from '../components/trace/traceFormat';
import {
  findSpan,
  findStep,
  isExternalAgentJob,
  isJobLive,
  neededJobIds,
  traceHasPendingSpan,
} from '../components/trace/traceTreeModel';

const POLL_ACTIVE_MS = 2_000;
const POLL_TERMINAL_MS = 10_000;

type DetailTab = 'io' | 'meta' | 'events';

// ── Right-hand detail panel (per-kind) ───────────────────────────────

function sanitizeKindHint(events: SpanEvent[] | undefined): SecretKind | undefined {
  for (const e of events ?? []) {
    if (e.kind.kind === 'sanitize_hit' && e.kind.kinds.length > 0) {
      return e.kind.kinds[0];
    }
  }
  return undefined;
}

function LlmCallDetail({ span, messageLog }: { span: Span; messageLog: SessionMessageRow[] }) {
  if (span.kind.kind !== 'llm_call') return null;
  const { begin, result } = span.kind;
  const hint = sanitizeKindHint(span.events);
  const failureReason = span.outcome.outcome === 'failed' ? span.outcome.reason : null;
  const inputMessages = resolveInputMessages(begin.input_messages, messageLog, span.started_at);

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
              <div className="text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">Emitted tool calls</div>
              {result.tool_calls.map((tc) => (
                <div key={tc.id} className="border-2 border-black rounded-md p-2 bg-canvas font-mono text-[0.8rem]">
                  <div className="text-ink-soft">
                    {tc.id} → <span className="text-brand font-bold">{tc.name}</span>
                  </div>
                  <pre className="mt-1 whitespace-pre-wrap break-all">{JSON.stringify(tc.arguments, null, 2)}</pre>
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
  messageLog,
  onJumpToLlm,
}: {
  span: Span;
  messageLog: SessionMessageRow[];
  onJumpToLlm: (llmSpanId: string) => void;
}) {
  if (span.kind.kind !== 'tool_call') return null;
  const { begin, result } = span.kind;
  const hint = sanitizeKindHint(span.events);
  const failureReason = span.outcome.outcome === 'failed' ? span.outcome.reason : null;
  // A larger output rides as a transcript pointer keyed by `tool_use_id` —
  // resolve it before rendering or the panel shows the raw `$baybo_ref` object.
  const output = result ? resolveToolCallOutput(result.output, messageLog, span.started_at) : null;

  return (
    <div className="space-y-6">
      <section>
        <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Tool</h4>
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
        <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Params</h4>
        <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
          {renderWithSanitizeChips(JSON.stringify(begin.params, null, 2), hint)}
        </pre>
      </section>

      {result && (
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
            Output {result.success ? '(success)' : '(failed)'}
          </h4>
          {result.output_truncated_from != null && (
            <p className="mb-2 font-mono text-[0.75rem] text-warning font-bold">
              partial output — {result.output_truncated_from.toLocaleString()} serialized bytes
              originally. Tool results entering model context use the same 32 KiB ceiling.
            </p>
          )}
          <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] bg-gray-50 border-2 border-black rounded-md p-3">
            {renderWithSanitizeChips(
              typeof output === 'string' ? output : JSON.stringify(output, null, 2),
              hint,
            )}
          </pre>
        </section>
      )}
    </div>
  );
}

function SubagentStubDetail({ span, onDrillIn }: { span: Span; onDrillIn: (sessionId: string) => void }) {
  if (span.kind.kind !== 'subagent_stub') return null;
  const childId = span.kind.child_session_id;
  return (
    <div className="space-y-4">
      <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Subagent</h4>
      <p className="text-[0.85rem] text-ink-soft">
        This stub bounds the parent's wait window. The actual work runs in the child session.
      </p>
      <Button variant="primary" onClick={() => onDrillIn(childId)} className="!py-2 !px-4 !text-[0.85rem] gap-2">
        Open child session <RiArrowRightLine />
      </Button>
    </div>
  );
}

function MetaTab({ span }: { span: Span }) {
  const ms = durationMs(span);
  const visual = spanVisual(span.kind.kind);
  const baseRows: [string, ReactNode][] = [
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

function ToolEventRow({ action, payload }: { action: string; payload: ToolEventPayload }) {
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
        {payload.content_type && <div className="text-ink-soft break-all">{payload.content_type}</div>}
        {payload.body_preview && (
          <details>
            <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
              Body preview ({payload.body_preview.length} chars)
            </summary>
            <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">{payload.body_preview}</pre>
          </details>
        )}
      </div>
    );
  }
  if (payload.type === 'parse_failure') {
    return (
      <div className="space-y-1 font-mono text-[0.85rem]">
        <div className="flex items-baseline justify-between gap-3">
          <span className="break-all">{action}</span>
          <span className="font-bold shrink-0 text-warning">parse failed</span>
        </div>
        <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">{payload.command}</pre>
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
        <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">{payload.input}</pre>
      </details>
      <details>
        <summary className="cursor-pointer text-[0.75rem] uppercase font-bold tracking-wider text-ink-soft">
          Output ({payload.output.length} chars)
        </summary>
        <pre className="mt-1 whitespace-pre-wrap break-all text-[0.75rem] text-ink-soft">{payload.output}</pre>
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
        <div key={`${e.span_id}-${e.seq}`} className="border-2 border-black rounded-md p-3 bg-canvas">
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
  tab: DetailTab;
  onTabChange: (t: DetailTab) => void;
  onJumpToLlm: (id: string) => void;
  onDrillIn: (id: string) => void;
}) {
  const visual = spanVisual(span.kind.kind);
  const Icon = visual.icon;
  const events = span.events ?? [];
  const eventCount = events.length;
  const ms = durationMs(span);

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <div className="flex flex-col border-b-[3px] border-black bg-canvas">
        <div className="flex items-center p-4 pb-2">
          <div className="flex items-center gap-3 min-w-0">
            <div className={`w-10 h-10 rounded-full border-2 border-black flex items-center justify-center shrink-0 ${visual.bg}`}>
              <Icon className={`${visual.accent} text-xl`} />
            </div>
            <div className="min-w-0">
              <h3 className="font-bold uppercase tracking-wider leading-tight text-[1rem] truncate">{visual.label}</h3>
              <div className="text-ink-soft text-[0.8rem] font-mono">
                {span.kind.kind} • {formatDuration(ms)}
              </div>
            </div>
          </div>
        </div>
        <div className="flex px-4 gap-6 relative top-[3px]">
          {(['io', 'meta', 'events'] as const).map((t) => {
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
            <ToolCallDetail span={span} messageLog={messageLog} onJumpToLlm={onJumpToLlm} />
          ) : (
            <SubagentStubDetail span={span} onDrillIn={onDrillIn} />
          ))}
        {tab === 'meta' && <MetaTab span={span} />}
        {tab === 'events' && <EventsTab events={events} />}
      </div>
    </div>
  );
}

// ── Step-level detail (no span selected) ─────────────────────────────

function StepDetail({
  rs,
  jobId,
  onSelectSpan,
  onDrillIn,
}: {
  rs: ReplayStep;
  jobId: string;
  onSelectSpan: (jobId: string, spanId: string) => void;
  onDrillIn: (sessionId: string) => void;
}) {
  const { step, spans } = rs;
  const visual = stepVisual(step.kind.kind);
  const Icon = visual.icon;
  const ms = durationMs(step);
  const failureReason =
    step.outcome.outcome === 'failed'
      ? step.outcome.reason
      : step.outcome.outcome === 'cancelled'
        ? `cancelled (${step.outcome.reason})`
        : null;

  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <div className="flex items-center gap-3 min-w-0 p-4 border-b-[3px] border-black bg-canvas">
        <div className={`w-10 h-10 rounded-full border-2 border-black flex items-center justify-center shrink-0 ${visual.bg}`}>
          <Icon className={`${visual.accent} text-xl`} />
        </div>
        <div className="min-w-0">
          <h3 className="font-bold uppercase tracking-wider leading-tight text-[1rem] truncate">{visual.label}</h3>
          <div className="text-ink-soft text-[0.8rem] font-mono flex items-center gap-2">
            <OutcomeBadge state={step.outcome} /> • {formatDuration(ms)}
          </div>
        </div>
      </div>

      <div className="flex-1 overflow-y-scroll p-5 space-y-6">
        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Summary</h4>
          <div className="font-mono text-[0.85rem] text-ink-soft">{stepSummaryText(step, spans)}</div>
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

        {step.kind.kind === 'subagent' && (
          <section>
            <p className="text-[0.85rem] text-ink-soft mb-2">
              The actual work runs in the child session spawned by this step.
            </p>
            <Button
              variant="primary"
              onClick={() => onDrillIn(step.kind.kind === 'subagent' ? step.kind.child_session_id : '')}
              className="!py-2 !px-4 !text-[0.85rem] gap-2"
            >
              Open child session <RiArrowRightLine />
            </Button>
          </section>
        )}

        {spans.length > 0 && (
          <section>
            <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">
              Spans ({spans.length})
            </h4>
            <div className="space-y-2">
              {spans.map((s) => {
                const sv = spanVisual(s.kind.kind);
                const SvIcon = sv.icon;
                const title =
                  s.kind.kind === 'llm_call'
                    ? s.kind.begin.model_id
                    : s.kind.kind === 'tool_call'
                      ? s.kind.begin.tool_name
                      : `subagent → ${s.kind.child_session_id}`;
                return (
                  <button
                    key={s.id}
                    type="button"
                    onClick={() => onSelectSpan(jobId, s.id)}
                    className="w-full text-left flex items-center gap-3 px-3 py-2 border-2 border-black rounded-md bg-white hover:bg-gray-50 hover:shadow-brutal-xs transition-all"
                  >
                    <div className={`w-8 h-8 rounded-full border-2 border-black flex items-center justify-center shrink-0 ${sv.bg}`}>
                      <SvIcon className={`${sv.accent} text-base`} />
                    </div>
                    <div className="flex-1 min-w-0">
                      <div className="font-bold text-[0.85rem] truncate">{title}</div>
                      <div className="text-[0.72rem] text-ink-soft font-mono">{sv.label}</div>
                    </div>
                    <OutcomeBadge state={s.outcome} />
                  </button>
                );
              })}
            </div>
          </section>
        )}
      </div>
    </div>
  );
}

// ── Job-level summary (default detail) ───────────────────────────────

function JobSummaryPanel({
  summary,
  trace,
  traceLoading,
  messageLog,
  jobIndex,
  totalJobs,
  interjections,
}: {
  summary: TraceJobSummary | undefined;
  trace: JobTrace | undefined;
  traceLoading: boolean;
  messageLog: SessionMessageRow[];
  jobIndex: number;
  totalJobs: number;
  interjections: SessionMessageRow[];
}) {
  const interjectionCount = interjections.length;
  if (!summary) {
    return <div className="flex-1 min-h-0 p-5 text-ink-soft italic text-[0.85rem]">No job available.</div>;
  }
  const { input, output, cached, cacheCreate, inputTotal } = trace ? traceTokens(trace) : summaryTokens(summary);
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
    <div className="flex-1 min-h-0 flex flex-col">
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
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Input</h4>
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

        {interjectionCount > 0 && (
          <section>
            <h4 className="flex items-center gap-1.5 font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-warn pb-1 text-warn">
              <RiCornerDownLeftLine className="text-[0.9rem]" />
              Interjections ({interjectionCount})
            </h4>
            <div className="space-y-2">
              {interjections.map((r) => (
                <div key={r.ordinal} className="border-2 border-warn rounded-md bg-warn/5 p-3">
                  <div className="mb-1 font-mono text-[0.65rem] uppercase tracking-wider text-warn">
                    {formatTime(r.created_at)}
                  </div>
                  <pre className="whitespace-pre-wrap break-all font-mono text-[0.85rem] text-ink max-h-48 overflow-y-auto">
                    {contentText(r.message.content) || '[no text]'}
                  </pre>
                </div>
              ))}
            </div>
          </section>
        )}

        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Output</h4>
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
              {summary.job_status_kind === 'completed' ? 'No output text.' : 'Awaiting final output…'}
            </div>
          )}
        </section>

        <section>
          <h4 className="font-bold uppercase tracking-wider text-[0.8rem] mb-2 border-b-2 border-black pb-1">Activity</h4>
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
          Select a step or span in the tree to inspect its inputs, outputs, and metadata.
        </section>
      </div>
    </div>
  );
}

function TranscriptPanel({ messageLog }: { messageLog: SessionMessageRow[] }) {
  return (
    <div className="flex-1 min-h-0 flex flex-col">
      <div className="border-b-[3px] border-black bg-canvas p-4">
        <h3 className="font-bold uppercase tracking-wider text-[1rem]">Session transcript</h3>
        <div className="text-ink-soft text-[0.8rem] font-mono">external agent · no step tree</div>
      </div>
      <div className="flex-1 overflow-y-scroll p-5">
        {messageLog.length > 0 ? (
          <MessageList messages={messageLog.map((r) => r.message)} foldHistory={false} />
        ) : (
          <div className="text-ink-soft text-[0.85rem] italic">No transcript recorded.</div>
        )}
      </div>
    </div>
  );
}

// ── Breadcrumb (orientation for the right-hand detail) ───────────────

interface Crumb {
  label: string;
  onClick?: () => void;
}

function Breadcrumb({ crumbs }: { crumbs: Crumb[] }) {
  return (
    <div className="shrink-0 flex items-center gap-1 flex-wrap px-4 py-2 border-b-2 border-black bg-canvas font-mono text-[0.7rem] text-ink-soft">
      {crumbs.map((c, i) => (
        <span key={i} className="inline-flex items-center gap-1 min-w-0">
          {i > 0 && <span className="text-ink-soft/50">›</span>}
          {c.onClick ? (
            <button type="button" onClick={c.onClick} className="hover:text-ink hover:underline cursor-pointer truncate max-w-[160px]">
              {c.label}
            </button>
          ) : (
            <span className="truncate max-w-[160px]">{c.label}</span>
          )}
        </span>
      ))}
    </div>
  );
}

// ── Interjection index ───────────────────────────────────────────────

// Precomputed lookup over a session's transcript so the per-span interjection
// count is O(log N) instead of rebuilding + re-scanning the whole message log
// for every LLM span on every poll tick.
interface InterjectionIndex {
  // Interjection rows in ascending ordinal order (a small subset), carrying
  // just what the hydration window predicate needs.
  entries: { ordinal: number; supersededBy: number | null }[];
  // Every row's ordinal (ascending) paired with a running max of its parsed
  // created-at — the input for the hydration epoch guard.
  ordinals: number[];
  prefixMaxCreated: number[];
}

// Highest ordinal in a transcript page, or undefined when empty — the
// `since_ordinal` cursor for the next incremental overview poll.
function maxOrdinal(rows: SessionMessageRow[]): number | undefined {
  if (rows.length === 0) return undefined;
  let max = rows[0].ordinal;
  for (const r of rows) if (r.ordinal > max) max = r.ordinal;
  return max;
}

function buildInterjectionIndex(log: SessionMessageRow[]): InterjectionIndex {
  const ordered = [...log].sort((a, b) => a.ordinal - b.ordinal);
  const entries: { ordinal: number; supersededBy: number | null }[] = [];
  const ordinals: number[] = [];
  const prefixMaxCreated: number[] = [];
  let running = Number.NEGATIVE_INFINITY;
  for (const row of ordered) {
    const createdMs = new Date(row.created_at).getTime();
    if (createdMs > running) running = createdMs;
    ordinals.push(row.ordinal);
    prefixMaxCreated.push(running);
    if (row.message.source === 'user_interjection') {
      entries.push({ ordinal: row.ordinal, supersededBy: row.superseded_by ?? null });
    }
  }
  return { entries, ordinals, prefixMaxCreated };
}

// Max parsed created-at over rows with ordinal ≤ `lastOrdinal`. Excluding
// superseded rows never lowers this (a superseded row predates its
// higher-ordinal replacement), so it equals the max over the reconstructed
// prefix — exactly what the hydration epoch guard tests.
function maxCreatedUpToOrdinal(index: InterjectionIndex, lastOrdinal: number): number {
  const { ordinals, prefixMaxCreated } = index;
  let lo = 0;
  let hi = ordinals.length - 1;
  let found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (ordinals[mid] <= lastOrdinal) {
      found = mid;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  return found >= 0 ? prefixMaxCreated[found] : Number.NEGATIVE_INFINITY;
}

// Count of `user_interjection` messages an LLM span's resolved input folds in,
// without rebuilding the message list. Mirrors `resolveInputMessages` /
// `hydratePersistedInput`: the epoch guard drops the whole input (count 0) when
// the reconstructed prefix holds a row created after the span started;
// otherwise it is (in-window prefix interjections) + (suffix ones).
function interjectionInputCount(
  input: LlmCallInputs,
  spanStartedAt: string,
  index: InterjectionIndex,
): number {
  if (Array.isArray(input)) {
    return input.filter((m) => m.source === 'user_interjection').length;
  }
  const lastOrdinal = input.last_ordinal;
  const spanStart = new Date(spanStartedAt).getTime();
  if (maxCreatedUpToOrdinal(index, lastOrdinal) > spanStart) return 0;
  let count = (input.suffix ?? []).filter((m) => m.source === 'user_interjection').length;
  for (const e of index.entries) {
    if (e.ordinal > lastOrdinal) break;
    if (e.supersededBy == null || e.supersededBy > lastOrdinal) count++;
  }
  return count;
}

// ── Page ─────────────────────────────────────────────────────────────

export function TraceSessionPage() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const isMock = useMockMode();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [overview, setOverview] = useState<TraceOverview | null>(null);
  const [jobTraces, setJobTraces] = useState<Map<string, JobTrace>>(() => new Map());
  const [loadingJobs, setLoadingJobs] = useState<Set<string>>(() => new Set());
  const [userToggles, setUserToggles] = useState<Map<string, boolean>>(() => new Map());
  const [overviewLoading, setOverviewLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  const [filterRaw, setFilterRaw] = useState('');
  const [filter, setFilter] = useState('');
  const [failuresOnly, setFailuresOnly] = useState(false);
  // Legend group to highlight across the minimap + tree (null = show all).
  const [highlight, setHighlight] = useState<TraceGroup | null>(null);

  // Lets the overview poll read the transcript it currently holds (its cursor +
  // supersede watermark) without making the fetch effect depend on `overview`
  // (which it sets — that would re-fire itself every tick).
  const overviewRef = useRef<TraceOverview | null>(null);
  overviewRef.current = overview;

  const inFlight = useRef<Set<string>>(new Set());
  // The job_status_kind each cached trace was fetched at, so a status change
  // (live → terminal) triggers a refetch of the finalized tree — a pending-span
  // heuristic alone misses a job cached at a no-pending moment that then ends.
  const fetchedStatus = useRef<Map<string, JobStatusKind>>(new Map());

  // Debounce the tree filter text (matches TracesPage/LogsPage cadence).
  useEffect(() => {
    const t = window.setTimeout(() => setFilter(filterRaw), 250);
    return () => window.clearTimeout(t);
  }, [filterRaw]);
  const filtering = failuresOnly || filter.trim() !== '';

  const sessionId = id ?? '';
  const jobIdParam = searchParams.get('job');
  const stepIdParam = searchParams.get('step');
  const spanIdParam = searchParams.get('span');
  const tabParam = (searchParams.get('tab') as DetailTab | null) ?? 'io';

  // Fetch the overview (session messages + job summaries). A same-session poll
  // pulls only the transcript delta above the cursor it already holds
  // (`since_ordinal`); a fresh session or cold start pulls the full page.
  // `jobs` is always the full (tiny) array — replaced, never merged.
  useEffect(() => {
    let cancelled = false;

    type OverviewFetch =
      | { status: 'ok'; overview: TraceOverview }
      | { status: 'error'; message: string }
      | { status: 'aborted' }; // cancelled or 401 (already logged out)

    async function fetchPage(sinceOrdinal: number | undefined): Promise<OverviewFetch> {
      const { data, error: apiError, response } = await client.GET('/v1/traces/{session_id}', {
        params: {
          path: { session_id: sessionId },
          query: sinceOrdinal != null ? { since_ordinal: sinceOrdinal } : undefined,
        },
      });
      if (cancelled) return { status: 'aborted' };
      if (response.status === 401) {
        logout();
        return { status: 'aborted' };
      }
      if (apiError || !response.ok) {
        return {
          status: 'error',
          message: (apiError as { error?: string })?.error || `HTTP Error ${response.status}`,
        };
      }
      return { status: 'ok', overview: data as unknown as TraceOverview };
    }

    async function loadOverview() {
      if (!sessionId) return;
      if (isMock) {
        setOverview(getMockTraceOverview(sessionId));
        setError(null);
        setOverviewLoading(false);
        return;
      }
      const held = overviewRef.current;
      const sinceOrdinal =
        held != null && held.session_id === sessionId ? maxOrdinal(held.session_messages) : undefined;

      setOverviewLoading(true);
      setError(null);
      try {
        const first = await fetchPage(sinceOrdinal);
        if (first.status === 'aborted') return;
        if (first.status === 'error') {
          setError(first.message);
          return;
        }
        // A moved supersede watermark means a compaction re-marked rows we may
        // already hold: the cached prefix is stale, so drop it and pull the
        // whole transcript once. (`held != null` whenever `sinceOrdinal` is set,
        // but the compiler needs the explicit guard to narrow it.)
        if (
          sinceOrdinal != null &&
          held != null &&
          first.overview.supersede_watermark !== held.supersede_watermark
        ) {
          const full = await fetchPage(undefined);
          if (full.status === 'aborted') return;
          if (full.status === 'error') {
            setError(full.message);
            return;
          }
          setOverview(full.overview);
          return;
        }
        if (sinceOrdinal != null && held != null) {
          // Delta rows are strictly newer than everything held — append them
          // (no dedup) and take the fresh full `jobs` array + watermark.
          setOverview({
            ...first.overview,
            session_messages: [...held.session_messages, ...first.overview.session_messages],
          });
        } else {
          setOverview(first.overview);
        }
      } catch (e) {
        if (cancelled) return;
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (!cancelled) setOverviewLoading(false);
      }
    }
    void loadOverview();
    return () => {
      cancelled = true;
    };
  }, [client, isMock, logout, sessionId, refreshKey]);

  // Reset per-session caches when the session id changes.
  useEffect(() => {
    setJobTraces(new Map());
    setUserToggles(new Map());
    inFlight.current = new Set();
    fetchedStatus.current = new Map();
  }, [sessionId]);

  // Derive the active job id from URL ∩ overview. Default to the oldest job.
  const activeJobId =
    jobIdParam && overview?.jobs.some((j) => j.job_id === jobIdParam)
      ? jobIdParam
      : (overview?.jobs[0]?.job_id ?? '');
  const activeJobSummary = overview?.jobs.find((j) => j.job_id === activeJobId);
  const activeJobTrace = activeJobId ? jobTraces.get(activeJobId) : undefined;
  const messageLog = useMemo(() => overview?.session_messages ?? [], [overview]);
  const interjectionIndex = useMemo(() => buildInterjectionIndex(messageLog), [messageLog]);

  const fetchJobTrace = useCallback(
    async (jobId: string, status?: JobStatusKind) => {
      if (!jobId) return;
      // `inFlight` dedupes concurrent fetches (double-invoke, overlapping
      // polls); the cache-hit skip lives in the caller, which holds current
      // `jobTraces` — so this callback stays a stable identity.
      if (inFlight.current.has(jobId)) return;
      inFlight.current.add(jobId);
      setLoadingJobs((prev) => {
        const next = new Set(prev);
        next.add(jobId);
        return next;
      });
      const record = () => {
        if (status) fetchedStatus.current.set(jobId, status);
      };
      try {
        if (isMock) {
          const mock = getMockJobTrace(sessionId, jobId);
          if (mock) {
            setJobTraces((prev) => new Map(prev).set(jobId, mock));
            record();
          }
          return;
        }
        const { data, error: apiError, response } = await client.GET('/v1/traces/{session_id}/jobs/{job_id}', {
          params: { path: { session_id: sessionId, job_id: jobId } },
        });
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          // Non-fatal — keep the overview visible; the next poll retries.
          return;
        }
        setJobTraces((prev) => new Map(prev).set(jobId, data as unknown as JobTrace));
        record();
      } catch {
        // Network errors on the per-job fetch are non-fatal.
      } finally {
        inFlight.current.delete(jobId);
        setLoadingJobs((prev) => {
          const next = new Set(prev);
          next.delete(jobId);
          return next;
        });
      }
    },
    [client, isMock, logout, sessionId],
  );

  // The jobs whose step tree we need loaded: the failure path + expanded +
  // selection. While a filter is active every job must load — a content filter
  // that searched only the already-loaded jobs would silently hide matches.
  const neededIds = useMemo(() => {
    if (!overview) return [];
    if (filtering) return overview.jobs.map((j) => j.job_id);
    return neededJobIds(overview.jobs, userToggles, activeJobId);
  }, [overview, userToggles, activeJobId, filtering]);

  // Load missing needed jobs and keep live ones fresh. A poll bumps
  // `refreshKey` → overview refetches → `neededIds` gets a new identity → this
  // effect re-runs, refreshing live jobs at the fast cadence. `jobTraces` is
  // read as a snapshot, NOT a dep — a job becomes needed solely via
  // `neededIds`/`overview`, and depending on `jobTraces` would make a live
  // job's completed fetch re-trigger and refetch forever.
  useEffect(() => {
    if (!overview) return;
    for (const jid of neededIds) {
      const summary = overview.jobs.find((j) => j.job_id === jid);
      const live = summary ? isJobLive(summary.job_status_kind) : false;
      const missing = !jobTraces.has(jid);
      if (missing || live) void fetchJobTrace(jid, summary?.job_status_kind);
    }
    // `jobTraces` is a deliberate snapshot read: adding it as a dep would make a
    // completed fetch re-fire this effect and refetch a live job forever.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [neededIds, overview, fetchJobTrace]);

  // Completion catcher: a job whose cached trace was fetched at a now-stale
  // status (its last fetch happened while live, before it went terminal) is
  // refetched once so the finalized tree/output loads. Keyed on `jobTraces` so
  // it fires even after polling stops (the fast-path load effect above can miss
  // this when a per-job fetch outlives the poll interval). Excludes live jobs,
  // so `status` settling makes it converge instead of looping.
  useEffect(() => {
    if (!overview) return;
    for (const summary of overview.jobs) {
      if (!jobTraces.has(summary.job_id)) continue;
      if (isJobLive(summary.job_status_kind)) continue;
      if (fetchedStatus.current.get(summary.job_id) !== summary.job_status_kind) {
        void fetchJobTrace(summary.job_id, summary.job_status_kind);
      }
    }
  }, [jobTraces, overview, fetchJobTrace]);

  // Interjections folded into each job (the [started, ended) window contains
  // the interjection row's created_at). Jobs run sequentially → unambiguous.
  const interjectionsByJob = useMemo(() => {
    const byJob = new Map<string, SessionMessageRow[]>();
    const interjections = (overview?.session_messages ?? []).filter((r) => r.message.source === 'user_interjection');
    if (interjections.length === 0) return byJob;
    for (const job of overview?.jobs ?? []) {
      const startIso = job.started_at ?? job.created_at;
      if (!startIso) continue;
      const start = new Date(startIso).getTime();
      const end = job.ended_at ? new Date(job.ended_at).getTime() : Number.POSITIVE_INFINITY;
      const rows = interjections.filter((r) => {
        const t = new Date(r.created_at).getTime();
        return t >= start && t < end;
      });
      if (rows.length > 0) byJob.set(job.job_id, rows);
    }
    return byJob;
  }, [overview]);

  const interjectionCountByJob = useMemo(() => {
    const counts = new Map<string, number>();
    for (const [jid, rows] of interjectionsByJob) counts.set(jid, rows.length);
    return counts;
  }, [interjectionsByJob]);

  // LLM-call spans whose input first folds in a mid-turn interjection — marked
  // across EVERY loaded job (the tree renders spans for any expanded job, not
  // just the active one). The monotonic count resets per job.
  const interjectionSpanIds = useMemo(() => {
    const ids = new Set<string>();
    for (const trace of jobTraces.values()) {
      const llmSpans: Span[] = [];
      for (const rs of trace.steps) {
        for (const span of rs.spans) {
          if (span.kind.kind === 'llm_call') llmSpans.push(span);
        }
      }
      llmSpans.sort((a, b) => new Date(a.started_at).getTime() - new Date(b.started_at).getTime());
      let seen = 0;
      let first = true;
      for (const span of llmSpans) {
        if (span.kind.kind !== 'llm_call') continue;
        // Counted off the precomputed index, NOT by rebuilding the message list
        // per span — that was O(spans × transcript) on every poll tick.
        const count = interjectionInputCount(span.kind.begin.input_messages, span.started_at, interjectionIndex);
        // Seed the baseline from the job's FIRST LLM input: interjections
        // already carried in from earlier jobs (persisted transcripts replay the
        // whole history) are not new to this job, so only a count that RISES
        // above the baseline marks the iteration this job's steering entered.
        if (first) {
          seen = count;
          first = false;
        } else if (count > seen) {
          ids.add(span.id);
        }
        seen = Math.max(seen, count);
      }
    }
    return ids;
  }, [jobTraces, interjectionIndex]);

  // Polling — visibility-aware, two-tier. A single `refreshKey` bump refetches
  // the overview; that cascades (via `neededIds`) into refetching live jobs.
  const activeIsLive =
    !!activeJobSummary && (isJobLive(activeJobSummary.job_status_kind) || traceHasPendingSpan(activeJobTrace));
  const anyJobLive = overview?.jobs.some((j) => isJobLive(j.job_status_kind)) ?? false;
  const polling = activeIsLive || anyJobLive;

  useEffect(() => {
    if (isMock || !polling) return;
    const tick = () => {
      if (document.visibilityState !== 'visible') return;
      setRefreshKey((k) => k + 1);
    };
    const cadence = activeIsLive ? POLL_ACTIVE_MS : POLL_TERMINAL_MS;
    const t = window.setInterval(tick, cadence);
    return () => window.clearInterval(t);
  }, [isMock, polling, activeIsLive]);

  // When the selection changes via URL — deep link, breadcrumb, jump-to-llm,
  // back/forward — drop any stale collapse override on its ancestors so the
  // selected node is revealed (the design's "URL-selected node auto-expands its
  // ancestors"). Keyed on the URL params only, NOT on jobTraces, so a poll
  // refetch never re-expands a node the user deliberately collapsed.
  useEffect(() => {
    setUserToggles((prev) => {
      const ancestors: string[] = [];
      if (jobIdParam) ancestors.push(jobIdParam);
      if (stepIdParam) ancestors.push(stepIdParam);
      if (spanIdParam) {
        const owning = findSpan(jobTraces.get(activeJobId), spanIdParam)?.stepId;
        if (owning) ancestors.push(owning);
      }
      if (!ancestors.some((a) => prev.has(a))) return prev;
      const next = new Map(prev);
      for (const a of ancestors) next.delete(a);
      return next;
    });
    // `jobTraces` is read as a snapshot, deliberately NOT a dep: re-running on
    // every poll refetch would re-expand a node the user just collapsed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [jobIdParam, stepIdParam, spanIdParam, activeJobId]);

  const updateUrl = useCallback(
    (next: Partial<{ job: string | null; step: string | null; span: string | null; tab: string | null }>) => {
      const sp = new URLSearchParams(searchParams);
      for (const [k, v] of Object.entries(next)) {
        if (v == null || v === '') sp.delete(k);
        else sp.set(k, v);
      }
      setSearchParams(sp, { replace: true });
    },
    [searchParams, setSearchParams],
  );

  const handleToggle = useCallback((nodeId: string, currentlyOpen: boolean) => {
    // Persist the flip of the *displayed* state — the tree passes the current
    // open state so a default-expanded (failure-path) node collapses on click
    // and a default-collapsed node expands, both correctly.
    setUserToggles((prev) => new Map(prev).set(nodeId, !currentlyOpen));
  }, []);

  const handleSelectJob = useCallback(
    (jobId: string) => updateUrl({ job: jobId, step: null, span: null }),
    [updateUrl],
  );
  // A job anchor is a "take me to this job" action, so it must clear an active
  // filter — otherwise the filter can hide the very job it just selected and the
  // tree shows nothing while the detail panel switches.
  const handleJumpToJob = useCallback(
    (jobId: string) => {
      setFailuresOnly(false);
      setFilterRaw('');
      setFilter('');
      handleSelectJob(jobId);
    },
    [handleSelectJob],
  );

  const handleSelectStep = useCallback(
    (jobId: string, stepId: string) => updateUrl({ job: jobId, step: stepId, span: null }),
    [updateUrl],
  );
  const handleSelectSpan = useCallback(
    (jobId: string, spanId: string) => updateUrl({ job: jobId, span: spanId, step: null, tab: tabParam }),
    [tabParam, updateUrl],
  );

  const handleJumpToLlm = useCallback(
    (llmSpanId: string) => {
      updateUrl({ span: llmSpanId, step: null });
      const found = findSpan(activeJobTrace, llmSpanId);
      if (found) {
        setTimeout(() => {
          document.querySelector(`[data-step-id="${found.stepId}"]`)?.scrollIntoView({ behavior: 'smooth', block: 'center' });
        }, 16);
      }
    },
    [activeJobTrace, updateUrl],
  );

  const handleDrillIntoChild = useCallback(
    (childSessionId: string) => navigate(`/traces/${encodeURIComponent(childSessionId)}`),
    [navigate],
  );

  const handleManualRefresh = useCallback(() => setRefreshKey((k) => k + 1), []);

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

  const selectedSpan = spanIdParam ? (findSpan(activeJobTrace, spanIdParam)?.span ?? null) : null;
  const selectedStepRs = !selectedSpan && stepIdParam ? findStep(activeJobTrace, stepIdParam) : null;
  const activeJobIndex = activeJobSummary ? overview.jobs.indexOf(activeJobSummary) : 0;
  const externalAgent =
    !selectedSpan &&
    !selectedStepRs &&
    !!activeJobSummary &&
    isExternalAgentJob(activeJobTrace, activeJobSummary.job_status_kind);

  const activeTokens = activeJobTrace
    ? traceTokens(activeJobTrace)
    : activeJobSummary
      ? summaryTokens(activeJobSummary)
      : null;

  // Breadcrumb: session › Job #i › [step] › [span].
  const crumbs: Crumb[] = [{ label: `Session ${overview.session_id.slice(0, 10)}` }];
  if (activeJobSummary) {
    crumbs.push({ label: `Job #${activeJobIndex + 1}`, onClick: () => handleSelectJob(activeJobId) });
  }
  const spanStepId = selectedSpan
    ? findSpan(activeJobTrace, selectedSpan.id)?.stepId ?? null
    : selectedStepRs?.step.id ?? null;
  if (spanStepId) {
    const rs = findStep(activeJobTrace, spanStepId);
    if (rs) {
      crumbs.push({
        label: stepVisual(rs.step.kind.kind).label,
        onClick: () => handleSelectStep(activeJobId, rs.step.id),
      });
    }
  }
  if (selectedSpan) {
    crumbs.push({ label: spanVisual(selectedSpan.kind.kind).label });
  }

  return (
    <div className="flex flex-col h-full overflow-hidden bg-canvas">
      <div className="p-5 shrink-0 flex items-center gap-4 bg-canvas border-b-[3px] border-black z-10">
        <IconButton onClick={() => navigate(-1)} aria-label="Go back">
          <RiArrowLeftLine />
        </IconButton>
        <div className="flex-1 min-w-0">
          <h2 className="text-[1.4rem] font-bold uppercase -tracking-[0.05em] leading-tight">TRACE DETAILS</h2>
          <div className="text-ink-soft text-[0.85rem] font-mono truncate flex items-center gap-3 flex-wrap">
            <span className="inline-flex items-center gap-1">
              <RiCpuLine /> Session: {overview.session_id}
            </span>
            <span>
              {overview.jobs.length} {overview.jobs.length === 1 ? 'job' : 'jobs'}
            </span>
            {activeJobSummary && activeTokens && (
              <span className="inline-flex items-center gap-2">
                <span className="text-ink-soft uppercase text-[0.7rem] font-bold tracking-wider">
                  {overview.jobs.length === 1 ? 'Tokens' : `Job #${activeJobIndex + 1}`}
                </span>
                <span className="text-ink">↑ {activeTokens.inputTotal.toLocaleString()}</span>
                <span className="text-ink">↓ {activeTokens.output.toLocaleString()}</span>
                <span className="text-ink-soft">• {formatDuration(jobDurationMs(activeJobSummary, activeJobTrace))}</span>
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
          disabled={overviewLoading || isMock}
          className="!py-2 !px-3 !text-[0.85rem] h-9 gap-1.5"
        >
          <RiRefreshLine className="text-lg shrink-0" /> Refresh
        </Button>
      </div>

      <TraceOverviewBar
        overview={overview}
        jobTraces={jobTraces}
        loadingJobs={loadingJobs}
        highlight={highlight}
        onHighlight={setHighlight}
        selectedSpanId={selectedSpan?.id ?? null}
        selectedStepId={selectedStepRs?.step.id ?? null}
        onSelectSpan={handleSelectSpan}
        onSelectStep={handleSelectStep}
      />

      <div className="flex-1 flex overflow-hidden min-h-0">
        <JobAnchors
          overview={overview}
          jobTraces={jobTraces}
          activeJobId={activeJobId}
          onSelectJob={handleJumpToJob}
        />
        <TraceTree
          overview={overview}
          jobTraces={jobTraces}
          loadingJobs={loadingJobs}
          userToggles={userToggles}
          onToggle={handleToggle}
          selectedJobId={activeJobId}
          selectedStepId={selectedStepRs?.step.id ?? null}
          selectedSpanId={selectedSpan?.id ?? null}
          onSelectJob={handleSelectJob}
          onSelectStep={handleSelectStep}
          onSelectSpan={handleSelectSpan}
          interjectionCountByJob={interjectionCountByJob}
          interjectionSpanIds={interjectionSpanIds}
          messageLog={messageLog}
          highlight={highlight}
          filterRaw={filterRaw}
          onFilterRawChange={setFilterRaw}
          failuresOnly={failuresOnly}
          onToggleFailures={() => setFailuresOnly((v) => !v)}
          filter={filter}
        />

        <aside className="w-[480px] shrink-0 border-l-[3px] border-black bg-surface flex flex-col z-20 shadow-[-4px_0_0_0_rgba(0,0,0,0.1)]">
          <Breadcrumb crumbs={crumbs} />
          {selectedSpan ? (
            <SpanDetailPanel
              span={selectedSpan}
              messageLog={messageLog}
              tab={tabParam}
              onTabChange={(t) => updateUrl({ tab: t })}
              onJumpToLlm={handleJumpToLlm}
              onDrillIn={handleDrillIntoChild}
            />
          ) : selectedStepRs ? (
            <StepDetail
              rs={selectedStepRs}
              jobId={activeJobId}
              onSelectSpan={handleSelectSpan}
              onDrillIn={handleDrillIntoChild}
            />
          ) : externalAgent ? (
            <TranscriptPanel messageLog={messageLog} />
          ) : (
            <JobSummaryPanel
              summary={activeJobSummary}
              trace={activeJobTrace}
              traceLoading={loadingJobs.has(activeJobId)}
              messageLog={messageLog}
              jobIndex={activeJobIndex}
              totalJobs={overview.jobs.length}
              interjections={interjectionsByJob.get(activeJobId) ?? []}
            />
          )}
        </aside>
      </div>
    </div>
  );
}
