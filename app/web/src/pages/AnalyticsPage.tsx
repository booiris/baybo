import { useEffect, useMemo, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  RiBarChartBoxLine,
  RiUploadLine,
  RiDownloadLine,
  RiHistoryLine,
  RiCpuLine,
  RiBrainLine,
  RiPriceTag3Line,
  RiDatabase2Line,
  RiLoader4Line,
  RiRefreshLine,
  RiTeamLine,
  RiMoneyDollarCircleLine,
} from 'react-icons/ri';
import type { IconType } from 'react-icons';
import {
  LineChart,
  Line,
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts';
import { Button } from '../components/Button';
import { SelectBox } from '../components/SelectBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';
import { useMockMode } from '../api/mock';

type AnalyticsResponse = components['schemas']['AnalyticsResponse'];

const RANGE_OPTIONS = [
  { value: 7, label: 'Last 7 days' },
  { value: 30, label: 'Last 30 days' },
  { value: 90, label: 'Last 90 days' },
] as const;

const thCell =
  'px-6 py-3 text-left font-bold text-[0.75rem] uppercase tracking-wider border-b-2 border-black bg-gray-50';
const tdCell = 'px-6 py-3 align-middle border-b border-black font-mono text-[0.85rem]';

function MetricCard({
  title,
  value,
  icon: Icon,
  subtitle,
  colorClass = 'bg-white',
}: {
  title: string;
  value: string | number;
  icon: IconType;
  subtitle?: string;
  colorClass?: string;
}) {
  return (
    <div className={`${colorClass} border-[3px] border-black rounded-md shadow-brutal p-5 flex flex-col justify-between`}>
      <div className="flex items-center justify-between mb-4">
        <span className="font-bold uppercase tracking-wider text-[0.8rem] text-ink-soft">{title}</span>
        <div className="w-10 h-10 rounded-md border-2 border-black flex items-center justify-center bg-white shadow-brutal-xs">
          <Icon className="text-xl" />
        </div>
      </div>
      <div>
        <div className="text-2xl font-bold font-mono tracking-tight">{value}</div>
        {subtitle && <div className="text-[0.75rem] text-ink-soft font-mono mt-1">{subtitle}</div>}
      </div>
    </div>
  );
}

function SectionTitle({ icon: Icon, title }: { icon: IconType; title: string }) {
  return (
    <h3 className="font-bold uppercase tracking-wider text-[1rem] mb-4 flex items-center gap-2">
      <Icon /> {title}
    </h3>
  );
}

function formatTokens(val: number): string {
  if (val >= 1_000_000) return `${(val / 1_000_000).toFixed(1)}M`;
  if (val >= 1_000) return `${(val / 1_000).toFixed(1)}k`;
  return Math.floor(val).toString();
}

// All three Anthropic-style input fields count as input sent to the model;
// they only differ in the cache billing tier. Total input volume = sum of
// uncached + cache reads + cache writes.
function totalInput(row: {
  input_tokens: number;
  cached_input_tokens: number;
  cache_creation_input_tokens: number;
}): number {
  return row.input_tokens + row.cached_input_tokens + row.cache_creation_input_tokens;
}

// Wire format is integer micro-USD (USD × 10^6). Divide here at the
// rendering boundary so the rest of the stack stays in exact integer
// math — no float drift across totals or chart aggregates.
function formatMicroUsd(microUsd: number): string {
  const usd = microUsd / 1_000_000;
  if (usd >= 1_000) return `$${usd.toFixed(0).toLocaleString()}`;
  if (usd >= 10) return `$${usd.toFixed(2)}`;
  return `$${usd.toFixed(4)}`;
}

// Build a synthetic mock AnalyticsResponse for DEV mock mode so the
// page is fully usable without a running backend.
function generateMockAnalytics(days: number): AnalyticsResponse {
  const until = new Date();
  const since = new Date(until.getTime() - days * 24 * 60 * 60 * 1000);
  const daily: AnalyticsResponse['daily'] = [];
  let totIn = 0;
  let totOut = 0;
  // Cost is in micro-USD on the wire — keep the mock generator in the
  // same unit so it reads back identically through formatMicroUsd.
  let totCostMicroUsd = 0;
  let totRecords = 0;
  let totCached = 0;
  let totCacheCreate = 0;
  for (let i = days - 1; i >= 0; i--) {
    const d = new Date(until);
    d.setDate(d.getDate() - i);
    const date = d.toISOString().split('T')[0];
    const input = Math.floor(Math.random() * 5000) + 1000;
    const output = Math.floor(Math.random() * 3000) + 800;
    // ~60% of input served from prompt cache, ~10% written into cache
    const cached = Math.floor(input * 0.6);
    const cacheCreate = Math.floor(input * 0.1);
    // Original mock rate: $0.0000035 per token ≈ 3.5 micro-USD/token.
    const costMicroUsd = Math.floor((input + output) * 3.5);
    const sessions = Math.floor(Math.random() * 50) + 5;
    daily.push({
      date,
      input_tokens: input,
      output_tokens: output,
      cached_input_tokens: cached,
      cache_creation_input_tokens: cacheCreate,
      cost_micro_usd: costMicroUsd,
      sessions_created: sessions,
    });
    totIn += input;
    totOut += output;
    totCached += cached;
    totCacheCreate += cacheCreate;
    totCostMicroUsd += costMicroUsd;
    totRecords += Math.floor(sessions * 1.5);
  }
  return {
    since: since.toISOString(),
    until: until.toISOString(),
    total_input_tokens: totIn,
    total_output_tokens: totOut,
    total_cached_input_tokens: totCached,
    total_cache_creation_input_tokens: totCacheCreate,
    total_cost_micro_usd: totCostMicroUsd,
    total_record_count: totRecords,
    daily,
    by_model: [
      {
        model: 'claude-sonnet-4-6',
        input_tokens: Math.floor(totIn * 0.7),
        output_tokens: Math.floor(totOut * 0.75),
        cached_input_tokens: Math.floor(totCached * 0.7),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.7),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.7),
        call_count: Math.floor(totRecords * 0.6),
      },
      {
        model: 'claude-haiku-4-5',
        input_tokens: Math.floor(totIn * 0.2),
        output_tokens: Math.floor(totOut * 0.18),
        cached_input_tokens: Math.floor(totCached * 0.2),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.2),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.15),
        call_count: Math.floor(totRecords * 0.3),
      },
      {
        model: 'gpt-4o',
        input_tokens: Math.floor(totIn * 0.1),
        output_tokens: Math.floor(totOut * 0.07),
        cached_input_tokens: Math.floor(totCached * 0.1),
        cache_creation_input_tokens: 0,
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.15),
        call_count: Math.floor(totRecords * 0.1),
      },
    ],
    by_reason: [
      {
        reason: 'chat',
        input_tokens: Math.floor(totIn * 0.8),
        output_tokens: Math.floor(totOut * 0.82),
        cached_input_tokens: Math.floor(totCached * 0.8),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.8),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.78),
        call_count: Math.floor(totRecords * 0.7),
      },
      {
        reason: 'compression',
        input_tokens: Math.floor(totIn * 0.15),
        output_tokens: Math.floor(totOut * 0.1),
        cached_input_tokens: Math.floor(totCached * 0.15),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.15),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.14),
        call_count: Math.floor(totRecords * 0.2),
      },
      {
        reason: 'tool',
        input_tokens: Math.floor(totIn * 0.05),
        output_tokens: Math.floor(totOut * 0.08),
        cached_input_tokens: Math.floor(totCached * 0.05),
        cache_creation_input_tokens: 0,
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.08),
        call_count: Math.floor(totRecords * 0.1),
      },
    ],
    by_reasoning_effort: [
      {
        reasoning_effort: 'high',
        input_tokens: Math.floor(totIn * 0.45),
        output_tokens: Math.floor(totOut * 0.5),
        cached_input_tokens: Math.floor(totCached * 0.45),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.45),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.45),
        call_count: Math.floor(totRecords * 0.4),
      },
      {
        reasoning_effort: 'medium',
        input_tokens: Math.floor(totIn * 0.4),
        output_tokens: Math.floor(totOut * 0.4),
        cached_input_tokens: Math.floor(totCached * 0.4),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.4),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.4),
        call_count: Math.floor(totRecords * 0.4),
      },
      {
        reasoning_effort: null,
        input_tokens: Math.floor(totIn * 0.15),
        output_tokens: Math.floor(totOut * 0.1),
        cached_input_tokens: Math.floor(totCached * 0.15),
        cache_creation_input_tokens: Math.floor(totCacheCreate * 0.15),
        cost_micro_usd: Math.floor(totCostMicroUsd * 0.15),
        call_count: Math.floor(totRecords * 0.2),
      },
    ],
  };
}

export function AnalyticsPage() {
  const isMock = useMockMode();
  const [searchParams, setSearchParams] = useSearchParams();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [days, setDays] = useState<number>(30);
  const [data, setData] = useState<AnalyticsResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        setData(generateMockAnalytics(days));
        setError(null);
        setLoading(false);
        return;
      }
      setLoading(true);
      setError(null);
      const until = new Date();
      const since = new Date(until.getTime() - days * 24 * 60 * 60 * 1000);
      try {
        const { data: payload, error: apiError, response } = await client.GET('/v1/analytics', {
          params: {
            query: { since: since.toISOString(), until: until.toISOString() },
          },
        });
        if (canceled) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          setError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
        setData(payload ?? null);
      } catch (e) {
        if (canceled) return;
        setError(
          e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway',
        );
      } finally {
        if (!canceled) setLoading(false);
      }
    }
    void fetchData();
    return () => {
      canceled = true;
    };
  }, [client, days, logout, refreshKey, isMock]);

  const chartData = useMemo(
    () =>
      (data?.daily ?? []).map((d) => ({
        ...d,
        total_input_tokens: totalInput(d),
        // "Uncached" = anything not served from prompt cache. Cache *creation*
        // is the first-time write — at this call it is not a cache hit, so it
        // bills at the non-cached tier and is grouped here.
        uncached_input_tokens: d.input_tokens + d.cache_creation_input_tokens,
      })),
    [data],
  );
  const last7Days = useMemo(() => chartData.slice(-7).reverse(), [chartData]);
  const totalSessions = useMemo(
    () => chartData.reduce((acc, d) => acc + d.sessions_created, 0),
    [chartData],
  );

  const toggleMock = () => {
    const next = new URLSearchParams(searchParams);
    if (isMock) next.delete('mock');
    else next.set('mock', 'true');
    setSearchParams(next);
  };

  if (loading && !data) {
    return (
      <div className="p-8 flex flex-col items-center justify-center h-full">
        <RiLoader4Line className="animate-spin text-4xl text-ink-soft mb-3" />
        <p className="text-ink-soft font-bold uppercase tracking-wider">Loading analytics…</p>
      </div>
    );
  }

  return (
    <div className="p-5 h-full flex flex-col overflow-y-auto bg-canvas">
      <div className="mb-6 flex justify-between items-start gap-3 flex-wrap">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">ANALYTICS</h2>
        </div>
        <div className="flex items-center gap-3">
          <SelectBox
            value={days}
            onChange={(e) => setDays(Number(e.target.value))}
            className="h-10 px-3"
          >
            {RANGE_OPTIONS.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </SelectBox>
          {import.meta.env.DEV && (
            <Button
              variant={isMock ? 'primary' : 'default'}
              onClick={toggleMock}
              className="!py-2 !px-4 !text-[0.9rem] h-10 w-[140px] justify-center gap-1.5"
            >
              {isMock ? 'Mock: ON' : 'Mock: OFF'}
            </Button>
          )}
          <Button
            onClick={() => setRefreshKey((k) => k + 1)}
            disabled={loading || isMock}
            className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
          >
            <RiRefreshLine className="text-lg shrink-0" /> Refresh
          </Button>
        </div>
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      )}

      {!data ? (
        <div className="p-8 flex flex-col items-center justify-center h-full">
          <RiBarChartBoxLine className="text-6xl text-ink-soft mb-4" />
          <p className="text-ink-soft font-bold uppercase">No analytics data available</p>
        </div>
      ) : (
        <>
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-5 mb-8">
            <MetricCard
              title="Total Input Tokens"
              value={formatTokens(
                data.total_input_tokens +
                  data.total_cached_input_tokens +
                  data.total_cache_creation_input_tokens,
              )}
              icon={RiUploadLine}
              subtitle="Prompts & context (incl. cache)"
            />
            <MetricCard
              title="Output Tokens"
              value={formatTokens(data.total_output_tokens)}
              icon={RiDownloadLine}
              subtitle="Completions"
            />
            <MetricCard
              title="Cached Input"
              value={formatTokens(data.total_cached_input_tokens)}
              icon={RiDatabase2Line}
              subtitle={
                data.total_cache_creation_input_tokens > 0
                  ? `+ ${formatTokens(data.total_cache_creation_input_tokens)} cache writes`
                  : 'Prompt-cache hits'
              }
            />
            <MetricCard
              title="Total Cost"
              value={formatMicroUsd(data.total_cost_micro_usd)}
              icon={RiMoneyDollarCircleLine}
              subtitle={`${data.total_record_count.toLocaleString()} LLM calls`}
            />
          </div>

          <div className="bg-white border-[3px] border-black rounded-md shadow-brutal p-6 flex flex-col mb-8 min-h-[400px]">
            <SectionTitle icon={RiBarChartBoxLine} title="Token Consumption Trend" />
            <div className="flex-1 w-full h-full min-h-[300px]">
              <ResponsiveContainer width="100%" height="100%">
                <LineChart data={chartData} margin={{ top: 5, right: 30, left: 20, bottom: 5 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke="#e0e0e0" vertical={false} />
                  <XAxis
                    dataKey="date"
                    axisLine={{ stroke: '#000', strokeWidth: 2 }}
                    tickLine={{ stroke: '#000' }}
                    tick={{ fill: '#555', fontSize: 11, fontFamily: 'Space Mono' }}
                    dy={10}
                  />
                  <YAxis
                    yAxisId="tokens"
                    axisLine={{ stroke: '#000', strokeWidth: 2 }}
                    tickLine={{ stroke: '#000' }}
                    tick={{ fill: '#555', fontSize: 11, fontFamily: 'Space Mono' }}
                    tickFormatter={(v: number) => formatTokens(v)}
                    dx={-10}
                  />
                  <Tooltip
                    contentStyle={{
                      backgroundColor: '#fff',
                      border: '3px solid #000',
                      borderRadius: '6px',
                      boxShadow: '4px 4px 0 0 #000',
                      fontFamily: 'Space Mono',
                    }}
                    itemStyle={{ fontWeight: 'bold' }}
                    formatter={(value: number) => value.toLocaleString()}
                  />
                  <Legend
                    wrapperStyle={{
                      paddingTop: '20px',
                      fontFamily: 'Space Mono',
                      fontWeight: 'bold',
                      textTransform: 'uppercase',
                      fontSize: '11px',
                    }}
                  />
                  <Line yAxisId="tokens" type="monotone" dataKey="total_input_tokens" name="Input (total)" stroke="#3b60e4" strokeWidth={3} dot={{ r: 3 }} activeDot={{ r: 5 }} />
                  <Line yAxisId="tokens" type="monotone" dataKey="cached_input_tokens" name="Input (cached)" stroke="#2f855a" strokeWidth={2} strokeDasharray="4 3" dot={{ r: 2 }} activeDot={{ r: 4 }} />
                  <Line yAxisId="tokens" type="monotone" dataKey="uncached_input_tokens" name="Input (uncached)" stroke="#dd6b20" strokeWidth={2} strokeDasharray="4 3" dot={{ r: 2 }} activeDot={{ r: 4 }} />
                  <Line yAxisId="tokens" type="monotone" dataKey="output_tokens" name="Output" stroke="#e53e3e" strokeWidth={3} dot={{ r: 3 }} activeDot={{ r: 5 }} />
                </LineChart>
              </ResponsiveContainer>
            </div>
          </div>

          <div className="bg-white border-[3px] border-black rounded-md shadow-brutal p-6 flex flex-col mb-8 min-h-[280px]">
            <div className="flex items-baseline justify-between mb-4">
              <SectionTitle icon={RiTeamLine} title="Sessions Created" />
              <span className="text-[0.75rem] text-ink-soft font-mono">
                {totalSessions.toLocaleString()} total over {chartData.length} days
              </span>
            </div>
            <div className="flex-1 w-full min-h-[220px]">
              <ResponsiveContainer width="100%" height="100%">
                <BarChart data={chartData} margin={{ top: 5, right: 30, left: 20, bottom: 45 }}>
                  <CartesianGrid strokeDasharray="3 3" stroke="#e0e0e0" vertical={false} />
                  <XAxis
                    dataKey="date"
                    axisLine={{ stroke: '#000', strokeWidth: 2 }}
                    tickLine={{ stroke: '#000' }}
                    tick={{ fill: '#555', fontSize: 11, fontFamily: 'Space Mono' }}
                    dy={10}
                  />
                  <YAxis
                    allowDecimals={false}
                    axisLine={{ stroke: '#000', strokeWidth: 2 }}
                    tickLine={{ stroke: '#000' }}
                    tick={{ fill: '#555', fontSize: 11, fontFamily: 'Space Mono' }}
                    dx={-10}
                  />
                  <Tooltip
                    contentStyle={{
                      backgroundColor: '#fff',
                      border: '3px solid #000',
                      borderRadius: '6px',
                      boxShadow: '4px 4px 0 0 #000',
                      fontFamily: 'Space Mono',
                    }}
                    itemStyle={{ fontWeight: 'bold' }}
                  />
                  <Bar dataKey="sessions_created" name="Sessions" fill="#2f855a" stroke="#000" strokeWidth={1.5} />
                </BarChart>
              </ResponsiveContainer>
            </div>
          </div>

          <div className="mb-8">
            <SectionTitle icon={RiHistoryLine} title="Last 7 Days Detail" />
            <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-x-auto">
              <table className="w-full border-separate border-spacing-0">
                <thead>
                  <tr>
                    <th className={thCell}>Date</th>
                    <th className={`${thCell} text-right`}>Input</th>
                    <th className={`${thCell} text-right`}>Output</th>
                    <th className={`${thCell} text-right`}>Cached</th>
                    <th className={`${thCell} text-right`}>Cost</th>
                    <th className={`${thCell} text-right`}>Sessions</th>
                  </tr>
                </thead>
                <tbody>
                  {last7Days.length === 0 && (
                    <tr>
                      <td colSpan={6} className="px-6 py-8 text-center text-ink-soft text-[0.85rem]">
                        No activity recorded.
                      </td>
                    </tr>
                  )}
                  {last7Days.map((day) => (
                    <tr key={day.date} className="hover:bg-gray-50">
                      <td className={tdCell}>{day.date}</td>
                      <td className={`${tdCell} text-right`}>{totalInput(day).toLocaleString()}</td>
                      <td className={`${tdCell} text-right`}>{day.output_tokens.toLocaleString()}</td>
                      <td className={`${tdCell} text-right`}>
                        {day.cached_input_tokens.toLocaleString()}
                        {day.cache_creation_input_tokens > 0 && (
                          <span className="text-ink-soft">
                            {' '}
                            (+{day.cache_creation_input_tokens.toLocaleString()})
                          </span>
                        )}
                      </td>
                      <td className={`${tdCell} text-right`}>{formatMicroUsd(day.cost_micro_usd)}</td>
                      <td className={`${tdCell} text-right`}>{day.sessions_created}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          <div className="mb-10">
            <SectionTitle icon={RiCpuLine} title="Per-Model Breakdown" />
            <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-x-auto">
              <table className="w-full border-separate border-spacing-0">
                <thead>
                  <tr>
                    <th className={thCell}>Model</th>
                    <th className={`${thCell} text-right`}>Calls</th>
                    <th className={`${thCell} text-right`}>Input</th>
                    <th className={`${thCell} text-right`}>Output</th>
                    <th className={`${thCell} text-right`}>Cached</th>
                    <th className={`${thCell} text-right`}>Cost</th>
                  </tr>
                </thead>
                <tbody>
                  {data.by_model.length === 0 && (
                    <tr>
                      <td colSpan={6} className="px-6 py-8 text-center text-ink-soft text-[0.85rem]">
                        No LLM calls recorded in this window.
                      </td>
                    </tr>
                  )}
                  {data.by_model.map((m) => (
                    <tr key={m.model} className="hover:bg-gray-50">
                      <td className={tdCell}>
                        <span className="font-bold break-all">{m.model}</span>
                      </td>
                      <td className={`${tdCell} text-right`}>{m.call_count.toLocaleString()}</td>
                      <td className={`${tdCell} text-right`}>{formatTokens(totalInput(m))}</td>
                      <td className={`${tdCell} text-right`}>{formatTokens(m.output_tokens)}</td>
                      <td className={`${tdCell} text-right`}>
                        {formatTokens(m.cached_input_tokens)}
                        {m.cache_creation_input_tokens > 0 && (
                          <span className="text-ink-soft">
                            {' '}
                            (+{formatTokens(m.cache_creation_input_tokens)})
                          </span>
                        )}
                      </td>
                      <td className={`${tdCell} text-right`}>{formatMicroUsd(m.cost_micro_usd)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          <div className="mb-10">
            <SectionTitle icon={RiBrainLine} title="Per-Thinking-Effort Breakdown" />
            <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-x-auto">
              <table className="w-full border-separate border-spacing-0">
                <thead>
                  <tr>
                    <th className={thCell}>Thinking Effort</th>
                    <th className={`${thCell} text-right`}>Calls</th>
                    <th className={`${thCell} text-right`}>Input</th>
                    <th className={`${thCell} text-right`}>Output</th>
                    <th className={`${thCell} text-right`}>Cached</th>
                    <th className={`${thCell} text-right`}>Cost</th>
                  </tr>
                </thead>
                <tbody>
                  {data.by_reasoning_effort.length === 0 && (
                    <tr>
                      <td colSpan={6} className="px-6 py-8 text-center text-ink-soft text-[0.85rem]">
                        No LLM calls recorded in this window.
                      </td>
                    </tr>
                  )}
                  {data.by_reasoning_effort.map((r) => (
                    <tr key={r.reasoning_effort ?? 'not-specified'} className="hover:bg-gray-50">
                      <td className={tdCell}>
                        <span className="font-bold">
                          {r.reasoning_effort ?? 'Not specified'}
                        </span>
                      </td>
                      <td className={`${tdCell} text-right`}>{r.call_count.toLocaleString()}</td>
                      <td className={`${tdCell} text-right`}>{formatTokens(totalInput(r))}</td>
                      <td className={`${tdCell} text-right`}>{formatTokens(r.output_tokens)}</td>
                      <td className={`${tdCell} text-right`}>
                        {formatTokens(r.cached_input_tokens)}
                        {r.cache_creation_input_tokens > 0 && (
                          <span className="text-ink-soft">
                            {' '}
                            (+{formatTokens(r.cache_creation_input_tokens)})
                          </span>
                        )}
                      </td>
                      <td className={`${tdCell} text-right`}>{formatMicroUsd(r.cost_micro_usd)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          <div className="mb-10">
            <SectionTitle icon={RiPriceTag3Line} title="Per-Reason Breakdown" />
            <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-x-auto">
              <table className="w-full border-separate border-spacing-0">
                <thead>
                  <tr>
                    <th className={thCell}>Reason</th>
                    <th className={`${thCell} text-right`}>Calls</th>
                    <th className={`${thCell} text-right`}>Input</th>
                    <th className={`${thCell} text-right`}>Output</th>
                    <th className={`${thCell} text-right`}>Cached</th>
                    <th className={`${thCell} text-right`}>Cost</th>
                  </tr>
                </thead>
                <tbody>
                  {data.by_reason.length === 0 && (
                    <tr>
                      <td colSpan={6} className="px-6 py-8 text-center text-ink-soft text-[0.85rem]">
                        No LLM calls recorded in this window.
                      </td>
                    </tr>
                  )}
                  {data.by_reason.map((r) => (
                    <tr key={r.reason} className="hover:bg-gray-50">
                      <td className={tdCell}>
                        <span className="font-bold">{r.reason}</span>
                      </td>
                      <td className={`${tdCell} text-right`}>{r.call_count.toLocaleString()}</td>
                      <td className={`${tdCell} text-right`}>{formatTokens(totalInput(r))}</td>
                      <td className={`${tdCell} text-right`}>{formatTokens(r.output_tokens)}</td>
                      <td className={`${tdCell} text-right`}>
                        {formatTokens(r.cached_input_tokens)}
                        {r.cache_creation_input_tokens > 0 && (
                          <span className="text-ink-soft">
                            {' '}
                            (+{formatTokens(r.cache_creation_input_tokens)})
                          </span>
                        )}
                      </td>
                      <td className={`${tdCell} text-right`}>{formatMicroUsd(r.cost_micro_usd)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
