import { useState } from 'react';
import {
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiBrainLine,
  RiRobot2Line,
  RiToolsLine,
} from 'react-icons/ri';
import type {
  LifecycleState,
  LlmCallBegin,
  LlmCallResult,
  ReplayStep,
  SessionMessageRow,
  Span,
} from '../../types/trace';
import { resolveInputMessages } from '../../types/trace';
import type { BenchTurn, BenchTrace } from '../../api/types';
import { fmtMs, fmtTokens } from '../../lib/format';
import { MessageList } from './MessageList';

function durMs(start?: string | null, end?: string | null): number | null {
  if (!start || !end) return null;
  const s = new Date(start).getTime();
  const e = new Date(end).getTime();
  if (Number.isNaN(s) || Number.isNaN(e)) return null;
  return e >= s ? e - s : null;
}

function OutcomeBadge({ outcome }: { outcome: LifecycleState }) {
  const map: Record<string, string> = {
    ok: 'bg-ok/15 text-ok',
    failed: 'bg-err/15 text-err',
    cancelled: 'bg-warn/15 text-warn',
    pending: 'bg-gray-100 text-ink-soft',
  };
  const cls = map[outcome.outcome] ?? 'bg-gray-100 text-ink-soft';
  // Just the status word — the reason can be a long error string, so it
  // rides as a hover title here and wraps in <OutcomeReason/> below the
  // header rather than overflowing this badge.
  const reason =
    outcome.outcome === 'failed' || outcome.outcome === 'cancelled'
      ? outcome.reason
      : undefined;
  return (
    <span
      title={reason}
      className={`shrink-0 whitespace-nowrap px-1.5 py-0.5 rounded border-2 border-black text-[0.6rem] font-bold uppercase tracking-wider ${cls}`}
    >
      {outcome.outcome}
    </span>
  );
}

/** The failure / cancellation reason as a wrapping block (empty for ok/pending). */
function OutcomeReason({ outcome }: { outcome: LifecycleState }) {
  if (outcome.outcome !== 'failed' && outcome.outcome !== 'cancelled') return null;
  const color = outcome.outcome === 'failed' ? 'text-err' : 'text-warn';
  return (
    <pre
      className={`whitespace-pre-wrap break-words font-mono text-[0.7rem] ${color} bg-gray-50 border-2 border-black rounded-md p-2`}
    >
      {outcome.reason}
    </pre>
  );
}

function TokenChips({
  inTok,
  outTok,
}: {
  inTok?: number;
  outTok?: number;
}) {
  if (inTok == null && outTok == null) return null;
  return (
    <span className="font-mono text-[0.7rem] text-ink-soft whitespace-nowrap">
      ↑{fmtTokens(inTok ?? 0)} ↓{fmtTokens(outTok ?? 0)}
    </span>
  );
}

function ClippedPre({ text }: { text: string }) {
  const LIMIT = 4_000;
  const [open, setOpen] = useState(false);
  const shown = open || text.length <= LIMIT ? text : `${text.slice(0, LIMIT)}\n…`;
  return (
    <div>
      <pre className="whitespace-pre-wrap break-words font-mono text-[0.8rem] text-ink bg-gray-50 border-2 border-black rounded-md p-2">
        {shown}
      </pre>
      {text.length > LIMIT && (
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="mt-1 text-[0.7rem] uppercase tracking-wider font-bold text-brand hover:underline cursor-pointer"
        >
          {open ? 'Collapse' : `Show full (${text.length.toLocaleString()} chars)`}
        </button>
      )}
    </div>
  );
}

function asText(value: unknown): string {
  if (typeof value === 'string') return value;
  return JSON.stringify(value, null, 2);
}

function SpanRow({
  span,
  sessionMessages,
}: {
  span: Span;
  sessionMessages: SessionMessageRow[];
}) {
  const [open, setOpen] = useState(false);
  const dur = durMs(span.started_at, span.ended_at);

  let icon = <RiToolsLine className="text-warn" />;
  let title = '';
  let chips: React.ReactNode = null;

  if (span.kind.kind === 'llm_call') {
    icon = <RiRobot2Line className="text-ok" />;
    title = span.kind.begin.model_id;
    chips = (
      <TokenChips
        inTok={span.kind.result?.input_tokens}
        outTok={span.kind.result?.output_tokens}
      />
    );
  } else {
    icon = <RiToolsLine className="text-warn" />;
    title = span.kind.begin.tool_name;
  }

  return (
    <div className="border-2 border-black rounded-md bg-white">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="w-full flex items-center gap-2 px-2 py-1.5 text-left hover:bg-gray-50 cursor-pointer"
      >
        {open ? <RiArrowDownSLine className="text-ink-soft shrink-0" /> : <RiArrowRightSLine className="text-ink-soft shrink-0" />}
        {icon}
        <span className="font-mono text-[0.8rem] font-bold truncate min-w-0">{title}</span>
        <span className="ml-auto flex items-center gap-2 shrink-0">
          {chips}
          <span className="font-mono text-[0.7rem] text-ink-soft">{fmtMs(dur)}</span>
          <OutcomeBadge outcome={span.outcome} />
        </span>
      </button>
      {open && (
        <div className="px-2 pb-2 pt-1 space-y-2 border-t border-black/15">
          <OutcomeReason outcome={span.outcome} />
          {span.kind.kind === 'llm_call' && (
            <LlmCallBody
              begin={span.kind.begin}
              result={span.kind.result}
              spanStartedAt={span.started_at}
              sessionMessages={sessionMessages}
            />
          )}
          {span.kind.kind === 'tool_call' && (
            <>
              <Labeled label="params">
                <ClippedPre text={asText(span.kind.begin.params)} />
              </Labeled>
              {span.kind.result && (
                <Labeled label={`result (${span.kind.result.success ? 'ok' : 'error'})`}>
                  <ClippedPre text={asText(span.kind.result.output)} />
                </Labeled>
              )}
            </>
          )}
        </div>
      )}
    </div>
  );
}

function LlmCallBody({
  begin,
  result,
  spanStartedAt,
  sessionMessages,
}: {
  begin: LlmCallBegin;
  result?: LlmCallResult | null;
  spanStartedAt: string;
  sessionMessages: SessionMessageRow[];
}) {
  const inputs = resolveInputMessages(begin.input_messages, sessionMessages, spanStartedAt);
  return (
    <div className="space-y-2">
      <Labeled label="input">
        <MessageList messages={inputs} foldHistory />
      </Labeled>
      {result?.thinking && (
        <Labeled label="thinking">
          <pre className="whitespace-pre-wrap break-words font-mono text-[0.8rem] text-ink-soft bg-gray-50 border-2 border-black rounded-md p-2 italic">
            {result.thinking}
          </pre>
        </Labeled>
      )}
      {result?.output_content && (
        <Labeled label="output">
          <ClippedPre text={result.output_content} />
        </Labeled>
      )}
      {result?.tool_calls && result.tool_calls.length > 0 && (
        <Labeled label="tool calls">
          <div className="space-y-1">
            {result.tool_calls.map((tc) => (
              <pre
                key={tc.id}
                className="whitespace-pre-wrap break-all font-mono text-[0.8rem] text-ink bg-gray-50 border-2 border-black rounded-md p-2"
              >
                {tc.name}({asText(tc.arguments)})
              </pre>
            ))}
          </div>
        </Labeled>
      )}
    </div>
  );
}

function Labeled({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold mb-1">
        {label}
      </div>
      {children}
    </div>
  );
}

function StepBlock({
  step,
  spans,
  sessionMessages,
}: ReplayStep & { sessionMessages: SessionMessageRow[] }) {
  const kind = step.kind.kind;
  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-2">
        <RiBrainLine className="text-ink-soft" />
        <span className="text-[0.7rem] uppercase tracking-wider font-bold text-ink-soft">
          {kind}
        </span>
        <OutcomeBadge outcome={step.outcome} />
      </div>
      <OutcomeReason outcome={step.outcome} />
      <div className="space-y-1.5 pl-4 border-l-2 border-black/15">
        {spans.map((s) => (
          <SpanRow key={s.id} span={s} sessionMessages={sessionMessages} />
        ))}
      </div>
    </div>
  );
}

function TurnBlock({
  turn,
  sessionMessages,
}: {
  turn: BenchTurn;
  sessionMessages: SessionMessageRow[];
}) {
  const dur = durMs(turn.turn.started_at, turn.turn.ended_at);
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2 text-[0.75rem]">
        <span className="font-bold uppercase tracking-wider text-ink-soft">turn</span>
        <span className="font-mono text-ink-soft truncate">{turn.turn.id}</span>
        <span className="ml-auto font-mono text-ink-soft">{fmtMs(dur)}</span>
      </div>
      <div className="space-y-3">
        {turn.steps.map((rs) => (
          <StepBlock
            key={rs.step.id}
            step={rs.step}
            spans={rs.spans}
            sessionMessages={sessionMessages}
          />
        ))}
      </div>
    </div>
  );
}

export function Timeline({ trace }: { trace: BenchTrace }) {
  if (trace.turns.length === 0) {
    return <div className="text-ink-soft text-[0.85rem] italic">No execution spans recorded.</div>;
  }
  return (
    <div className="space-y-5">
      {trace.turns.map((t, i) => (
        <TurnBlock key={t.turn.id || i} turn={t} sessionMessages={trace.session_messages} />
      ))}
    </div>
  );
}
