import { useCallback, useEffect, useState } from 'react';
import { RiRefreshLine, RiLoader4Line, RiRobot2Line, RiTerminalBoxLine } from 'react-icons/ri';
import { Button } from '../components/Button';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';
import { useMockMode, MOCK_BACKGROUND_JOBS } from '../api/mock';

type BackgroundJobsResponse = components['schemas']['BackgroundJobsResponse'];
type BackgroundJob = components['schemas']['BackgroundJob'];

// Background jobs are live + short-lived, so poll while the tab is open.
const POLL_MS = 4000;

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

const EMPTY: BackgroundJobsResponse = { jobs: [], budget: { running: 0, total: 0 } };

function stateBadge(state: string) {
  const style = state === 'running' ? 'bg-ok text-white' : 'bg-warning text-white';
  return (
    <span
      className={`inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${style}`}
    >
      {state}
    </span>
  );
}

function kindBadge(kind: string) {
  const isCommand = kind === 'command';
  return (
    <span className="inline-flex items-center gap-1.5 font-mono text-[0.85rem]">
      {isCommand ? (
        <RiTerminalBoxLine className="text-ink-soft text-lg shrink-0" />
      ) : (
        <RiRobot2Line className="text-brand text-lg shrink-0" />
      )}
      {kind}
    </span>
  );
}

function BudgetGauge({ running, total }: { running: number; total: number }) {
  // total === 0 shouldn't happen (config validates >= 1) but guard the bar math.
  const slots = Math.max(total, running, 1);
  return (
    <div className="bg-white border-[3px] border-black rounded-md shadow-brutal px-5 py-4 mb-4">
      <div className="flex items-center justify-between mb-2">
        <span className="text-[0.8rem] font-bold uppercase tracking-wider text-ink-soft">
          Background subagent budget
        </span>
        <span className="font-mono text-[0.95rem] font-bold">
          {running} / {total} running
        </span>
      </div>
      <div className="flex gap-1">
        {Array.from({ length: slots }).map((_, i) => (
          <div
            key={i}
            className={`h-4 flex-1 border-2 border-black rounded-sm ${
              i < running ? 'bg-brand' : 'bg-gray-100'
            }`}
          />
        ))}
      </div>
    </div>
  );
}

export function JobsPage() {
  const isMock = useMockMode();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [data, setData] = useState<BackgroundJobsResponse>(EMPTY);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  const fetchData = useCallback(
    async (canceled: () => boolean) => {
      if (isMock) {
        setData(MOCK_BACKGROUND_JOBS);
        setLoading(false);
        setError(null);
        return;
      }
      setLoading(true);
      try {
        const { data: body, error: apiError, response } = await client.GET('/v1/background-jobs');
        if (canceled()) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          setError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
        setData(body ?? EMPTY);
        setError(null);
      } catch (e) {
        if (canceled()) return;
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (!canceled()) setLoading(false);
      }
    },
    [client, logout, isMock],
  );

  useEffect(() => {
    let canceled = false;
    void fetchData(() => canceled);
    return () => {
      canceled = true;
    };
  }, [fetchData, refreshKey]);

  // Poll while mounted (no polling in mock mode — the data is static).
  useEffect(() => {
    if (isMock) return;
    const handle = window.setInterval(() => setRefreshKey((k) => k + 1), POLL_MS);
    return () => window.clearInterval(handle);
  }, [isMock]);

  const jobs: BackgroundJob[] = data.jobs;
  const queued = jobs.filter((j) => j.state === 'queued').length;

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">BACKGROUND JOBS</h2>
        <Button
          onClick={() => setRefreshKey((k) => k + 1)}
          disabled={loading || isMock}
          className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
        >
          <RiRefreshLine className="text-lg shrink-0" /> Refresh
        </Button>
      </div>

      <BudgetGauge running={data.budget.running} total={data.budget.total} />

      {error && (
        <div className="mb-4 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={`${thCell} w-[160px]`}>State</th>
                <th className={`${thCell} w-[180px]`}>Kind</th>
                <th className={thCell}>Summary</th>
                <th className={`${thCell} w-[220px]`}>Session</th>
                <th className={`${thCell} w-[220px]`}>Handle</th>
              </tr>
            </thead>
            <tbody>
              {jobs.length === 0 && !loading && (
                <tr>
                  <td colSpan={5} className="px-6 py-10 text-center text-ink-soft text-[0.9rem]">
                    No background jobs in flight.
                  </td>
                </tr>
              )}
              {jobs.map((job) => {
                const cell = 'px-6 py-4 align-middle border-b border-black';
                return (
                  <tr key={job.handle} className="hover:bg-gray-50">
                    <td className={cell}>{stateBadge(job.state)}</td>
                    <td className={cell}>{kindBadge(job.kind)}</td>
                    <td className={cell}>
                      <span className="text-[0.9rem] line-clamp-1">{job.summary}</span>
                    </td>
                    <td className={cell}>
                      <code className="font-mono text-[0.8rem] text-ink-soft break-all">
                        {job.session_id}
                      </code>
                    </td>
                    <td className={cell}>
                      <code className="font-mono text-[0.8rem] break-all">{job.handle}</code>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>

        <div className="flex justify-between items-center px-4 py-3 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft">
            {loading && jobs.length === 0 ? (
              <span className="flex items-center gap-2">
                <RiLoader4Line className="animate-spin" /> Loading…
              </span>
            ) : (
              `${jobs.length} in flight · ${queued} queued`
            )}
          </span>
        </div>
      </div>
    </div>
  );
}
