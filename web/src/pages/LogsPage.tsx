import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  RiAlertFill,
  RiBug2Fill,
  RiBroadcastLine,
  RiCloseCircleFill,
  RiDownloadLine,
  RiEyeLine,
  RiInformationFill,
  RiPulseLine,
  RiRefreshLine,
} from 'react-icons/ri';
import type { IconType } from 'react-icons';
import { Button } from '../components/Button';
import { IconButton } from '../components/IconButton';
import { SearchBox } from '../components/SearchBox';
import { SelectBox } from '../components/SelectBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';

type LogEntry = components['schemas']['LogEntry'];
type ApiLogLevel = components['schemas']['LogLevel'];

const PAGE_SIZE = 50;

const LEVEL_META: Record<ApiLogLevel, { className: string; Icon: IconType }> = {
  error: { className: 'bg-err text-white', Icon: RiCloseCircleFill },
  warn: { className: 'bg-warn text-white', Icon: RiAlertFill },
  info: { className: 'bg-info text-white', Icon: RiInformationFill },
  debug: { className: 'bg-gray-200 text-ink border-ink', Icon: RiBug2Fill },
  trace: { className: 'bg-gray-100 text-ink-soft border-ink-soft', Icon: RiPulseLine },
};

const thCell =
  'px-6 py-4 text-left font-bold text-[0.85rem] uppercase tracking-wider border-b-2 border-black';

function splitTimestamp(iso: string): { date: string; time: string } {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { date: iso, time: '' };
  return {
    date: d.toISOString().slice(0, 10),
    time: d.toISOString().slice(11, 23),
  };
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
  const client = useAdminClient();
  const { token, baseUrl, logout } = useAuth();

  const [filter, setFilter] = useState('');
  const [debouncedFilter, setDebouncedFilter] = useState('');
  const [level, setLevel] = useState<'all' | ApiLogLevel>('all');
  const [last24h, setLast24h] = useState(true);
  const [offset, setOffset] = useState(0);
  const [items, setItems] = useState<LogEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<LogEntry | null>(null);
  const [live, setLive] = useState(false);
  const [liveConnected, setLiveConnected] = useState(false);

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
  }, [debouncedFilter, level, last24h]);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    const query: Record<string, string | number> = {
      limit: PAGE_SIZE,
      offset,
    };
    if (level !== 'all') query.level = level;
    if (debouncedFilter.trim()) query.q = debouncedFilter.trim();
    if (last24h) {
      query.since = new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString();
    }
    try {
      const { data, error: apiError, response } = await client.GET('/v1/logs', {
        params: { query },
      });
      if (apiError) {
        setError(apiError.error);
        if (response.status === 401) logout();
        return;
      }
      setItems(data?.items ?? []);
      setTotal(data?.total ?? 0);
    } catch (e) {
      setError(
        e instanceof Error
          ? `Network error: ${e.message}`
          : 'Network error contacting gateway',
      );
    } finally {
      setLoading(false);
    }
  }, [client, debouncedFilter, last24h, level, logout, offset]);

  useEffect(() => {
    void load();
  }, [load]);

  // Live tail: subscribe to /v1/logs/stream while Live is on and we're
  // on page 1. EventSource can't carry an Authorization header, so the
  // token rides as a query param (the admin middleware accepts
  // `?token=...` as a fallback). Re-opened whenever the filter changes.
  useEffect(() => {
    setLiveConnected(false);
    if (!live || offset !== 0 || !token) return;

    const params = new URLSearchParams();
    params.set('token', token);
    if (level !== 'all') params.set('level', level);
    if (debouncedFilter.trim()) params.set('q', debouncedFilter.trim());
    if (last24h) {
      params.set(
        'since',
        new Date(Date.now() - 24 * 60 * 60 * 1000).toISOString(),
      );
    }
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
          return next.length > PAGE_SIZE ? next.slice(0, PAGE_SIZE) : next;
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
  }, [live, offset, token, baseUrl, level, debouncedFilter, last24h]);

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

  const badge = useMemo(
    () =>
      ({ level: lvl }: { level: ApiLogLevel }) => {
        const { className, Icon } = LEVEL_META[lvl];
        return (
          <span
            className={`inline-flex items-center gap-1.5 px-2.5 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${className}`}
          >
            <Icon className="text-[0.85rem]" />
            {lvl}
          </span>
        );
      },
    [],
  );

  return (
    <div className="p-8">
      <div className="flex justify-between items-start mb-8">
        <div>
          <h2 className="text-[2.5rem] font-bold uppercase -tracking-[0.05em] mb-2">
            SYSTEM LOGS
          </h2>
          <p className="text-ink-soft">Real-time event and error tracking.</p>
        </div>
        <Button variant="primary" onClick={handleExport} disabled={items.length === 0}>
          <RiDownloadLine /> Export
        </Button>
      </div>

      <div className="flex items-center gap-3 mb-6">
        <SearchBox
          placeholder="Filter by message or source..."
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
        <SelectBox
          value={level}
          onChange={(e) => setLevel(e.target.value as 'all' | ApiLogLevel)}
        >
          <option value="all">All Levels</option>
          <option value="error">Error</option>
          <option value="warn">Warn</option>
          <option value="info">Info</option>
          <option value="debug">Debug</option>
          <option value="trace">Trace</option>
        </SelectBox>
        <Button
          variant={last24h ? 'primary' : 'default'}
          onClick={() => setLast24h((v) => !v)}
          aria-pressed={last24h}
        >
          Last 24 Hours
        </Button>
        <Button
          variant={live ? 'primary' : 'default'}
          onClick={() => setLive((v) => !v)}
          aria-pressed={live}
          disabled={offset !== 0}
          title={
            offset !== 0
              ? 'Return to page 1 to enable live tail'
              : 'Stream new log records as they arrive'
          }
        >
          <RiBroadcastLine
            className={live && liveConnected ? 'animate-pulse' : undefined}
          />
          {live ? (liveConnected ? 'Live' : 'Connecting…') : 'Live'}
        </Button>
        <Button onClick={() => void load()} disabled={loading}>
          <RiRefreshLine /> Refresh
        </Button>
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm">
          {error}
        </div>
      )}

      <div className="bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden">
        <table className="w-full border-collapse">
          <thead>
            <tr>
              <th className={thCell}>Timestamp</th>
              <th className={thCell}>Level</th>
              <th className={thCell}>Source</th>
              <th className={thCell}>Message</th>
              <th className={`${thCell} text-right`}>Action</th>
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
            {items.map((log, idx) => {
              const { date, time } = splitTimestamp(log.timestamp);
              const notLast = idx !== items.length - 1;
              const cell = `px-6 py-4 align-top ${notLast ? 'border-b border-black' : ''}`;
              return (
                <tr key={log.id} className="hover:bg-gray-50">
                  <td className={cell}>
                    <div className="text-ink-soft text-[0.85rem] leading-snug">
                      {date}
                      <br />
                      {time}
                    </div>
                  </td>
                  <td className={cell}>{badge({ level: log.level })}</td>
                  <td className={cell}>
                    <code className="font-mono text-[0.9rem]">{log.target}</code>
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

        <div className="flex justify-between items-center px-6 py-4 border-t-2 border-black bg-white">
          <span className="text-[0.85rem] text-ink-soft">
            {total === 0
              ? loading
                ? 'Loading...'
                : 'No logs'
              : `Showing ${pageStart} to ${pageEnd} of ${total.toLocaleString()} logs`}
          </span>
          <div className="flex gap-2">
            <Button
              onClick={() => setOffset((o) => Math.max(0, o - PAGE_SIZE))}
              disabled={!hasPrev || loading}
            >
              Prev
            </Button>
            <Button
              onClick={() => setOffset((o) => o + PAGE_SIZE)}
              disabled={!hasNext || loading}
            >
              Next
            </Button>
          </div>
        </div>
      </div>

      {selected && <LogDetailModal entry={selected} onClose={() => setSelected(null)} />}
    </div>
  );
}

function LogDetailModal({ entry, onClose }: { entry: LogEntry; onClose: () => void }) {
  const { date, time } = splitTimestamp(entry.timestamp);
  const { className, Icon } = LEVEL_META[entry.level];
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
              className={`inline-flex items-center gap-1.5 px-2.5 py-1 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black shadow-brutal-xs ${className}`}
            >
              <Icon className="text-[0.85rem]" />
              {entry.level}
            </span>
            <code className="font-mono text-[0.9rem]">{entry.target}</code>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink"
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
