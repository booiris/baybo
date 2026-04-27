import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import {
  RiBroadcastLine,
  RiDownloadLine,
  RiEyeLine,
  RiRefreshLine,
  RiLoader4Line,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { SearchBox } from '../components/SearchBox';
import { SelectBox } from '../components/SelectBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';
import { useMockMode, MOCK_LOGS } from '../api/mock';

type LogEntry = components['schemas']['LogEntry'];
type ApiLogLevel = components['schemas']['LogLevel'];

// Default page size for logs
const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [20, 50, 100, 200, 500];

const LEVEL_META: Record<ApiLogLevel, { className: string }> = {
  error: { className: 'bg-err text-white' },
  warn: { className: 'bg-warn text-white' },
  info: { className: 'bg-info text-white' },
  debug: { className: 'bg-gray-200 text-ink border-ink' },
  trace: { className: 'bg-gray-100 text-ink-soft border-ink-soft' },
};

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black sticky top-0 z-10 bg-white';

function splitTimestamp(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: iso, time: '' };
  
  // Format as YYYY-MM-DD in local time
  const date = d.toLocaleDateString('sv-SE'); 
  // Format as HH:mm:ss in local time
  const time = d.toLocaleTimeString([], { 
    hour: '2-digit', 
    minute: '2-digit', 
    second: '2-digit',
    hour12: false 
  });

  return { date, time };
}

function triggerDownload(filename: string, mime: string, body: string): void {
  const blob = new Blob([body], { type: mime });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}

export function LogsPage() {
  const isMock = useMockMode();
  const [searchParams, setSearchParams] = useSearchParams();
  const client = useAdminClient();
  const { token, baseUrl, logout } = useAuth();

  const [filter, setFilter] = useState('');
  const [debouncedFilter, setDebouncedFilter] = useState('');
  const [level, setLevel] = useState<'all' | ApiLogLevel>('all');
  const [offset, setOffset] = useState(0);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);
  const [items, setItems] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<LogEntry | null>(null);
  const [live, setLive] = useState(true);
  const [liveConnected, setLiveConnected] = useState(false);
  const scrollRef = useRef<HTMLDivElement>(null);

  // Debounce the filter input so we don't hammer the gateway on every keystroke.
  useEffect(() => {
    const handle = window.setTimeout(() => setDebouncedFilter(filter), 250);
    return () => window.clearTimeout(handle);
  }, [filter]);

  // Any non-pagination filter change should reset to page 1.
  const isFirstRender = useRef(true);
  useEffect(() => {
    if (isFirstRender.current) {
      isFirstRender.current = false;
      return;
    }
    setOffset(0);
  }, [debouncedFilter, level, pageSize]);

  // For the manual refresh button, we'll use a refresh counter.
  const [refreshKey, setRefreshKey] = useState(0);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        setItems(MOCK_LOGS.slice(offset, offset + pageSize));
        setTotal(MOCK_LOGS.length);
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
      if (level !== 'all') query.level = level;
      if (debouncedFilter.trim()) query.q = debouncedFilter.trim();
      query.since = new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString();

      try {
        const { data, error: apiError, response } = await client.GET('/v1/logs', {
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
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (!canceled) setLoading(false);
      }
    }
    void fetchData();
    return () => { canceled = true; };
  }, [client, debouncedFilter, level, logout, offset, pageSize, refreshKey, isMock]);

  // Reset scroll on page change
  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = 0;
    }
  }, [offset, pageSize]);

  // Live tail: subscribe to /v1/logs/stream while Live is on and we're
  // on page 1. EventSource can't carry an Authorization header, so the
  // token rides as a query param (the admin middleware accepts
  // `?token=...` as a fallback). Re-opened whenever the filter changes.
  useEffect(() => {
    setLiveConnected(false);
    if (isMock || !live || offset !== 0 || !token) return;

    const params = new URLSearchParams();
    params.set('token', token);
    if (level !== 'all') params.set('level', level);
    if (debouncedFilter.trim()) params.set('q', debouncedFilter.trim());
    params.set(
      'since',
      new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString(),
    );
    const base = baseUrl.replace(/\/$/, '');
    const url = `${base}/v1/logs/stream?${params.toString()}`;
    const es = new EventSource(url);

    es.addEventListener('open', () => setLiveConnected(true));

    es.addEventListener('log', (evt) => {
      try {
        const entry = JSON.parse((evt as MessageEvent).data) as LogEntry;
        setItems((prev) => {
          if (prev.some((e) => e.id === entry.id)) return prev;
          const next = [entry, ...prev];
          return next.length > pageSize ? next.slice(0, pageSize) : next;
        });
        setTotal((t) => t + 1);
      } catch {
        // Malformed payload — skip rather than tear the stream down.
      }
    });

    es.addEventListener('lagged', (evt) => {
      const dropped = (evt as MessageEvent).data;
      setError(
        `Live stream lagged behind — ${dropped} events were dropped. Refresh to resync.`,
      );
    });

    es.addEventListener('error', () => {
      setLiveConnected(false);
      // Browsers auto-reconnect EventSource on transient network
      // failures, so we don't tear it down here — the `open` handler
      // will flip `liveConnected` back on.
    });

    return () => {
      es.close();
      setLiveConnected(false);
    };
  }, [live, offset, token, baseUrl, level, debouncedFilter, isMock, pageSize]);

  // Pagination away from page 1 implicitly drops the user out of live
  // mode — the stream only makes sense at the head of the list.
  useEffect(() => {
    if (offset !== 0 && live) setLive(false);
  }, [offset, live]);

  const pageStart = items.length === 0 ? 0 : offset + 1;
  const pageEnd = offset + items.length;
  const hasPrev = offset > 0;
  const hasNext = pageEnd < total;

  const handleExport = useCallback(() => {
    const body = JSON.stringify(items, null, 2);
    const stamp = new Date().toISOString().replace(/[:.]/g, '-');
    triggerDownload(`aura-logs-${stamp}.json`, 'application/json', body);
  }, [items]);

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

  const badge = useMemo(
    () =>
      ({ level: lvl }: { level: ApiLogLevel }) => {
        const { className } = LEVEL_META[lvl];
        return (
          <span
            className={`inline-flex items-center px-2 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${className}`}
          >
            <span>{lvl}</span>
          </span>
        );
      },
    [],
  );

  return (
    <div className="p-5 h-full flex flex-col overflow-hidden">
      <div className="flex justify-between items-start mb-3">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">
            SYSTEM LOGS
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
            variant="primary" 
            onClick={handleExport} 
            disabled={items.length === 0}
            className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
          >
            <RiDownloadLine className="text-base" /> Export
          </Button>
        </div>
      </div>

      <div className="flex items-center gap-3 mb-4">
        <SearchBox
          placeholder="Filter by message or source..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="h-10"
        />
        <SelectBox
          value={level}
          onChange={(e) => setLevel(e.target.value as 'all' | ApiLogLevel)}
          className="h-10 px-3"
        >
          <option value="all">All Levels</option>
          <option value="error">Error</option>
          <option value="warn">Warn</option>
          <option value="info">Info</option>
          <option value="debug">Debug</option>
          <option value="trace">Trace</option>
        </SelectBox>

        <Button
          variant={live ? 'primary' : 'default'}
          onClick={() => setLive((v) => !v)}
          aria-pressed={live}
          disabled={offset !== 0 || isMock}
          className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
          title={
            isMock 
              ? 'Live tail disabled in mock mode'
              : offset !== 0
              ? 'Return to page 1 to enable live tail'
              : 'Stream new log records as they arrive'
          }
        >
          {live && !liveConnected && !isMock ? (
            <RiLoader4Line className="animate-spin text-base" />
          ) : (
            <RiBroadcastLine
              className={`text-base ${live && liveConnected ? 'animate-pulse' : ''}`}
            />
          )}
          Live
        </Button>
        <Button 
          onClick={() => setRefreshKey((k) => k + 1)} 
          disabled={loading || isMock}
          className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
        >
          <RiRefreshLine className="text-base" /> Refresh
        </Button>
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      )}

      <div className="flex-1 flex flex-col min-h-0 bg-white border-[3px] border-black rounded-md shadow-brutal">
        <div ref={scrollRef} className="flex-1 overflow-auto overscroll-none">
          <table className="w-full border-separate border-spacing-0">
          <thead>
            <tr>
              <th className={`${thCell} w-[160px]`}>Timestamp</th>
              <th className={`${thCell} w-[160px]`}>Level</th>
              <th className={`${thCell} w-[400px]`}>Source</th>
              <th className={thCell}>Message</th>
              <th className={`${thCell} w-[100px] text-right`}>Action</th>
            </tr>
          </thead>
          <tbody>
            {items.length === 0 && !loading && (
              <tr>
                <td
                  colSpan={5}
                  className="px-6 py-10 text-center text-ink-soft text-[0.9rem]"
                >
                  No logs match the current filters.
                </td>
              </tr>
            )}
            {items.map((log) => {
              const { date, time } = splitTimestamp(log.timestamp);
              const cell = `px-6 py-4 align-middle border-b border-black`;
              return (
                <tr key={log.id} className="hover:bg-gray-50">
                  <td className={cell}>
                    <div className="text-ink-soft text-[0.85rem] leading-snug whitespace-nowrap">
                      {date}
                      <br />
                      {time}
                    </div>
                  </td>
                  <td className={cell}>{badge({ level: log.level })}</td>
                  <td className={cell}>
                    <code className="font-mono text-[0.9rem] break-all">{log.target}</code>
                  </td>
                  <td className={cell}>
                    <span className="text-[0.9rem] break-words">{log.message}</span>
                  </td>
                  <td className={`${cell} text-right`}>
                    <IconButton
                      aria-label="View log detail"
                      onClick={() => setSelected(log)}
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
                <RiLoader4Line className="animate-spin" /> Loading logs...
              </span>
            ) : total === 0 ? (
              'No logs'
            ) : (
              `Showing ${pageStart} to ${pageEnd} of ${total.toLocaleString()} logs`
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

      {selected && <LogDetailModal entry={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function LogDetailModal({ entry, onClose }: { entry: LogEntry; onClose: () => void }) {
  const { date, time } = splitTimestamp(entry.timestamp);
  const { className } = LEVEL_META[entry.level];
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
            <span
              className={`inline-flex items-center px-2.5 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${className}`}
            >
              {entry.level}
            </span>
            <code className="font-mono text-[0.9rem]">{entry.target}</code>
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
          <div className="text-ink-soft text-[0.85rem] font-mono">
            {date} {time}
          </div>
          <pre className="whitespace-pre-wrap break-words font-mono text-[0.9rem] bg-gray-50 border-2 border-black rounded-md px-4 py-3">
            {entry.message}
          </pre>
          {entry.fields.length > 0 && (
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 font-mono text-[0.85rem]">
              {entry.fields.map((f, i) => (
                <div key={`${f.name}-${i}`} className="contents">
                  <dt className="font-bold text-ink-soft">{f.name}</dt>
                  <dd className="break-words">{f.value}</dd>
                </div>
              ))}
            </dl>
          )}
        </div>
      </div>
    </div>
  );
}
