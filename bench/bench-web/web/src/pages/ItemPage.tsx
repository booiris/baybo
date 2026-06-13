import { useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { api } from '../api/client';
import { useAsync, type AsyncState } from '../lib/useAsync';
import type { BenchTrace } from '../api/types';
import { cacheRatePct, fmtCost, fmtMs, fmtTokensCell } from '../lib/format';
import { Card, StatusPill, StatChip, Spinner, ErrorBox, Empty } from '../components/ui';
import { MessageList } from '../components/trace/MessageList';
import { Timeline } from '../components/trace/Timeline';
import { FlowRail } from '../components/trace/FlowRail';
import type { Item } from '../generated/Item';
import type { BenchExtra } from '../generated/BenchExtra';
import type { ArtifactRef } from '../generated/ArtifactRef';
import type { ChatMessage } from '../types/trace';

export function ItemPage() {
  const { benchId = '', runKey = '', itemId = '' } = useParams();
  const run = useAsync(() => api.run(benchId, runKey), [benchId, runKey]);
  const item = run.data?.items.find((i) => i.id === itemId);

  const tracePath = item?.trace?.trace;
  const messagesPath = item?.trace?.messages ?? undefined;
  const trace = useAsync(
    () => (tracePath ? api.trace(benchId, tracePath, messagesPath) : Promise.resolve(null)),
    [benchId, tracePath, messagesPath],
  );

  if (run.loading) return <Spinner />;
  if (run.error) return <ErrorBox message={run.error} />;
  if (!item) return <Empty message="Item not found in this run." />;

  const messages: ChatMessage[] =
    trace.data?.session_messages.map((row) => row.message) ?? [];

  return (
    <div className="space-y-5">
      <div>
        <Link
          to={`/bench/${encodeURIComponent(benchId)}/run/${encodeURIComponent(runKey)}`}
          className="text-[0.8rem] text-brand font-bold hover:underline"
        >
          ← {runKey}
        </Link>
        <h1 className="text-lg font-bold font-mono mt-1 flex items-center gap-3 break-all">
          {item.id}
          <StatusPill passed={item.passed} />
        </h1>
      </div>

      <div className="flex flex-wrap gap-3">
        <StatChip label="latency" value={fmtMs(item.latency_ms)} />
        <StatChip label="cost" value={fmtCost(item.cost_micro_usd)} />
        <StatChip
          label="tokens (in/out · cached)"
          value={fmtTokensCell(item.input_tokens, item.output_tokens, item.cached_input_tokens)}
        />
        {item.cached_input_tokens != null && (
          <StatChip
            label="input cache rate"
            value={cacheRatePct(item.cached_input_tokens, item.input_tokens)}
          />
        )}
        {item.source_run && <StatChip label="source run" value={item.source_run} />}
      </div>

      <ExtraPanel benchId={benchId} extra={item.extra} />

      <TraceSection item={item} trace={trace} messages={messages} />
    </div>
  );
}

/**
 * The unified conversation+execution view (one chronological timeline),
 * with a collapsible "raw detail" fallback exposing the linear transcript
 * and the raw step/span tree for deep debugging.
 */
function TraceSection({
  item,
  trace,
  messages,
}: {
  item: Item;
  trace: AsyncState<BenchTrace | null>;
  messages: ChatMessage[];
}) {
  if (!item.trace) return <Empty message="No agent trace recorded for this item." />;
  if (trace.loading) return <Spinner label="loading trace…" />;
  if (trace.error) return <ErrorBox message={trace.error} />;
  if (!trace.data) return <Empty message="Trace unavailable." />;
  return (
    <div className="space-y-3">
      <Card className="p-4">
        <FlowRail trace={trace.data} />
      </Card>
      <details className="border-[3px] border-black rounded-md bg-white shadow-brutal-sm">
        <summary className="cursor-pointer px-4 py-2 text-[0.75rem] font-bold uppercase tracking-wider text-ink-soft">
          raw detail (transcript · span tree)
        </summary>
        <div className="px-4 pb-4 space-y-5">
          <section>
            <h3 className="text-[0.7rem] uppercase tracking-wider font-bold text-ink-soft mb-2">
              transcript
            </h3>
            <MessageList messages={messages} foldHistory={false} />
          </section>
          <section>
            <h3 className="text-[0.7rem] uppercase tracking-wider font-bold text-ink-soft mb-2">
              execution spans
            </h3>
            <Timeline trace={trace.data} />
          </section>
        </div>
      </details>
    </div>
  );
}

function ExtraPanel({ benchId, extra }: { benchId: string; extra: BenchExtra }) {
  if (extra.type === 'swe') {
    return (
      <Card className="p-4 space-y-3">
        <PanelTitle>SWE instance</PanelTitle>
        <div className="flex flex-wrap gap-3">
          <StatChip label="repo" value={extra.repo} />
          <StatChip label="patch bytes" value={extra.patch_bytes.toLocaleString()} />
          {extra.empty_patch && <StatChip label="patch" value="empty" />}
          {extra.errored && <StatChip label="errored" value="yes" />}
        </div>
        {extra.error && (
          <pre className="whitespace-pre-wrap break-words font-mono text-[0.8rem] text-err bg-err/10 border-2 border-black rounded-md p-2">
            {extra.error}
          </pre>
        )}
        <Artifacts benchId={benchId} artifacts={extra.artifacts} />
      </Card>
    );
  }
  if (extra.type === 'tb') {
    return (
      <Card className="p-4 space-y-3">
        <PanelTitle>Terminal-bench task</PanelTitle>
        {extra.failure_mode && extra.failure_mode !== 'unset' && (
          <StatChip label="failure mode" value={extra.failure_mode} />
        )}
        {extra.parser_results.length > 0 && (
          <div>
            <div className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold mb-1">
              tests
            </div>
            <div className="space-y-1">
              {extra.parser_results.map((p) => (
                <div key={p.name} className="flex items-center gap-2 text-[0.8rem] font-mono">
                  <span
                    className={`px-1.5 py-0.5 rounded border-2 border-black text-[0.6rem] font-bold uppercase ${
                      p.status === 'passed' ? 'bg-ok/15 text-ok' : 'bg-err/15 text-err'
                    }`}
                  >
                    {p.status}
                  </span>
                  <span className="break-all">{p.name}</span>
                </div>
              ))}
            </div>
          </div>
        )}
        {extra.instruction && (
          <details>
            <summary className="cursor-pointer text-[0.7rem] uppercase tracking-wider font-bold text-ink-soft">
              instruction
            </summary>
            <pre className="whitespace-pre-wrap break-words font-mono text-[0.8rem] text-ink bg-gray-50 border-2 border-black rounded-md p-2 mt-1">
              {extra.instruction}
            </pre>
          </details>
        )}
        <Artifacts benchId={benchId} artifacts={extra.artifacts} />
      </Card>
    );
  }
  // memory
  return (
    <Card className="p-4 space-y-3">
      <PanelTitle>Memory question</PanelTitle>
      <div className="flex flex-wrap gap-3">
        <StatChip label="category" value={extra.category} />
        <StatChip label="f1" value={extra.f1.toFixed(3)} />
      </div>
      <Field label="question" value={extra.question} />
      <Field label="gold answer" value={extra.gold} />
      <Field label="agent answer" value={extra.answer} />
      <Field label="judge reason" value={extra.judge_reason} />
    </Card>
  );
}

function Field({ label, value }: { label: string; value: string }) {
  if (!value) return null;
  return (
    <div>
      <div className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold mb-1">
        {label}
      </div>
      <pre className="whitespace-pre-wrap break-words font-mono text-[0.85rem] text-ink bg-gray-50 border-2 border-black rounded-md p-2">
        {value}
      </pre>
    </div>
  );
}

function PanelTitle({ children }: { children: React.ReactNode }) {
  return <div className="text-[0.8rem] uppercase tracking-wider font-bold">{children}</div>;
}

function Artifacts({ benchId, artifacts }: { benchId: string; artifacts: ArtifactRef[] }) {
  if (artifacts.length === 0) return null;
  return (
    <div className="space-y-2">
      <div className="text-[0.6rem] uppercase tracking-wider text-ink-soft font-bold">artifacts</div>
      {artifacts.map((a) => (
        <ArtifactViewer key={a.path} benchId={benchId} artifact={a} />
      ))}
    </div>
  );
}

function ArtifactViewer({ benchId, artifact }: { benchId: string; artifact: ArtifactRef }) {
  const [open, setOpen] = useState(false);
  const loaded = useAsync(
    () => (open ? api.file(benchId, artifact.path) : Promise.resolve(null)),
    [open, benchId, artifact.path],
  );
  return (
    <div className="border-2 border-black rounded-md">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="w-full flex items-center gap-2 px-2 py-1.5 text-left text-[0.75rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
      >
        {open ? '▾' : '▸'} {artifact.label}
      </button>
      {open && (
        <div className="px-2 pb-2">
          {loaded.loading && <Spinner />}
          {loaded.error && <ErrorBox message={loaded.error} />}
          {loaded.data != null && <ArtifactBody kind={artifact.kind} text={loaded.data} />}
        </div>
      )}
    </div>
  );
}

function ArtifactBody({ kind, text }: { kind: ArtifactRef['kind']; text: string }) {
  if (kind === 'diff') {
    return (
      <pre className="whitespace-pre-wrap break-words font-mono text-[0.78rem] bg-gray-50 border-2 border-black rounded-md p-2 overflow-x-auto">
        {text.split('\n').map((line, i) => {
          let color = 'text-ink';
          if (line.startsWith('+') && !line.startsWith('+++')) color = 'text-ok';
          else if (line.startsWith('-') && !line.startsWith('---')) color = 'text-err';
          else if (line.startsWith('@@')) color = 'text-info';
          else if (line.startsWith('diff ') || line.startsWith('index ')) color = 'text-ink-soft';
          return (
            <div key={i} className={color}>
              {line || ' '}
            </div>
          );
        })}
      </pre>
    );
  }
  return (
    <pre className="whitespace-pre-wrap break-words font-mono text-[0.78rem] text-ink bg-gray-50 border-2 border-black rounded-md p-2 overflow-x-auto">
      {text}
    </pre>
  );
}
