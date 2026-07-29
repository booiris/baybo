import { useEffect, useMemo, useRef, useState } from 'react';
import { useSearchParams, useNavigate } from 'react-router-dom';
import { RiLoader4Line, RiRefreshLine } from 'react-icons/ri';
import { Button } from '../components/Button';
import { SearchBox } from '../components/SearchBox';
import { SelectBox } from '../components/SelectBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';
import { useMockMode, MOCK_TRACE_SUMMARIES } from '../api/mock';

type TraceSessionSummary = components['schemas']['TraceSessionSummary'];
type TurnStatusKind = components['schemas']['TurnStatusKind'];
type SessionKind = components['schemas']['SessionKind'];

const PAGE_SIZE_OPTIONS = [20, 50, 100];
const DEFAULT_PAGE_SIZE = 20;

// Smart-poll cadence: only re-fetch when the visible page contains an
// in-flight session. Static pages (everything terminal) just sit.
const ACTIVE_POLL_MS = 5_000;

const STATUS_OPTIONS: { value: 'all' | TurnStatusKind; label: string }[] = [
  { value: 'all', label: 'All Statuses' },
  { value: 'in_progress', label: 'In Progress' },
  { value: 'pending', label: 'Pending' },
  { value: 'stuck', label: 'Stuck' },
  { value: 'completed', label: 'Completed' },
  { value: 'failed', label: 'Failed' },
  { value: 'cancelled', label: 'Cancelled' },
];

const KIND_OPTIONS: { value: 'all' | SessionKind; label: string }[] = [
  { value: 'all', label: 'All Kinds' },
  { value: 'user', label: 'User' },
  { value: 'subagent', label: 'Subagent' },
  { value: 'cron', label: 'Cron' },
];

const STATUS_BADGE_CLASS: Record<TurnStatusKind, string> = {
  pending: 'bg-gray-200 text-ink border-ink',
  in_progress: 'bg-info text-white',
  stuck: 'bg-warn text-white',
  completed: 'bg-ok text-white',
  failed: 'bg-err text-white',
  cancelled: 'bg-gray-300 text-ink border-ink-soft',
};

const KIND_BADGE_CLASS: Record<SessionKind, string> = {
  user: 'bg-white text-ink',
  cron: 'bg-warn/15 text-warn',
  subagent: 'bg-brand/10 text-brand',
};

const KIND_BADGE_LABEL: Record<SessionKind, string> = {
  user: 'user',
  cron: 'cron',
  subagent: 'subagent',
};

// Only genuinely in-flight kinds keep the list poll alive. `stuck` is
// excluded on purpose: a stuck row never advances on its own, so polling
// for it would run forever with nothing to observe.
const ACTIVE_KINDS: TurnStatusKind[] = ['pending', 'in_progress'];

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

function splitTimestamp(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: iso, time: '' };
  return {
    date: d.toLocaleDateString('sv-SE'),
    time: d.toLocaleTimeString([], {
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hour12: false,
    }),
  };
}

function formatNumber(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}

function applyMockFilters(
  rows: TraceSessionSummary[],
  status: 'all' | TurnStatusKind,
  kind: 'all' | SessionKind,
  query: string,
  since?: string,
  until?: string,
): TraceSessionSummary[] {
  let out = rows;
  if (status !== 'all') {
    out = out.filter((r) => r.latest_turn_status?.kind === status);
  }
  if (kind !== 'all') {
    out = out.filter((r) => r.kind === kind);
  }
  if (query) {
    const needle = query.toLowerCase();
    out = out.filter((r) => r.session_id.toLowerCase().includes(needle));
  }
  if (since) {
    const t = new Date(since).getTime();
    out = out.filter((r) => new Date(r.last_active).getTime() >= t);
  }
  if (until) {
    const t = new Date(until).getTime();
    out = out.filter((r) => new Date(r.last_active).getTime() < t);
  }
  return out;
}

function KindBadge({ kind }: { kind: SessionKind }) {
  return (
    <span
      className={`inline-flex items-center px-1.5 py-0.5 rounded-md text-[0.65rem] font-bold uppercase tracking-wider border-2 border-black ${KIND_BADGE_CLASS[kind]}`}
    >
      {KIND_BADGE_LABEL[kind]}
    </span>
  );
}

function StatusBadge({ status }: { status?: components['schemas']['TurnStatus'] | null }) {
  if (!status) {
    return (
      <span className="inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs bg-gray-100 text-ink-soft">
        no turns
      </span>
    );
  }
  const cls = STATUS_BADGE_CLASS[status.kind] ?? 'bg-gray-100 text-ink';
  return (
    <span
      className={`inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${cls}`}
      title={status.reason ?? status.cancel_reason ?? undefined}
    >
      {status.kind.replace('_', ' ')}
    </span>
  );
}

export function TracesPage() {
  const isMock = useMockMode();
  const [searchParams, setSearchParams] = useSearchParams();
  const navigate = useNavigate();
  const client = useAdminClient();
  const { logout } = useAuth();

  const [filter, setFilter] = useState('');
  const [debouncedFilter, setDebouncedFilter] = useState('');
  const [status, setStatus] = useState<'all' | TurnStatusKind>('all');
  const [kind, setKind] = useState<'all' | SessionKind>('all');
  const [since, setSince] = useState<string>('');
  const [until, setUntil] = useState<string>('');

  const [offset, setOffset] = useState(0);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);
  const [items, setItems] = useState<TraceSessionSummary[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  // Debounce the session-id filter.
  useEffect(() => {
    const handle = window.setTimeout(() => setDebouncedFilter(filter), 250);
    return () => window.clearTimeout(handle);
  }, [filter]);

  // Filter changes reset to page 1.
  const isFirstRender = useRef(true);
  useEffect(() => {
    if (isFirstRender.current) {
      isFirstRender.current = false;
      return;
    }
    setOffset(0);
  }, [debouncedFilter, status, kind, pageSize, since, until]);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        const filtered = applyMockFilters(
          MOCK_TRACE_SUMMARIES,
          status,
          kind,
          debouncedFilter.trim(),
          since || undefined,
          until || undefined,
        );
        setItems(filtered.slice(offset, offset + pageSize));
        setTotal(filtered.length);
        setLoading(false);
        setError(null);
        return;
      }

      setLoading(true);
      setError(null);
      const query: Record<string, string | number> = {
        limit: pageSize,
        offset,
      };
      if (status !== 'all') query.status = status;
      if (kind !== 'all') query.kind = kind;
      if (debouncedFilter.trim()) query.q = debouncedFilter.trim();
      if (since) query.since = new Date(since).toISOString();
      if (until) query.until = new Date(until).toISOString();

      try {
        const { data, error: apiError, response } = await client.GET('/v1/traces', {
          params: { query },
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
        setItems(data?.items ?? []);
        setTotal(data?.total ?? 0);
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
  }, [
    client,
    debouncedFilter,
    status,
    kind,
    since,
    until,
    logout,
    offset,
    pageSize,
    refreshKey,
    isMock,
  ]);

  // Smart polling: tick every ACTIVE_POLL_MS only when at least one
  // visible row is in a non-terminal state, and only while the tab is
  // visible.
  const hasActive = useMemo(
    () =>
      items.some(
        (it) =>
          it.latest_turn_status &&
          ACTIVE_KINDS.includes(it.latest_turn_status.kind),
      ),
    [items],
  );

  useEffect(() => {
    if (isMock || !hasActive) return;
    let timer: number | undefined;
    const tick = () => {
      if (document.visibilityState === 'visible') {
        setRefreshKey((k) => k + 1);
      }
    };
    timer = window.setInterval(tick, ACTIVE_POLL_MS);
    return () => {
      if (timer !== undefined) window.clearInterval(timer);
    };
  }, [hasActive, isMock]);

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
    setOffset(0);
  };

  const buildDetailHref = (id: string) => {
    const next = new URLSearchParams(searchParams);
    return `/traces/${encodeURIComponent(id)}${next.toString() ? `?${next}` : ''}`;
  };

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">TRACES</h2>
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

      <div className="flex items-center gap-3 mb-4 flex-wrap">
        <SearchBox
          placeholder="Filter by session id..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="h-10"
        />
        <SelectBox
          value={status}
          onChange={(e) => setStatus(e.target.value as 'all' | TurnStatusKind)}
          className="h-10 px-3"
        >
          {STATUS_OPTIONS.map((opt) => (
            <option key={opt.value} value={opt.value}>
              {opt.label}
            </option>
          ))}
        </SelectBox>
        <SelectBox
          value={kind}
          onChange={(e) => setKind(e.target.value as 'all' | SessionKind)}
          className="h-10 px-3"
        >
          {KIND_OPTIONS.map((opt) => (
            <option key={opt.value} value={opt.value}>
              {opt.label}
            </option>
          ))}
        </SelectBox>
        <label className="flex items-center gap-2 text-[0.85rem] uppercase font-bold tracking-wider text-ink-soft">
          From
          <input
            type="datetime-local"
            value={since}
            onChange={(e) => setSince(e.target.value)}
            className="border-2 border-black rounded-md px-2 h-10 font-mono text-[0.85rem] bg-white"
          />
        </label>
        <label className="flex items-center gap-2 text-[0.85rem] uppercase font-bold tracking-wider text-ink-soft">
          To
          <input
            type="datetime-local"
            value={until}
            onChange={(e) => setUntil(e.target.value)}
            className="border-2 border-black rounded-md px-2 h-10 font-mono text-[0.85rem] bg-white"
          />
        </label>
        {(since || until) && (
          <Button
            onClick={() => {
              setSince('');
              setUntil('');
            }}
            className="!py-1 !px-3 !text-[0.8rem] h-10"
          >
            Clear range
          </Button>
        )}
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0 table-fixed">
            <thead>
              <tr>
                <th className={`${thCell} w-[160px]`}>Created</th>
                <th className={`${thCell} w-[160px]`}>Last Active</th>
                <th className={`${thCell}`}>Session ID</th>
                <th className={`${thCell} w-[140px]`}>Status</th>
                <th className={`${thCell} w-[110px]`}>Spans</th>
                <th className={`${thCell} w-[130px] whitespace-nowrap`}>Tok In</th>
                <th className={`${thCell} w-[130px] whitespace-nowrap`}>Tok Out</th>
                <th className={`${thCell} w-[130px] whitespace-nowrap`}>Cached</th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 && !loading && (
                <tr>
                  <td colSpan={7} className="px-6 py-10 text-center text-ink-soft text-[0.9rem]">
                    No sessions found.
                  </td>
                </tr>
              )}
              {items.map((row) => {
                const created = splitTimestamp(row.created_at);
                const active = splitTimestamp(row.last_active);
                const cell = `px-6 py-4 align-middle border-b border-black`;
                return (
                  <tr
                    key={row.session_id}
                    className="hover:bg-gray-50 cursor-pointer group"
                    onClick={() => navigate(buildDetailHref(row.session_id))}
                  >
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug whitespace-nowrap">
                        {created.date}
                        <br />
                        {created.time}
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="text-ink-soft text-[0.85rem] leading-snug whitespace-nowrap">
                        {active.date}
                        <br />
                        {active.time}
                      </div>
                    </td>
                    <td className={cell}>
                      <div className="flex items-center gap-2 flex-wrap">
                        <code className="font-mono text-[0.9rem] break-all group-hover:text-brand">
                          {row.session_id}
                        </code>
                        <KindBadge kind={row.kind} />
                      </div>
                      <div className="text-ink-soft text-[0.75rem] mt-1">
                        {row.turn_count} {row.turn_count === 1 ? 'turn' : 'turns'}
                      </div>
                    </td>
                    <td className={cell}>
                      <StatusBadge status={row.latest_turn_status} />
                    </td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-mono">{row.span_count}</span>
                    </td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-mono text-ink-soft">
                        {formatNumber(row.input_tokens)}
                      </span>
                    </td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-mono text-ink-soft">
                        {formatNumber(row.output_tokens)}
                      </span>
                    </td>
                    <td className={cell}>
                      <span className="text-[0.9rem] font-mono text-ink-soft">
                        {formatNumber(row.cached_input_tokens)}
                        {row.cache_creation_input_tokens > 0 && (
                          <span className="text-[0.75rem] ml-1">
                            (+{formatNumber(row.cache_creation_input_tokens)})
                          </span>
                        )}
                      </span>
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
                <RiLoader4Line className="animate-spin" /> Loading sessions...
              </span>
            ) : total === 0 ? (
              'No sessions'
            ) : (
              <>
                Showing {pageStart} to {pageEnd} of {total.toLocaleString()} sessions
                {hasActive && !isMock && (
                  <span className="ml-2 text-info font-bold">• live</span>
                )}
              </>
            )}
          </span>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <span className="text-[0.85rem] text-ink-soft whitespace-nowrap">Per page:</span>
              <SelectBox
                value={pageSize}
                onChange={(e) => setPageSize(Number(e.target.value))}
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
    </div>
  );
}
