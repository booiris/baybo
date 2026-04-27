import { useMemo } from 'react';
import {
  RiBarChartBoxLine,
  RiCoinLine,
  RiInputMethodLine,
  RiDownloadLine,
  RiDatabaseLine,
  RiHistoryLine,
  RiCpuLine,
  RiLightbulbLine,
  RiToolsLine,
} from 'react-icons/ri';
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts';
import { MOCK_ANALYTICS } from '../api/mock';

const thCell =
  'px-6 py-3 text-left font-bold text-[0.75rem] uppercase tracking-wider border-b-2 border-black bg-gray-50';
const tdCell = 'px-6 py-3 align-middle border-b border-black font-mono text-[0.85rem]';

function MetricCard({ 
  title, 
  value, 
  icon: Icon, 
  subtitle, 
  colorClass = 'bg-white' 
}: { 
  title: string; 
  value: string | number; 
  icon: any; 
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

function SectionTitle({ icon: Icon, title }: { icon: any, title: string }) {
  return (
    <h3 className="font-bold uppercase tracking-wider text-[1rem] mb-4 flex items-center gap-2">
      <Icon /> {title}
    </h3>
  );
}

export function AnalyticsPage() {
  const data = MOCK_ANALYTICS;

  const chartData = useMemo(() => {
    return data?.dailyConsumption || [];
  }, [data]);

  const last7Days = useMemo(() => {
    return chartData.slice(-7).reverse();
  }, [chartData]);

  if (!data) {
    return (
      <div className="p-8 flex flex-col items-center justify-center h-full">
        <RiBarChartBoxLine className="text-6xl text-ink-soft mb-4" />
        <p className="text-ink-soft font-bold uppercase">No analytics data available</p>
      </div>
    );
  }

  const formatTokens = (val: number) => {
    if (val >= 1000000) return (val / 1000000).toFixed(1) + 'M';
    if (val >= 1000) return (val / 1000).toFixed(1) + 'k';
    return Math.floor(val).toString();
  };

  return (
    <div className="p-5 h-full flex flex-col overflow-y-auto bg-canvas">
      <div className="mb-6">
        <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">
          ANALYTICS
        </h2>
        <p className="text-ink-soft text-sm">System consumption and performance metrics over the last 30 days.</p>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-5 mb-8">
        <MetricCard 
          title="Total Tokens" 
          value={formatTokens(data.totalTokens.input + data.totalTokens.cache + data.totalTokens.output)}
          icon={RiCoinLine}
          subtitle="Combined consumption"
          colorClass="bg-brand/10"
        />
        <MetricCard 
          title="Input Tokens" 
          value={formatTokens(data.totalTokens.input)}
          icon={RiInputMethodLine}
          subtitle="User prompts & context"
        />
        <MetricCard 
          title="Cache Tokens" 
          value={formatTokens(data.totalTokens.cache)}
          icon={RiDatabaseLine}
          subtitle="Prompt caching savings"
        />
        <MetricCard 
          title="Output Tokens" 
          value={formatTokens(data.totalTokens.output)}
          icon={RiDownloadLine}
          subtitle="LLM completions"
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
                  fontFamily: 'Space Mono'
                }}
                itemStyle={{ fontWeight: 'bold' }}
              />
              <Legend 
                wrapperStyle={{ 
                  paddingTop: '20px', 
                  fontFamily: 'Space Mono', 
                  fontWeight: 'bold',
                  textTransform: 'uppercase',
                  fontSize: '11px'
                }} 
              />
              <Line type="monotone" dataKey="input" name="Input" stroke="#3b60e4" strokeWidth={3} dot={{ r: 3 }} activeDot={{ r: 5 }} />
              <Line type="monotone" dataKey="cache" name="Cache" stroke="#2f855a" strokeWidth={3} dot={{ r: 3 }} activeDot={{ r: 5 }} />
              <Line type="monotone" dataKey="output" name="Output" stroke="#e53e3e" strokeWidth={3} dot={{ r: 3 }} activeDot={{ r: 5 }} />
            </LineChart>
          </ResponsiveContainer>
        </div>
      </div>

      {/* Daily Breakdown */}
      <div className="mb-8">
        <SectionTitle icon={RiHistoryLine} title="Last 7 Days Detail" />
        <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={thCell}>Date</th>
                <th className={thCell}>Token Input</th>
                <th className={thCell}>Token Output</th>
                <th className={thCell}>Sessions Created</th>
              </tr>
            </thead>
            <tbody>
              {last7Days.map((day) => (
                <tr key={day.date} className="hover:bg-gray-50">
                  <td className={tdCell}>{day.date}</td>
                  <td className={tdCell}>{day.input.toLocaleString()}</td>
                  <td className={tdCell}>{day.output.toLocaleString()}</td>
                  <td className={tdCell}>{day.sessionsCreated}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Usage Grouped By Sections */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-8 mb-8">
        {/* Model Usage */}
        <div>
          <SectionTitle icon={RiCpuLine} title="Model Usage" />
          <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden">
            <table className="w-full border-separate border-spacing-0">
              <thead>
                <tr>
                  <th className={thCell}>Model</th>
                  <th className={thCell}>Total Input</th>
                  <th className={thCell}>Total Output</th>
                </tr>
              </thead>
              <tbody>
                {data.modelUsage.map((m) => (
                  <tr key={m.model} className="hover:bg-gray-50">
                    <td className={tdCell}><span className="font-bold">{m.model}</span></td>
                    <td className={tdCell}>{formatTokens(m.input)}</td>
                    <td className={tdCell}>{formatTokens(m.output)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>

        {/* Skill Usage */}
        <div>
          <SectionTitle icon={RiLightbulbLine} title="Skill Usage" />
          <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden">
            <table className="w-full border-separate border-spacing-0">
              <thead>
                <tr>
                  <th className={thCell}>Skill</th>
                  <th className={thCell}>Execution Count</th>
                </tr>
              </thead>
              <tbody>
                {data.skillUsage.map((s) => (
                  <tr key={s.skill} className="hover:bg-gray-50">
                    <td className={tdCell}><span className="font-bold">{s.skill}</span></td>
                    <td className={tdCell}>{s.count}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      </div>

      {/* Tool Usage */}
      <div className="mb-10">
        <SectionTitle icon={RiToolsLine} title="Tool Usage" />
        <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={thCell}>Tool</th>
                <th className={thCell}>Usage Count</th>
                <th className={thCell}>Avg. Execution Time</th>
              </tr>
            </thead>
            <tbody>
              {data.toolUsage.map((t) => (
                <tr key={t.tool} className="hover:bg-gray-50">
                  <td className={tdCell}><code className="bg-gray-100 px-1 rounded">{t.tool}</code></td>
                  <td className={tdCell}>{t.count}</td>
                  <td className={tdCell}>{t.avgExecutionTimeMs}ms</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
