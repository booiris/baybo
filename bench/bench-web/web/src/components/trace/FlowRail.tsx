import { useMemo, useState } from 'react';
import { RiArrowDownSLine, RiArrowRightSLine } from 'react-icons/ri';
import type {
  ChatMessage,
  LifecycleState,
  SessionMessageRow,
  Span,
  Step,
  ToolCallBegin,
  ToolCallResult,
  LlmCallResult,
  LlmCallBegin,
} from '../../types/trace';
import { resolveInputMessages } from '../../types/trace';
import type { BenchTrace } from '../../api/types';
import { cacheRatePct, fmtMs, fmtTokens } from '../../lib/format';
import { renderWithSanitizeChips } from './SanitizeChip';
import { MessageList } from './MessageList';

// ── model ────────────────────────────────────────────────────────────

type Role = 'system' | 'user' | 'llm' | 'tool' | 'aux';

const ROLE: Record<Role, { glyph: string; glyphColor: string; cell: string; bar: string }> = {
  system: { glyph: 'S', glyphColor: 'text-ink-soft', cell: 'bg-ink-soft/40', bar: 'bg-ink-soft' },
  user: { glyph: 'U', glyphColor: 'text-brand', cell: 'bg-brand', bar: 'bg-brand' },
  llm: { glyph: 'L', glyphColor: 'text-ok', cell: 'bg-ok', bar: 'bg-brand' },
  tool: { glyph: 't', glyphColor: 'text-warn', cell: 'bg-warn', bar: 'bg-ink-soft' },
  aux: { glyph: 'A', glyphColor: 'text-ink-soft', cell: 'bg-ink-soft/50', bar: 'bg-ink-soft' },
};

type Detail =
  | { kind: 'text'; text: string }
  | { kind: 'llm'; begin: LlmCallBegin; result?: LlmCallResult | null; startedAt: string }
  | { kind: 'tool'; begin: ToolCallBegin; result?: ToolCallResult | null }
  | { kind: 'aux'; step: Step; spans: Span[] };

interface FlowStep {
  ord: number;
  t: string;
  role: Role;
  label: string;
  durMs: number | null;
  outcome: LifecycleState;
  isError: boolean;
  inTok?: number;
  outTok?: number;
  cachedTok?: number;
  detail: Detail;
}

// ── helpers ──────────────────────────────────────────────────────────

function durMs(start?: string | null, end?: string | null): number | null {
  if (!start || !end) return null;
  const s = new Date(start).getTime();
  const e = new Date(end).getTime();
  if (Number.isNaN(s) || Number.isNaN(e)) return null;
  return e >= s ? e - s : null;
}

function clip(s: string, n: number): string {
  const oneLine = s.replace(/\s+/g, ' ').trim();
  return oneLine.length > n ? `${oneLine.slice(0, n)}…` : oneLine;
}

function textOf(message: ChatMessage): string {
  return message.content.map((b) => ('Text' in b ? b.Text : '')).join('');
}

/** Pull a representative scalar (command / path / url / pattern …) from a
 * tool's params for the one-line preview; fall back to compact JSON. */
function argsPreview(params: unknown): string {
  if (typeof params === 'string') return clip(params, 46);
  if (params && typeof params === 'object') {
    const obj = params as Record<string, unknown>;
    for (const k of ['command', 'cmd', 'path', 'file_path', 'file', 'url', 'pattern', 'query', 'q']) {
      const v = obj[k];
      if (typeof v === 'string' && v) return clip(v, 46);
    }
    const firstStr = Object.values(obj).find((v) => typeof v === 'string' && v) as string | undefined;
    if (firstStr) return clip(firstStr, 46);
  }
  return clip(JSON.stringify(params ?? ''), 46);
}

function llmLabel(result?: LlmCallResult | null): string {
  if (result?.output_content?.trim()) return clip(result.output_content, 60);
  if (result?.tool_calls?.length) return `→ ${result.tool_calls.map((t) => t.name).join(', ')}`;
  if (result?.thinking?.trim()) return '(thinking)';
  return '(llm call)';
}

function asText(value: unknown): string {
  return typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}

/** Flatten a trace into the ordered rail: one row per user/system message,
 * LLM call, tool call, and aux step, sorted by timestamp. */
function buildFlowSteps(trace: BenchTrace): FlowStep[] {
  const raw: Omit<FlowStep, 'ord'>[] = [];

  for (const row of trace.session_messages) {
    const role = row.message.role;
    if (role === 'user' || role === 'system') {
      const text = textOf(row.message);
      raw.push({
        t: row.created_at,
        role,
        label: role === 'system' ? 'system prompt' : clip(text, 70),
        durMs: null,
        outcome: { outcome: 'ok' },
        isError: false,
        detail: { kind: 'text', text },
      });
    }
  }

  for (const job of trace.jobs) {
    for (const { step, spans } of job.steps) {
      if (step.kind.kind === 'llm_iteration') {
        for (const s of spans) {
          if (s.kind.kind === 'llm_call') {
            const result = s.kind.result;
            raw.push({
              t: s.started_at,
              role: 'llm',
              label: llmLabel(result),
              durMs: durMs(s.started_at, s.ended_at),
              outcome: s.outcome,
              isError: s.outcome.outcome === 'failed',
              inTok: result?.input_tokens,
              outTok: result?.output_tokens,
              cachedTok: result?.cached_input_tokens,
              detail: { kind: 'llm', begin: s.kind.begin, result, startedAt: s.started_at },
            });
          } else if (s.kind.kind === 'tool_call') {
            raw.push({
              t: s.started_at,
              role: 'tool',
              label: `${s.kind.begin.tool_name} ${argsPreview(s.kind.begin.params)}`,
              durMs: durMs(s.started_at, s.ended_at),
              outcome: s.outcome,
              isError: s.outcome.outcome === 'failed',
              detail: { kind: 'tool', begin: s.kind.begin, result: s.kind.result },
            });
          }
        }
      } else {
        const label =
          step.kind.kind === 'subagent' ? `subagent → ${step.kind.child_session_id}` : step.kind.kind;
        raw.push({
          t: step.started_at,
          role: 'aux',
          label,
          durMs: durMs(step.started_at, step.ended_at),
          outcome: step.outcome,
          isError: step.outcome.outcome === 'failed',
          detail: { kind: 'aux', step, spans },
        });
      }
    }
  }

  raw.sort((a, b) => (a.t < b.t ? -1 : a.t > b.t ? 1 : 0));
  return raw.map((r, i) => ({ ...r, ord: i + 1 }));
}

type Filter = 'all' | 'failures' | 'tools' | 'llm';

// ── small pieces ─────────────────────────────────────────────────────

function OutcomeWord({ outcome }: { outcome: LifecycleState }) {
  // `ok` stays neutral so green reads as "LLM role" everywhere, not status;
  // failure/cancel keep their semantic red/orange.
  const c =
    outcome.outcome === 'failed'
      ? 'text-err'
      : outcome.outcome === 'cancelled'
        ? 'text-warn'
        : 'text-ink-soft';
  return <span className={`text-[0.6rem] font-bold uppercase ${c}`}>{outcome.outcome}</span>;
}

/** Color/glyph key — roles (sequence cells + row glyphs) vs the red failure flag. */
function Legend() {
  const roles: [Role, string][] = [
    ['user', 'user'],
    ['llm', 'llm'],
    ['tool', 'tool'],
    ['aux', 'aux / system'],
  ];
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[0.62rem] text-ink-soft">
      <span className="font-bold uppercase tracking-wider">legend</span>
      {roles.map(([r, name]) => (
        <span key={r} className="flex items-center gap-1">
          <span className={`inline-block h-2.5 w-2.5 rounded-sm border border-black/40 ${ROLE[r].cell}`} />
          <span className={`font-mono font-bold ${ROLE[r].glyphColor}`}>{ROLE[r].glyph}</span>
          {name}
        </span>
      ))}
      <span className="flex items-center gap-1">
        <span className="inline-block h-2.5 w-2.5 rounded-sm border border-black/40 bg-err" />
        <span className="text-err font-bold">failed</span>
      </span>
      <span className="text-ink-soft/80">· bar = wall-clock vs slowest step</span>
    </div>
  );
}

function DurationBar({ ms, max, role, isError }: { ms: number | null; max: number; role: Role; isError: boolean }) {
  const pct = ms != null && max > 0 ? Math.max(2, Math.round((ms / max) * 100)) : 0;
  const color = isError ? 'bg-err' : ROLE[role].bar;
  return (
    <div className="h-2 w-14 border border-black/30 rounded-sm bg-canvas overflow-hidden shrink-0">
      {ms != null && <div className={`h-full ${color}`} style={{ width: `${pct}%` }} />}
    </div>
  );
}

function Labeled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold mb-1">{label}</div>
      {children}
    </div>
  );
}

function ClippedPre({ text, error = false }: { text: string; error?: boolean }) {
  const LIMIT = 3_000;
  const [open, setOpen] = useState(false);
  const shown = open || text.length <= LIMIT ? text : `${text.slice(0, LIMIT)}\n…`;
  return (
    <div>
      <pre
        className={`whitespace-pre-wrap break-words font-mono text-[0.78rem] bg-gray-50 border-2 border-black rounded-md p-2 ${
          error ? 'text-err' : 'text-ink'
        }`}
      >
        {renderWithSanitizeChips(shown)}
      </pre>
      {text.length > LIMIT && (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="mt-1 text-[0.7rem] uppercase tracking-wider font-bold text-brand hover:underline cursor-pointer"
        >
          {open ? 'collapse' : `show full (${text.length.toLocaleString()} chars)`}
        </button>
      )}
    </div>
  );
}

function StepDetail({ detail, sessionMessages }: { detail: Detail; sessionMessages: SessionMessageRow[] }) {
  if (detail.kind === 'text') {
    return <ClippedPre text={detail.text.trim() || '(empty)'} />;
  }
  if (detail.kind === 'tool') {
    const isError = detail.result ? !detail.result.success : false;
    return (
      <div className="space-y-2">
        <Labeled label="params">
          <ClippedPre text={asText(detail.begin.params)} />
        </Labeled>
        {detail.result && (
          <Labeled label={`result (${detail.result.success ? 'ok' : 'error'})`}>
            <ClippedPre text={asText(detail.result.output)} error={isError} />
          </Labeled>
        )}
      </div>
    );
  }
  if (detail.kind === 'aux') {
    const reason =
      detail.step.outcome.outcome === 'failed' || detail.step.outcome.outcome === 'cancelled'
        ? detail.step.outcome.reason
        : null;
    return (
      <div className="space-y-1 text-[0.8rem] text-ink-soft">
        <div>
          {detail.spans.length} span{detail.spans.length === 1 ? '' : 's'}
        </div>
        {reason && <ClippedPre text={reason} error />}
      </div>
    );
  }
  // llm
  const r = detail.result;
  return (
    <div className="space-y-2">
      {r?.thinking?.trim() && (
        <details>
          <summary className="cursor-pointer text-[0.6rem] uppercase tracking-wider font-bold text-ink-soft">
            thinking
          </summary>
          <pre className="whitespace-pre-wrap break-words font-mono text-[0.78rem] text-ink-soft bg-gray-50 border-2 border-black rounded-md p-2 italic mt-1">
            {r.thinking}
          </pre>
        </details>
      )}
      {r?.output_content?.trim() && (
        <Labeled label="output">
          <ClippedPre text={r.output_content} />
        </Labeled>
      )}
      {r?.tool_calls && r.tool_calls.length > 0 && (
        <Labeled label="tool calls">
          <div className="font-mono text-[0.78rem] text-ink">
            {r.tool_calls.map((t) => t.name).join(', ')}
          </div>
        </Labeled>
      )}
      <details>
        <summary className="cursor-pointer text-[0.6rem] uppercase tracking-wider font-bold text-ink-soft">
          input messages
        </summary>
        <div className="mt-1">
          <MessageList
            messages={resolveInputMessages(detail.begin.input_messages, sessionMessages, detail.startedAt)}
            foldHistory
          />
        </div>
      </details>
    </div>
  );
}

// ── the rail ─────────────────────────────────────────────────────────

export function FlowRail({ trace }: { trace: BenchTrace }) {
  const steps = useMemo(() => buildFlowSteps(trace), [trace]);
  const [filter, setFilter] = useState<Filter>('all');
  const [open, setOpen] = useState<Set<number>>(() => new Set(steps.filter((s) => s.isError).map((s) => s.ord)));

  const maxDur = steps.reduce((m, s) => Math.max(m, s.durMs ?? 0), 0);
  const totIn = steps.reduce((a, s) => a + (s.inTok ?? 0), 0);
  const totOut = steps.reduce((a, s) => a + (s.outTok ?? 0), 0);
  const totCached = steps.reduce((a, s) => a + (s.cachedTok ?? 0), 0);
  const failures = steps.filter((s) => s.isError);
  const counts = {
    user: steps.filter((s) => s.role === 'user' || s.role === 'system').length,
    llm: steps.filter((s) => s.role === 'llm').length,
    tool: steps.filter((s) => s.role === 'tool').length,
    aux: steps.filter((s) => s.role === 'aux').length,
  };

  if (steps.length === 0) {
    return <div className="text-ink-soft text-[0.85rem] italic">No conversation or execution recorded.</div>;
  }

  const shown = steps.filter((s) => {
    if (filter === 'failures') return s.isError;
    if (filter === 'tools') return s.role === 'tool';
    if (filter === 'llm') return s.role === 'llm';
    return true;
  });

  const toggle = (ord: number) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(ord)) next.delete(ord);
      else next.add(ord);
      return next;
    });

  const jumpTo = (ord: number) => {
    setOpen((prev) => new Set(prev).add(ord));
    document.getElementById(`flowstep-${ord}`)?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  };

  const inPct = 100;
  const outPct = totIn > 0 ? Math.round((totOut / totIn) * 100) : 0;

  return (
    <div>
      {/* ── glance header ── */}
      <div className="space-y-2 mb-3">
        <div className="flex items-start gap-2 text-[0.7rem]">
          <span className="font-bold uppercase tracking-wider text-ink-soft w-16 shrink-0 pt-0.5">sequence</span>
          <div className="flex flex-wrap gap-0.5">
            {steps.map((s) => (
              <button
                key={s.ord}
                type="button"
                title={`#${s.ord} ${s.role}: ${s.label}`}
                onClick={() => jumpTo(s.ord)}
                className={`h-4 w-3 rounded-sm border border-black/40 cursor-pointer ${
                  s.isError ? 'bg-err' : ROLE[s.role].cell
                }`}
              />
            ))}
          </div>
          {failures.length > 0 && (
            <span className="ml-auto shrink-0 font-bold text-err">
              {failures.length} fail @ #{failures.map((f) => f.ord).join(', #')}
            </span>
          )}
        </div>
        {totIn > 0 && (
          <div className="flex items-center gap-2 text-[0.7rem] font-mono">
            <span className="font-bold uppercase tracking-wider text-ink-soft w-16 shrink-0">tokens</span>
            <span className="text-ink-soft">in</span>
            <div className="h-2 w-40 border border-black/30 rounded-sm overflow-hidden">
              <div className="h-full bg-brand" style={{ width: `${inPct}%` }} />
            </div>
            <span>{fmtTokens(totIn)}</span>
            <span className="text-ink-soft ml-2">out</span>
            <div className="h-2 w-40 border border-black/30 rounded-sm overflow-hidden">
              <div className="h-full bg-ok" style={{ width: `${outPct}%` }} />
            </div>
            <span>{fmtTokens(totOut)}</span>
            {totCached > 0 && <span className="text-ink-soft ml-2">cache {cacheRatePct(totCached, totIn)}</span>}
          </div>
        )}
        {/* ── filter bar ── */}
        <div className="flex items-center gap-2">
          {(
            [
              ['all', `all`],
              ['failures', `failures(${failures.length})`],
              ['tools', `tools`],
              ['llm', `llm`],
            ] as [Filter, string][]
          ).map(([f, lbl]) => (
            <button
              key={f}
              type="button"
              onClick={() => setFilter(f)}
              className={`px-2 py-0.5 border-2 border-black rounded text-[0.65rem] font-bold uppercase tracking-wider cursor-pointer ${
                filter === f ? 'bg-brand text-white' : 'bg-white text-ink hover:bg-gray-50'
              }`}
            >
              {lbl}
            </button>
          ))}
          <span className="ml-auto text-[0.7rem] text-ink-soft">{steps.length} steps</span>
        </div>
        <Legend />
      </div>

      {/* ── rail rows ── */}
      <div className="border-2 border-black rounded-md divide-y divide-black/10">
        {shown.map((s) => {
          const isOpen = open.has(s.ord);
          return (
            <div
              key={s.ord}
              id={`flowstep-${s.ord}`}
              className={s.isError ? 'border-l-4 border-err bg-err/5' : ''}
            >
              <button
                type="button"
                onClick={() => toggle(s.ord)}
                className="w-full flex items-center gap-2 px-2 py-1.5 text-left hover:bg-gray-50 cursor-pointer"
              >
                <span className="w-6 text-right font-mono text-[0.7rem] text-ink-soft shrink-0">{s.ord}</span>
                {isOpen ? (
                  <RiArrowDownSLine className="text-ink-soft shrink-0" />
                ) : (
                  <RiArrowRightSLine className="text-ink-soft shrink-0" />
                )}
                <span className={`w-4 text-center font-mono text-[0.8rem] font-bold shrink-0 ${ROLE[s.role].glyphColor}`}>
                  {ROLE[s.role].glyph}
                </span>
                <span className="flex-1 min-w-0 truncate font-mono text-[0.8rem]">{s.label}</span>
                <DurationBar ms={s.durMs} max={maxDur} role={s.role} isError={s.isError} />
                <span className="w-12 text-right font-mono text-[0.7rem] text-ink-soft shrink-0">
                  {s.durMs != null ? fmtMs(s.durMs) : ''}
                </span>
                <span className="w-12 text-right shrink-0">
                  <OutcomeWord outcome={s.outcome} />
                </span>
                <span className="w-24 text-right font-mono text-[0.65rem] text-ink-soft shrink-0">
                  {s.inTok != null ? `↑${fmtTokens(s.inTok)} ↓${fmtTokens(s.outTok ?? 0)}` : ''}
                </span>
              </button>
              {isOpen && (
                <div className="px-3 pb-3 pt-1 pl-10">
                  <StepDetail detail={s.detail} sessionMessages={trace.session_messages} />
                </div>
              )}
            </div>
          );
        })}
      </div>

      {/* ── aggregate footer ── */}
      <div className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-[0.72rem] font-mono text-ink-soft border-t-2 border-black pt-2">
        <span>{counts.user} user</span>
        <span>{counts.llm} llm</span>
        <span>
          {counts.tool} tool
          {failures.filter((f) => f.role === 'tool').length > 0 && (
            <span className="text-err"> ({failures.filter((f) => f.role === 'tool').length}✗)</span>
          )}
        </span>
        <span>{counts.aux} aux</span>
        <span className="ml-auto">
          {fmtTokens(totIn)}↑ {fmtTokens(totOut)}↓ tok
          {totCached > 0 && ` · cache ${cacheRatePct(totCached, totIn)}`}
        </span>
      </div>
    </div>
  );
}
