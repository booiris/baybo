import { useEffect, useState, type ChangeEvent } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  RiAlarmLine,
  RiRefreshLine,
  RiEyeLine,
  RiTimeLine,
  RiChatSmile2Line,
  RiLoader4Line,
  RiDeleteBinLine,
  RiPauseLine,
  RiPlayLine,
  RiArrowGoBackLine,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { SelectBox } from '../components/SelectBox';
import { SearchBox } from '../components/SearchBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { AdminClient } from '../api/client';
import type { components } from '../api/schema';
import { useMockMode, MOCK_CRONS } from '../api/mock';

type CronJob = components['schemas']['CronJob'];
type CronStatus = components['schemas']['CronStatus'];

// Default page size for cron jobs
const DEFAULT_PAGE_SIZE = 20;
const PAGE_SIZE_OPTIONS = [10, 20, 50, 100];

const MOCK_BLOCKED = 'Cron mutations are disabled in mock mode.';

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

const STATUS_BADGE_STYLE: Record<CronStatus, string> = {
  enabled: 'bg-ok text-white',
  disabled: 'bg-gray-200 text-ink-soft',
  executed: 'bg-brand text-ink',
};

/** Which list the page is showing: the live jobs, or the recycle bin. */
export type CronView = 'live' | 'deleted';

/** The bodyless `POST /v1/cron/{id}/<action>` routes. */
export type CronAction = 'pause' | 'resume' | 'restore';

export type CronListOutcome =
  | { kind: 'ok'; items: CronJob[] }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

export type CronMutationOutcome =
  | { kind: 'ok' }
  | { kind: 'unauthorized' }
  | { kind: 'failed'; message: string };

function networkMessage(e: unknown): string {
  return e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway';
}

/**
 * A paused job resumes, a running one pauses; an executed one-shot has nothing
 * left to schedule, so it offers neither.
 */
export function toggleActionFor(status: CronStatus): 'pause' | 'resume' | null {
  if (status === 'enabled') return 'pause';
  if (status === 'disabled') return 'resume';
  return null;
}

export async function fetchCronJobs(client: AdminClient, view: CronView): Promise<CronListOutcome> {
  try {
    const { data, error, response } = await client.GET('/v1/cron', {
      params: { query: { deleted: view === 'deleted' } },
    });
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error || !response.ok) {
      return { kind: 'failed', message: error?.error || `HTTP Error ${response.status}` };
    }
    return { kind: 'ok', items: data?.items ?? [] };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

async function runMutation(
  call: () => Promise<{ error?: components['schemas']['ErrorBody']; response: Response }>,
): Promise<CronMutationOutcome> {
  try {
    const { error, response } = await call();
    if (response.status === 401) return { kind: 'unauthorized' };
    if (error || !response.ok) {
      return { kind: 'failed', message: error?.error || `HTTP Error ${response.status}` };
    }
    return { kind: 'ok' };
  } catch (e) {
    return { kind: 'failed', message: networkMessage(e) };
  }
}

/** Soft delete: the row moves to the recycle bin, it does not disappear. */
export function deleteCronJob(client: AdminClient, id: string): Promise<CronMutationOutcome> {
  return runMutation(() => client.DELETE('/v1/cron/{id}', { params: { path: { id } } }));
}

export function actOnCronJob(
  client: AdminClient,
  id: string,
  action: CronAction,
): Promise<CronMutationOutcome> {
  switch (action) {
    case 'pause':
      return runMutation(() => client.POST('/v1/cron/{id}/pause', { params: { path: { id } } }));
    case 'resume':
      // 400 when a one-shot's moment has already passed — the caller surfaces it.
      return runMutation(() => client.POST('/v1/cron/{id}/resume', { params: { path: { id } } }));
    case 'restore':
      return runMutation(() => client.POST('/v1/cron/{id}/restore', { params: { path: { id } } }));
  }
}

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

  const [view, setView] = useState<CronView>('live');
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
  const [pendingDeleteId, setPendingDeleteId] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);
  const [mutationError, setMutationError] = useState<string | null>(null);

  const deletedView = view === 'deleted';

  // Debounce the filter input
  useEffect(() => {
    const handle = window.setTimeout(() => setDebouncedFilter(filter), 250);
    return () => window.clearTimeout(handle);
  }, [filter]);

  // Reset offset on filter change
  useEffect(() => {
    setOffset(0);
  }, [debouncedFilter, statusFilter, channelFilter, scheduleKindFilter, pageSize, view]);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        setAllItems(MOCK_CRONS.filter((job) => Boolean(job.deleted_at) === deletedView));
        setLoading(false);
        setError(null);
        return;
      }

      setLoading(true);
      setError(null);

      const outcome = await fetchCronJobs(client, view);
      if (canceled) return;
      setLoading(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setAllItems(outcome.items);
    }
    void fetchData();
    return () => { canceled = true; };
  }, [client, logout, refreshKey, isMock, view, deletedView]);

  // Client-side filtering
  const filteredItems = allItems.filter(item => {
    if (statusFilter !== 'all' && item.status !== statusFilter) return false;
    if (channelFilter !== 'all' && item.channel !== channelFilter) return false;
    if (scheduleKindFilter !== 'all' && item.schedule.kind !== scheduleKindFilter) return false;

    if (debouncedFilter.trim()) {
      const q = debouncedFilter.toLowerCase().trim();
      const matchId = item.id.toLowerCase().includes(q);
      const matchTitle = item.title.toLowerCase().includes(q);
      const matchPrompt = item.prompt.toLowerCase().includes(q);
      if (!matchId && !matchTitle && !matchPrompt) return false;
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

  const mutate = async (id: string, run: () => Promise<CronMutationOutcome>): Promise<void> => {
    if (isMock) {
      setMutationError(MOCK_BLOCKED);
      return;
    }
    setMutating(true);
    setMutationError(null);
    const outcome = await run();
    setMutating(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setMutationError(outcome.message);
      return;
    }
    setPendingDeleteId(null);
    // The row's status / list membership just changed server-side: drop the
    // detail modal's stale copy and refetch rather than patching in place.
    setSelected((cur) => (cur?.id === id ? null : cur));
    setRefreshKey((k) => k + 1);
  };

  const handleDelete = (id: string): Promise<void> => mutate(id, () => deleteCronJob(client, id));

  const handleAction = (id: string, action: CronAction): Promise<void> =>
    mutate(id, () => actOnCronJob(client, id, action));

  const toggleMock = () => {
    const newParams = new URLSearchParams(searchParams);
    if (isMock) {
      newParams.delete('mock');
    } else {
      newParams.set('mock', 'true');
    }
    setSearchParams(newParams);
  };

  const statusBadge = (status: CronStatus) => (
    <span
      className={`inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${STATUS_BADGE_STYLE[status]}`}
    >
      <span>{status}</span>
    </span>
  );

  const scheduleDisplay = (
    schedule: components['schemas']['CronSchedule'],
    timezone: string,
  ) => {
    if (schedule.kind === 'cron') {
      return (
        <div className="flex items-center gap-2 font-mono text-[0.9rem]">
          <RiTimeLine className="text-ink-soft text-lg shrink-0" />
          <span>{schedule.expr}</span>
          <span className="text-[0.7rem] text-ink-soft">({timezone})</span>
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
            <RiRefreshLine className="text-lg shrink-0" /> Refresh
          </Button>
        </div>
      </div>

      <div className="flex items-center gap-3 mb-4">
        <SelectBox
          aria-label="Job list view"
          value={view}
          onChange={(e: ChangeEvent<HTMLSelectElement>) => {
            setMutationError(null);
            setView(e.target.value as CronView);
          }}
          className="h-10 px-3 min-w-[160px]"
        >
          <option value="live">Live Jobs</option>
          <option value="deleted">Recycle Bin</option>
        </SelectBox>

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
          <option value="executed">Executed</option>
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

      {/* Row-level pause / resume have no modal of their own; the confirm and
          detail modals each render their own copy of this while open, since a
          banner painted here would sit under their overlay. */}
      {mutationError && !pendingDeleteId && !selected && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {mutationError}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0">
            <thead>
              <tr>
                <th className={`${thCell} w-[150px]`}>ID</th>
                <th className={`${thCell} w-[200px]`}>Title</th>
                <th className={`${thCell} w-[140px]`}>Status</th>
                <th className={`${thCell} w-[280px]`}>Schedule</th>
                <th className={`${thCell} w-[120px]`}>Channel</th>
                <th className={thCell}>Action</th>
                <th className={`${thCell} w-[160px]`}>Created At</th>
                <th className={`${thCell} w-[160px]`}>
                  {deletedView ? 'Deleted At' : 'Next Trigger'}
                </th>
                <th className={`${thCell} w-[150px] text-right`}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 && !loading && (
                <tr>
                  <td
                    colSpan={9}
                    className="px-6 py-10 text-center text-ink-soft text-[0.9rem]"
                  >
                    {deletedView ? 'Recycle bin is empty.' : 'No cron jobs found.'}
                  </td>
                </tr>
              )}
              {items.map((job) => {
                const cell = `px-6 py-4 align-middle border-b border-black`;
                const toggle = toggleActionFor(job.status);
                return (
                  <tr key={job.id} className="hover:bg-gray-50">
                    <td className={cell}>
                      <code className="font-mono text-[0.85rem]">{job.id}</code>
                    </td>
                    <td className={cell}>
                      {/* Jobs created before titles existed fall back to their prompt. */}
                      <span className="text-[0.9rem] font-bold line-clamp-1">
                        {job.title || job.prompt}
                      </span>
                    </td>
                    <td className={cell}>{statusBadge(job.status)}</td>
                    <td className={cell}>{scheduleDisplay(job.schedule, job.timezone)}</td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-bold uppercase tracking-wider">{job.channel}</span>
                    </td>
                    <td className={cell}>
                      <div className="flex items-center gap-2">
                        <RiChatSmile2Line className="text-brand shrink-0 text-lg" />
                        <span className="text-[0.9rem] line-clamp-1">{job.prompt}</span>
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug">
                        {formatTimestamp(job.created_at)}
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug">
                        {formatTimestamp(deletedView ? job.deleted_at : job.next_trigger_at)}
                      </div>
                    </td>
                    <td className={`${cell} text-right`}>
                      <div className="inline-flex gap-1">
                        <IconButton
                          aria-label="View job detail"
                          onClick={() => setSelected(job)}
                        >
                          <RiEyeLine />
                        </IconButton>
                        {deletedView ? (
                          <IconButton
                            aria-label="Restore cron job"
                            title="Restore from the recycle bin"
                            onClick={() => {
                              void handleAction(job.id, 'restore');
                            }}
                            disabled={isMock || mutating}
                            className="!border-ok !text-ok hover:!bg-ok/10"
                          >
                            <RiArrowGoBackLine />
                          </IconButton>
                        ) : (
                          <>
                            {toggle && (
                              <IconButton
                                aria-label={toggle === 'pause' ? 'Pause cron job' : 'Resume cron job'}
                                title={
                                  toggle === 'pause'
                                    ? 'Pause: stop firing until resumed'
                                    : 'Resume: next trigger recomputed from now'
                                }
                                onClick={() => {
                                  void handleAction(job.id, toggle);
                                }}
                                disabled={isMock || mutating}
                              >
                                {toggle === 'pause' ? <RiPauseLine /> : <RiPlayLine />}
                              </IconButton>
                            )}
                            <IconButton
                              aria-label="Move cron job to recycle bin"
                              title="Move to the recycle bin"
                              onClick={() => {
                                setMutationError(null);
                                setPendingDeleteId(job.id);
                              }}
                              disabled={isMock || mutating}
                              className="!border-err !text-err hover:!bg-err/10"
                            >
                              <RiDeleteBinLine />
                            </IconButton>
                          </>
                        )}
                      </div>
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
              deletedView ? 'Recycle bin is empty' : 'No cron jobs'
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

      {selected && (
        <CronDetailModal
          job={selected}
          submitting={mutating}
          error={mutationError}
          onClose={() => {
            setMutationError(null);
            setSelected(null);
          }}
          onTrash={
            selected.deleted_at
              ? undefined
              : () => {
                  setMutationError(null);
                  setPendingDeleteId(selected.id);
                }
          }
          onRestore={
            selected.deleted_at
              ? () => {
                  void handleAction(selected.id, 'restore');
                }
              : undefined
          }
        />
      )}
      {pendingDeleteId && (
        <TrashConfirmModal
          id={pendingDeleteId}
          submitting={mutating}
          error={mutationError}
          onCancel={() => {
            setPendingDeleteId(null);
            setMutationError(null);
          }}
          onConfirm={() => {
            void handleDelete(pendingDeleteId);
          }}
        />
      )}
    </div>
  );
}

function CronDetailModal({
  job,
  submitting,
  error,
  onClose,
  onTrash,
  onRestore,
}: {
  job: CronJob;
  submitting: boolean;
  error: string | null;
  onClose: () => void;
  onTrash?: () => void;
  onRestore?: () => void;
}) {
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
        <header className="flex items-center justify-between gap-6 px-6 py-4 border-b-2 border-black">
          <div className="flex items-center gap-3 min-w-0">
            <h3 className="font-bold uppercase tracking-wider shrink-0">Cron Job Detail</h3>
            <code className="font-mono text-[0.9rem] bg-gray-100 px-2 py-0.5 rounded border border-black truncate">{job.id}</code>
          </div>
          <div className="flex items-center gap-4 shrink-0">
            {onRestore && (
              <button
                type="button"
                onClick={onRestore}
                disabled={submitting}
                className="text-[0.85rem] font-bold uppercase tracking-wider text-ok hover:text-ok/80 cursor-pointer inline-flex items-center gap-1"
              >
                <RiArrowGoBackLine className="text-base" /> Restore
              </button>
            )}
            {onTrash && (
              <button
                type="button"
                onClick={onTrash}
                className="text-[0.85rem] font-bold uppercase tracking-wider text-err hover:text-err/80 cursor-pointer inline-flex items-center gap-1"
              >
                <RiDeleteBinLine className="text-base" /> Move to Bin
              </button>
            )}
            <button
              type="button"
              onClick={onClose}
              className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
            >
              Close
            </button>
          </div>
        </header>
        <div className="px-6 py-4 space-y-4">
          {error && (
            <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
              {error}
            </div>
          )}
          <div className="grid grid-cols-2 gap-4">
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">User ID</label>
              <div className="font-mono text-[0.9rem] break-all">{job.user_id}</div>
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
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Timezone</label>
              <div className="font-mono text-[0.9rem]">{job.timezone}</div>
            </div>
            {job.deleted_at && (
              <div>
                <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Deleted At</label>
                <div className="text-[0.9rem]">{formatTimestamp(job.deleted_at)}</div>
              </div>
            )}
          </div>

          <div>
            <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Prompt</label>
            <div className="bg-gray-50 border-2 border-black rounded-md px-4 py-3 font-mono text-[0.9rem]">
              {job.prompt}
            </div>
          </div>

          {job.origin_session_id && (
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">Origin Session</label>
              <code className="font-mono text-[0.85rem] break-all">{job.origin_session_id}</code>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

function TrashConfirmModal({
  id,
  submitting,
  error,
  onCancel,
  onConfirm,
}: {
  id: string;
  submitting: boolean;
  error: string | null;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onCancel}
    >
      <div
        className="max-w-md w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden"
        onClick={(e) => e.stopPropagation()}
      >
        <header className="px-6 py-4 border-b-2 border-black">
          <h3 className="font-bold uppercase tracking-wider">Move to Recycle Bin</h3>
        </header>
        <div className="px-6 py-4 space-y-3">
          <p className="text-[0.95rem]">
            Move cron job{' '}
            <code className="font-mono text-[0.85rem] bg-gray-100 px-1 rounded border border-black">
              {id}
            </code>
            {' '}to the recycle bin? It stops firing and leaves this list, but the record is kept —
            you can restore it from the Recycle Bin view.
          </p>
          {error && (
            <div className="bg-white border-[2px] border-err text-err rounded-md px-3 py-2 font-mono text-[0.85rem]">
              {error}
            </div>
          )}
        </div>
        <footer className="flex justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
          <Button
            type="button"
            onClick={onCancel}
            disabled={submitting}
            className="!py-1 !px-3 !text-[0.85rem] h-9"
          >
            Cancel
          </Button>
          <Button
            type="button"
            onClick={onConfirm}
            disabled={submitting}
            className="!py-1 !px-3 !text-[0.85rem] h-9 gap-1.5 !bg-err !text-white !border-err hover:!bg-err/90"
          >
            {submitting && <RiLoader4Line className="animate-spin text-base shrink-0" />}
            <RiDeleteBinLine className="text-base shrink-0" /> Move to Bin
          </Button>
        </footer>
      </div>
    </div>
  );
}
