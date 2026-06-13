import { useMemo, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { RiArrowDownSLine, RiArrowUpSLine, RiFileList3Line } from 'react-icons/ri';
import { api } from '../api/client';
import { useAsync } from '../lib/useAsync';
import { fmtCost, fmtMs, fmtTime, fmtTokensCell } from '../lib/format';
import { Card, PassRateBar, StatusPill, Spinner, ErrorBox, Empty } from '../components/ui';
import type { BenchExtra } from '../generated/BenchExtra';
import type { Item } from '../generated/Item';

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

type SortKey = 'item' | 'status' | 'latency' | 'cost' | 'tokens';
type SortDir = 'asc' | 'desc';

// First click on a column lands on its most useful direction: metrics
// largest-first, status failures-first (false < true), id A→Z. A second
// click on the same column flips it.
const DEFAULT_DIR: Record<SortKey, SortDir> = {
  item: 'asc',
  status: 'asc',
  latency: 'desc',
  cost: 'desc',
  tokens: 'desc',
};

/** A comparable scalar per column. Missing numeric metrics sort to the
 * bottom (largest-first) via a -1 sentinel rather than mixing with zeros. */
function sortValue(it: Item, key: SortKey): number | string {
  switch (key) {
    case 'item':
      return it.id;
    case 'status':
      return it.passed ? 1 : 0;
    case 'latency':
      return it.latency_ms ?? -1;
    case 'cost':
      return it.cost_micro_usd ?? -1;
    case 'tokens':
      return (it.input_tokens ?? 0) + (it.output_tokens ?? 0);
  }
}

function SortHeader({
  label,
  col,
  sortKey,
  sortDir,
  onSort,
}: {
  label: string;
  col: SortKey;
  sortKey: SortKey | null;
  sortDir: SortDir;
  onSort: (k: SortKey) => void;
}) {
  const active = sortKey === col;
  return (
    <th className="text-left border-b-2 border-black px-3 py-2">
      <button
        type="button"
        onClick={() => onSort(col)}
        className={`flex items-center gap-1 font-bold uppercase tracking-wider text-[0.7rem] cursor-pointer ${
          active ? 'text-ink' : 'text-ink-soft hover:text-ink'
        }`}
      >
        {label}
        {active ? (
          sortDir === 'asc' ? (
            <RiArrowUpSLine />
          ) : (
            <RiArrowDownSLine />
          )
        ) : (
          <RiArrowDownSLine className="opacity-25" />
        )}
      </button>
    </th>
  );
}

function PlainHeader({ label }: { label: string }) {
  return (
    <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft">
      {label}
    </th>
  );
}

export function RunPage() {
  const { benchId = '', runKey = '' } = useParams();
  const { data, error, loading } = useAsync(() => api.run(benchId, runKey), [benchId, runKey]);

  const [sortKey, setSortKey] = useState<SortKey | null>(null);
  const [sortDir, setSortDir] = useState<SortDir>('desc');

  const onSort = (key: SortKey) => {
    if (sortKey === key) {
      setSortDir((d) => (d === 'asc' ? 'desc' : 'asc'));
    } else {
      setSortKey(key);
      setSortDir(DEFAULT_DIR[key]);
    }
  };

  const items = useMemo(() => {
    const base = data?.items ?? [];
    if (!sortKey) return base;
    const arr = [...base];
    arr.sort((a, b) => {
      const av = sortValue(a, sortKey);
      const bv = sortValue(b, sortKey);
      const cmp =
        typeof av === 'string' && typeof bv === 'string'
          ? av.localeCompare(bv)
          : (av as number) - (bv as number);
      return sortDir === 'asc' ? cmp : -cmp;
    });
    return arr;
  }, [data?.items, sortKey, sortDir]);

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

      {items.length === 0 ? (
        <Empty message="No items in this run." />
      ) : (
        <Card className="overflow-x-auto">
          <table className="w-full text-[0.85rem]">
            <thead>
              <tr>
                <SortHeader label="item" col="item" sortKey={sortKey} sortDir={sortDir} onSort={onSort} />
                <SortHeader label="status" col="status" sortKey={sortKey} sortDir={sortDir} onSort={onSort} />
                <PlainHeader label="detail" />
                <SortHeader label="latency" col="latency" sortKey={sortKey} sortDir={sortDir} onSort={onSort} />
                <SortHeader label="cost" col="cost" sortKey={sortKey} sortDir={sortDir} onSort={onSort} />
                <SortHeader label="tokens" col="tokens" sortKey={sortKey} sortDir={sortDir} onSort={onSort} />
                <PlainHeader label="trace" />
              </tr>
            </thead>
            <tbody>
              {items.map((it) => (
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
