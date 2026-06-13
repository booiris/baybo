import { Link, useSearchParams } from 'react-router-dom';
import { api } from '../api/client';
import { useAsync } from '../lib/useAsync';
import { Card, StatusPill, Spinner, ErrorBox, Empty } from '../components/ui';

export function SearchPage() {
  const [params] = useSearchParams();
  const q = params.get('q') ?? '';
  const { data, error, loading } = useAsync(() => api.search(q), [q]);

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-bold uppercase tracking-wider">
        Search <span className="text-ink-soft font-mono normal-case">“{q}”</span>
      </h1>
      {loading && <Spinner />}
      {error && <ErrorBox message={error} />}
      {data && data.length === 0 && <Empty message="No matching items." />}
      {data && data.length > 0 && (
        <>
          <div className="text-[0.8rem] text-ink-soft">{data.length} matches</div>
          <Card className="overflow-x-auto">
            <table className="w-full text-[0.85rem]">
              <thead>
                <tr>
                  {['bench', 'run', 'arm', 'item', 'detail', 'status'].map((h) => (
                    <th
                      key={h}
                      className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft"
                    >
                      {h}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {data.map((hit, i) => (
                  <tr key={`${hit.bench}-${hit.run_key}-${hit.item_id}-${i}`} className="border-t-2 border-black/15 hover:bg-gray-50">
                    <td className="px-3 py-2 font-mono text-[0.8rem]">{hit.bench}</td>
                    <td className="px-3 py-2 font-mono text-[0.8rem] text-ink-soft">{hit.run_id}</td>
                    <td className="px-3 py-2">{hit.arm || '—'}</td>
                    <td className="px-3 py-2">
                      <Link
                        to={`/bench/${encodeURIComponent(hit.bench)}/run/${encodeURIComponent(
                          hit.run_key,
                        )}/item/${encodeURIComponent(hit.item_id)}`}
                        className="text-brand font-bold hover:underline font-mono break-all"
                      >
                        {hit.item_id}
                      </Link>
                    </td>
                    <td className="px-3 py-2 text-ink-soft font-mono text-[0.8rem]">
                      {hit.detail || '—'}
                    </td>
                    <td className="px-3 py-2">
                      <StatusPill passed={hit.passed} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        </>
      )}
    </div>
  );
}
