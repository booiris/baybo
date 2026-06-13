import { Link, useParams } from 'react-router-dom';
import { RiFileList3Line } from 'react-icons/ri';
import { api } from '../api/client';
import { useAsync } from '../lib/useAsync';
import { fmtCost, fmtMs, fmtTime, fmtTokensCell } from '../lib/format';
import { Card, PassRateBar, StatusPill, Spinner, ErrorBox, Empty } from '../components/ui';
import type { BenchExtra } from '../generated/BenchExtra';

function extraQuick(extra: BenchExtra): string {
  switch (extra.type) {
    case 'swe':
      return extra.repo;
    case 'tb':
      return extra.failure_mode && extra.failure_mode !== 'unset' ? extra.failure_mode : '';
    case 'memory':
      return extra.category;
  }
}

export function RunPage() {
  const { benchId = '', runKey = '' } = useParams();
  const { data, error, loading } = useAsync(() => api.run(benchId, runKey), [benchId, runKey]);

  if (loading) return <Spinner />;
  if (error) return <ErrorBox message={error} />;
  if (!data) return <Empty message="Run not found." />;

  const s = data.summary;

  return (
    <div className="space-y-5">
      <div>
        <Link
          to={`/bench/${encodeURIComponent(benchId)}`}
          className="text-[0.8rem] text-brand font-bold hover:underline"
        >
          ← {data.summary.bench}
        </Link>
        <h1 className="text-lg font-bold font-mono mt-1 flex items-center gap-3">
          {s.run_id}
          {s.is_merged && (
            <span className="text-[0.6rem] uppercase tracking-wider font-bold border-2 border-black rounded px-1 text-ink-soft">
              merged
            </span>
          )}
        </h1>
        <div className="text-[0.8rem] text-ink-soft mt-1">
          {s.arm && <>arm {s.arm} · </>}
          {s.model && <>model {s.model} · </>}
          {fmtTime(s.started_at)}
        </div>
        <div className="mt-3">
          <PassRateBar rate={s.pass_rate} passed={s.n_passed} total={s.n_total} />
        </div>
      </div>

      {data.items.length === 0 ? (
        <Empty message="No items in this run." />
      ) : (
        <Card className="overflow-x-auto">
          <table className="w-full text-[0.85rem]">
            <thead>
              <tr>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  item
                </th>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  status
                </th>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  detail
                </th>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  latency
                </th>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  cost
                </th>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  tokens
                </th>
                <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
                  trace
                </th>
              </tr>
            </thead>
            <tbody>
              {data.items.map((it) => (
                <tr key={it.id} className="border-t-2 border-black/15 hover:bg-gray-50">
                  <td className="px-3 py-2">
                    <Link
                      to={`/bench/${encodeURIComponent(benchId)}/run/${encodeURIComponent(
                        runKey,
                      )}/item/${encodeURIComponent(it.id)}`}
                      className="text-brand font-bold hover:underline font-mono break-all"
                    >
                      {it.id}
                    </Link>
                    {it.source_run && (
                      <span className="ml-2 text-[0.65rem] text-ink-soft">({it.source_run})</span>
                    )}
                  </td>
                  <td className="px-3 py-2">
                    <StatusPill passed={it.passed} />
                  </td>
                  <td className="px-3 py-2 text-ink-soft font-mono text-[0.8rem]">
                    {extraQuick(it.extra) || '—'}
                  </td>
                  <td className="px-3 py-2 whitespace-nowrap">{fmtMs(it.latency_ms)}</td>
                  <td className="px-3 py-2 whitespace-nowrap">{fmtCost(it.cost_micro_usd)}</td>
                  <td className="px-3 py-2 whitespace-nowrap">
                    {fmtTokensCell(it.input_tokens, it.output_tokens, it.cached_input_tokens)}
                  </td>
                  <td className="px-3 py-2">
                    {it.trace ? <RiFileList3Line className="text-ok" title="trace available" /> : '—'}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </Card>
      )}
    </div>
  );
}
