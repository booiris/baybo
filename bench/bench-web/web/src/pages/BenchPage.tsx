import { Link, useParams } from 'react-router-dom';
import {
  CartesianGrid,
  Legend,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import { api } from '../api/client';
import { useAsync } from '../lib/useAsync';
import { fmtCost, fmtMs, fmtTime, fmtTokensCell } from '../lib/format';
import { Card, PassRateBar, Spinner, ErrorBox, Empty } from '../components/ui';
import type { RunSummary } from '../generated/RunSummary';

const ARM_COLORS = ['#3b60e4', '#2f855a', '#dd6b20', '#e53e3e', '#3182ce', '#805ad5'];

function chartLabel(r: RunSummary): string {
  if (r.started_at) {
    const d = new Date(r.started_at);
    if (!Number.isNaN(d.getTime())) {
      return d.toLocaleString(undefined, {
        month: '2-digit',
        day: '2-digit',
        hour: '2-digit',
        minute: '2-digit',
      });
    }
  }
  return r.run_id;
}

export function BenchPage() {
  const { benchId = '' } = useParams();
  const { data, error, loading } = useAsync(() => api.bench(benchId), [benchId]);

  if (loading) return <Spinner />;
  if (error) return <ErrorBox message={error} />;
  if (!data) return <Empty message="Bench not found." />;

  const individual = data.runs.filter((r) => !r.is_merged);
  const arms = Array.from(new Set(individual.map((r) => r.arm || 'result')));
  const oldestFirst = individual.slice().reverse();
  const passPoints = oldestFirst.map((r) => {
    const p: Record<string, number | string> = { t: chartLabel(r) };
    p[r.arm || 'result'] = Number((r.pass_rate * 100).toFixed(1));
    return p;
  });
  const hasCost = individual.some((r) => r.total_cost_micro_usd != null);
  const costPoints = hasCost
    ? oldestFirst.map((r) => {
        const p: Record<string, number | string> = { t: chartLabel(r) };
        if (r.total_cost_micro_usd != null) {
          p[r.arm || 'result'] = Number((r.total_cost_micro_usd / 1_000_000).toFixed(4));
        }
        return p;
      })
    : [];

  return (
    <div className="space-y-6">
      <div>
        <Link to="/" className="text-[0.8rem] text-brand font-bold hover:underline">
          ← all benches
        </Link>
        <h1 className="text-xl font-bold uppercase tracking-wider mt-1">{data.info.label}</h1>
      </div>

      <Section title="Standing">
        {data.standing.length === 0 ? (
          <Empty message="No runs yet." />
        ) : (
          <Card className="overflow-x-auto">
            <table className="w-full text-[0.85rem]">
              <thead>
                <Tr head>
                  <Th>arm</Th>
                  <Th>source</Th>
                  <Th>pass rate</Th>
                  <Th>f1</Th>
                  <Th>cost</Th>
                  <Th>tokens (in/out)</Th>
                  <Th>mean latency</Th>
                </Tr>
              </thead>
              <tbody>
                {data.standing.map((s) => (
                  <Tr key={s.arm || 'all'}>
                    <Td bold>{s.arm || 'result'}</Td>
                    <Td muted>{s.source}</Td>
                    <Td>
                      <PassRateBar rate={s.pass_rate} passed={s.n_passed} total={s.n_total} />
                    </Td>
                    <Td>{s.mean_f1 != null ? s.mean_f1.toFixed(3) : '—'}</Td>
                    <Td>{fmtCost(s.total_cost_micro_usd)}</Td>
                    <Td>{fmtTokensCell(s.input_tokens, s.output_tokens, s.cached_input_tokens)}</Td>
                    <Td>{fmtMs(s.mean_latency_ms)}</Td>
                  </Tr>
                ))}
              </tbody>
            </table>
          </Card>
        )}
      </Section>

      {passPoints.length > 1 && (
        <Section title="Trends">
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-5">
            <Card className="p-4">
              <ChartTitle>Pass rate over runs (%)</ChartTitle>
              <ResponsiveContainer width="100%" height={220}>
                <LineChart data={passPoints} margin={{ top: 8, right: 16, bottom: 0, left: -16 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke="#0001" />
                  <XAxis dataKey="t" tick={{ fontSize: 11 }} />
                  <YAxis domain={[0, 100]} tick={{ fontSize: 11 }} />
                  <Tooltip />
                  <Legend />
                  {arms.map((arm, i) => (
                    <Line
                      key={arm}
                      type="monotone"
                      dataKey={arm}
                      stroke={ARM_COLORS[i % ARM_COLORS.length]}
                      strokeWidth={2}
                      connectNulls
                      dot={{ r: 3 }}
                    />
                  ))}
                </LineChart>
              </ResponsiveContainer>
            </Card>
            {hasCost && (
              <Card className="p-4">
                <ChartTitle>Total cost over runs ($)</ChartTitle>
                <ResponsiveContainer width="100%" height={220}>
                  <LineChart data={costPoints} margin={{ top: 8, right: 16, bottom: 0, left: -8 }}>
                    <CartesianGrid strokeDasharray="3 3" stroke="#0001" />
                    <XAxis dataKey="t" tick={{ fontSize: 11 }} />
                    <YAxis tick={{ fontSize: 11 }} />
                    <Tooltip />
                    <Legend />
                    {arms.map((arm, i) => (
                      <Line
                        key={arm}
                        type="monotone"
                        dataKey={arm}
                        stroke={ARM_COLORS[i % ARM_COLORS.length]}
                        strokeWidth={2}
                        connectNulls
                        dot={{ r: 3 }}
                      />
                    ))}
                  </LineChart>
                </ResponsiveContainer>
              </Card>
            )}
          </div>
        </Section>
      )}

      <Section title="Run history">
        {data.runs.length === 0 ? (
          <Empty message="No runs yet." />
        ) : (
          <Card className="overflow-x-auto">
            <table className="w-full text-[0.85rem]">
              <thead>
                <Tr head>
                  <Th>time</Th>
                  <Th>run</Th>
                  <Th>arm</Th>
                  <Th>model</Th>
                  <Th>pass rate</Th>
                  <Th>duration</Th>
                  <Th>cost</Th>
                  <Th>tokens</Th>
                </Tr>
              </thead>
              <tbody>
                {data.runs.map((r) => (
                  <tr
                    key={r.run_key}
                    className="border-t-2 border-black/15 hover:bg-gray-50"
                  >
                    <Td muted>{fmtTime(r.started_at)}</Td>
                    <Td>
                      <Link
                        to={`/bench/${encodeURIComponent(benchId)}/run/${encodeURIComponent(r.run_key)}`}
                        className="text-brand font-bold hover:underline font-mono"
                      >
                        {r.run_id}
                      </Link>
                      {r.is_merged && (
                        <span className="ml-2 text-[0.6rem] uppercase tracking-wider font-bold border-2 border-black rounded px-1 text-ink-soft">
                          merged
                        </span>
                      )}
                    </Td>
                    <Td>{r.arm || '—'}</Td>
                    <Td muted>{r.model ?? '—'}</Td>
                    <Td>
                      <PassRateBar rate={r.pass_rate} passed={r.n_passed} total={r.n_total} />
                    </Td>
                    <Td>{r.duration_ms != null ? fmtMs(r.duration_ms) : fmtMs(r.mean_latency_ms)}</Td>
                    <Td>{fmtCost(r.total_cost_micro_usd)}</Td>
                    <Td>{fmtTokensCell(r.input_tokens, r.output_tokens, r.cached_input_tokens)}</Td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>
        )}
      </Section>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section>
      <h2 className="text-[0.8rem] uppercase tracking-wider font-bold text-ink-soft mb-2">
        {title}
      </h2>
      {children}
    </section>
  );
}

function ChartTitle({ children }: { children: React.ReactNode }) {
  return <div className="text-[0.75rem] font-bold uppercase tracking-wider mb-2">{children}</div>;
}

function Tr({ children, head = false }: { children: React.ReactNode; head?: boolean }) {
  return <tr className={head ? '' : 'border-t-2 border-black/15'}>{children}</tr>;
}

function Th({ children }: { children: React.ReactNode }) {
  return (
    <th className="text-left font-bold uppercase tracking-wider border-b-2 border-black px-3 py-2 text-[0.7rem] text-ink-soft whitespace-nowrap">
      {children}
    </th>
  );
}

function Td({
  children,
  bold = false,
  muted = false,
}: {
  children: React.ReactNode;
  bold?: boolean;
  muted?: boolean;
}) {
  return (
    <td
      className={`px-3 py-2 align-middle whitespace-nowrap ${bold ? 'font-bold' : ''} ${
        muted ? 'text-ink-soft' : ''
      }`}
    >
      {children}
    </td>
  );
}
