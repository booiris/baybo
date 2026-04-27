import { useEffect, useState, type ChangeEvent } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  RiAlarmLine,
  RiRefreshLine,
  RiEyeLine,
  RiTimeLine,
  RiTerminalLine,
  RiChatSmile2Line,
  RiLoader4Line,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { SelectBox } from '../components/SelectBox';
import { SearchBox } from '../components/SearchBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';
import { useMockMode, MOCK_CRONS } from '../api/mock';

type CronJob = components['schemas']['CronJob'];
type CronStatus = components['schemas']['CronStatus'];

// Default page size for cron jobs
const DEFAULT_PAGE_SIZE = 20;
const PAGE_SIZE_OPTIONS = [10, 20, 50, 100];

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

function formatTimestamp(iso: string | null | undefined): string {
  if (!iso) return '-';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString('sv-SE', {
    dateStyle: 'short',
    timeStyle: 'short',
  });
}

export function CronPage() {
  const isMock = useMockMode();
  const [searchParams, setSearchParams] = useSearchParams();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [filter, setFilter] = useState('');
  const [debouncedFilter, setDebouncedFilter] = useState('');
  const [statusFilter, setStatusFilter] = useState<'all' | CronStatus>('all');
  const [channelFilter, setChannelFilter] = useState<'all' | string>('all');
  const [scheduleKindFilter, setScheduleKindFilter] = useState<'all' | 'cron' | 'at'>('all');

  const [offset, setOffset] = useState(0);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);
  const [allItems, setAllItems] = useState<CronJob[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<CronJob | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  // Debounce the filter input
  useEffect(() => {
    const handle = window.setTimeout(() => setDebouncedFilter(filter), 250);
    return () => window.clearTimeout(handle);
  }, [filter]);

  // Reset offset on filter change
  useEffect(() => {
    setOffset(0);
  }, [debouncedFilter, statusFilter, channelFilter, scheduleKindFilter, pageSize]);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        setAllItems(MOCK_CRONS);
        setLoading(false);
        setError(null);
        return;
      }

      setLoading(true);
      setError(null);

      try {
        const { data, error: apiError, response } = await client.GET('/v1/cron');
        if (canceled) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          setError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
        setAllItems(data?.items ?? []);
      } catch (e) {
        if (canceled) return;
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (!canceled) setLoading(false);
      }
    }
    void fetchData();
    return () => { canceled = true; };
  }, [client, logout, refreshKey, isMock]);

  // Client-side filtering
  const filteredItems = allItems.filter(item => {
    if (statusFilter !== 'all' && item.status !== statusFilter) return false;
    if (channelFilter !== 'all' && item.channel !== channelFilter) return false;
    if (scheduleKindFilter !== 'all' && item.schedule.kind !== scheduleKindFilter) return false;
    
    if (debouncedFilter.trim()) {
      const q = debouncedFilter.toLowerCase().trim();
      const matchId = item.id.toLowerCase().includes(q);
      const matchPrompt = item.action.kind === 'prompt' && item.action.prompt.toLowerCase().includes(q);
      const matchTool = item.action.kind === 'tool_call' && item.action.tool_name.toLowerCase().includes(q);
      if (!matchId && !matchPrompt && !matchTool) return false;
    }
    
    return true;
  });

  // Unique channels for filter dropdown
  const availableChannels = Array.from(new Set(allItems.map(i => i.channel))).sort();

  // Client-side pagination
  const items = filteredItems.slice(offset, offset + pageSize);
  const total = filteredItems.length;
  const pageStart = items.length === 0 ? 0 : offset + 1;
  const pageEnd = offset + items.length;
  const hasPrev = offset > 0;
  const hasNext = pageEnd < total;

  const toggleMock = () => {
    const newParams = new URLSearchParams(searchParams);
    if (isMock) {
      newParams.delete('mock');
    } else {
      newParams.set('mock', 'true');
    }
    setSearchParams(newParams);
  };

  const statusBadge = (status: components['schemas']['CronStatus']) => {
    const isEnabled = status === 'enabled';
    return (
      <span
        className={`inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${
          isEnabled ? 'bg-ok text-white' : 'bg-gray-200 text-ink-soft'
        }`}
      >
        <span>{status}</span>
      </span>
    );
  };

  const scheduleDisplay = (schedule: components['schemas']['CronSchedule']) => {
    if (schedule.kind === 'cron') {
      return (
        <div className="flex items-center gap-2 font-mono text-[0.9rem]">
          <RiTimeLine className="text-ink-soft text-lg shrink-0" />
          <span>{schedule.expr}</span>
        </div>
      );
    }
    return (
      <div className="flex items-center gap-2 font-mono text-[0.9rem]">
        <RiAlarmLine className="text-ink-soft text-lg shrink-0" />
        <span>{formatTimestamp(schedule.time)}</span>
      </div>
    );
  };

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">
            CRON JOBS
          </h2>
        </div>
        <div className="flex gap-3">
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
            <RiRefreshLine className="text-base" /> Refresh
          </Button>
        </div>
      </div>

      <div className="flex items-center gap-3 mb-4">
        <SearchBox
          placeholder="Filter by ID or message..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="h-10"
        />
        
        <SelectBox
          value={statusFilter}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => setStatusFilter(e.target.value as any)}
          className="h-10 px-3"
        >
          <option value="all">All Status</option>
          <option value="enabled">Enabled</option>
          <option value="disabled">Disabled</option>
        </SelectBox>

        <SelectBox
          value={scheduleKindFilter}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => setScheduleKindFilter(e.target.value as any)}
          className="h-10 px-3"
        >
          <option value="all">All Types</option>
          <option value="cron">Recurring (Cron)</option>
          <option value="at">One-shot (At)</option>
        </SelectBox>

        <SelectBox
          value={channelFilter}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => setChannelFilter(e.target.value)}
          className="h-10 px-3 min-w-[140px]"
        >
          <option value="all">All Channels</option>
          {availableChannels.map(ch => (
            <option key={ch} value={ch}>{ch.toUpperCase()}</option>
          ))}
        </SelectBox>
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={`${thCell} w-[150px]`}>ID</th>
                <th className={`${thCell} w-[140px]`}>Status</th>
                <th className={`${thCell} w-[200px]`}>Schedule</th>
                <th className={`${thCell} w-[120px]`}>Channel</th>
                <th className={thCell}>Action</th>
                <th className={`${thCell} w-[160px]`}>Created At</th>
                <th className={`${thCell} w-[160px]`}>Next Trigger</th>
                <th className={`${thCell} w-[80px] text-right`}>Detail</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 && !loading && (
                <tr>
                  <td
                    colSpan={8}
                    className="px-6 py-10 text-center text-ink-soft text-[0.9rem]"
                  >
                    No cron jobs found.
                  </td>
                </tr>
              )}
              {items.map((job) => {
                const cell = `px-6 py-4 align-middle border-b border-black`;
                return (
                  <tr key={job.id} className="hover:bg-gray-50">
                    <td className={cell}>
                      <code className="font-mono text-[0.85rem]">{job.id}</code>
                    </td>
                    <td className={cell}>{statusBadge(job.status)}</td>
                    <td className={cell}>{scheduleDisplay(job.schedule)}</td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-bold uppercase tracking-wider">{job.channel}</span>
                    </td>
                    <td className={cell}>
                      <div className="flex items-center gap-2">
                        {job.action.kind === 'prompt' ? (
                          <RiChatSmile2Line className="text-brand shrink-0 text-lg" />
                        ) : (
                          <RiTerminalLine className="text-warn shrink-0 text-lg" />
                        )}
                        <span className="text-[0.9rem] line-clamp-1">
                          {job.action.kind === 'prompt' ? job.action.prompt : job.action.tool_name}
                        </span>
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug">
                        {formatTimestamp(job.created_at)}
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug">
                        {formatTimestamp(job.next_trigger_at)}
                      </div>
                    </td>
                    <td className={`${cell} text-right`}>
                      <IconButton
                        aria-label="View job detail"
                        onClick={() => setSelected(job)}
                      >
                        <RiEyeLine />
                      </IconButton>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>

        <div className="flex justify-between items-center px-4 py-3 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft min-w-[200px]">
            {loading ? (
              <span className="flex items-center gap-2">
                <RiLoader4Line className="animate-spin" /> Loading jobs...
              </span>
            ) : total === 0 ? (
              'No cron jobs'
            ) : (
              `Showing ${pageStart} to ${pageEnd} of ${total.toLocaleString()} jobs`
            )}
          </span>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <span className="text-[0.85rem] text-ink-soft whitespace-nowrap">Per page:</span>
              <SelectBox
                value={pageSize}
                onChange={(e: ChangeEvent<HTMLSelectElement>) => setPageSize(Number(e.target.value))}
                className="h-8 px-2"
              >
                {PAGE_SIZE_OPTIONS.map((opt) => (
                  <option key={opt} value={opt}>
                    {opt}
                  </option>
                ))}
              </SelectBox>
            </div>
            <div className="flex gap-2">
              <Button
                onClick={() => setOffset((o) => Math.max(0, o - pageSize))}
                disabled={!hasPrev || loading}
                className="!py-1 !px-3 !text-[0.85rem] h-8"
              >
                Prev
              </Button>
              <Button
                onClick={() => setOffset((o) => o + pageSize)}
                disabled={!hasNext || loading}
                className="!py-1 !px-3 !text-[0.85rem] h-8"
              >
                Next
              </Button>
            </div>
          </div>
        </div>
      </div>

      {selected && <CronDetailModal job={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function CronDetailModal({ job, onClose }: { job: CronJob; onClose: () => void }) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onClose}
    >
      <div
        className="max-w-2xl w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="flex items-center justify-between px-6 py-4 border-b-2 border-black">
          <div className="flex items-center gap-3">
            <h3 className="font-bold uppercase tracking-wider">Cron Job Detail</h3>
            <code className="font-mono text-[0.9rem] bg-gray-100 px-2 py-0.5 rounded border border-black">{job.id}</code>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
          >
            Close
          </button>
        </header>
        <div className="px-6 py-4 space-y-4">
          <div className="grid grid-cols-2 gap-4">
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">User ID</label>
              <div className="font-mono text-[0.9rem]">{job.user_id}</div>
            </div>
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Channel</label>
              <div className="font-bold uppercase text-[0.9rem]">{job.channel}</div>
            </div>
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Created At</label>
              <div className="text-[0.9rem]">{formatTimestamp(job.created_at)}</div>
            </div>
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Last Triggered</label>
              <div className="text-[0.9rem]">{formatTimestamp(job.last_triggered_at)}</div>
            </div>
          </div>

          <div>
            <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Action ({job.action.kind})</label>
            <div className="bg-gray-50 border-2 border-black rounded-md px-4 py-3 font-mono text-[0.9rem]">
              {job.action.kind === 'prompt' ? (
                <div>{job.action.prompt}</div>
              ) : (
                <pre className="whitespace-pre-wrap">
                  {job.action.tool_name}({JSON.stringify(job.action.params, null, 2)})
                </pre>
              )}
            </div>
          </div>

          {job.origin_session_id && (
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Origin Session</label>
              <code className="font-mono text-[0.85rem]">{job.origin_session_id}</code>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
