import { Link } from 'react-router-dom';
import { api } from '../api/client';
import { useAsync } from '../lib/useAsync';
import { relTime } from '../lib/format';
import { Card, PassRateBar, Spinner, ErrorBox, Empty } from '../components/ui';
import type { BenchInfo } from '../generated/BenchInfo';

export function HomePage() {
  const { data, error, loading } = useAsync(() => api.benches(), []);

  if (loading) return <Spinner label="scanning benches…" />;
  if (error) return <ErrorBox message={error} />;
  if (!data || data.length === 0) return <Empty message="No benches found under the bench root." />;

  return (
    <div>
      <h1 className="text-xl font-bold uppercase tracking-wider mb-4">Benchmarks</h1>
      <div className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-2 gap-5 auto-rows-fr">
        {data.map((b) => (
          <BenchCard key={b.id} bench={b} />
        ))}
      </div>
    </div>
  );
}

function BenchCard({ bench }: { bench: BenchInfo }) {
  return (
    <Link to={`/bench/${encodeURIComponent(bench.id)}`} className="block h-full">
      <Card className="p-5 h-full hover:-translate-y-0.5 hover:shadow-brutal transition-transform">
        <div className="flex items-baseline gap-3 mb-3">
          <span className="text-lg font-bold">{bench.label}</span>
          <span className="text-[0.6rem] uppercase tracking-wider font-bold border-2 border-black rounded px-1.5 py-0.5 text-ink-soft">
            {bench.kind}
          </span>
          <span className="ml-auto text-[0.75rem] text-ink-soft">
            {bench.run_count} run{bench.run_count === 1 ? '' : 's'} · {relTime(bench.last_run_at)}
          </span>
        </div>
        {bench.standing.length === 0 ? (
          <Empty message="No runs yet." />
        ) : (
          <div className="space-y-2">
            {bench.standing.map((s) => (
              <div key={s.arm || 'all'} className="flex items-center gap-3">
                <span className="font-mono text-[0.8rem] font-bold w-24 truncate">
                  {s.arm || 'result'}
                </span>
                <PassRateBar rate={s.pass_rate} passed={s.n_passed} total={s.n_total} />
                {s.mean_f1 != null && (
                  <span className="font-mono text-[0.75rem] text-ink-soft">
                    f1 {s.mean_f1.toFixed(2)}
                  </span>
                )}
              </div>
            ))}
          </div>
        )}
      </Card>
    </Link>
  );
}
