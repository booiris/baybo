import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  RiAlarmLine,
  RiArrowDownSLine,
  RiArrowUpSLine,
  RiLoader4Line,
  RiNotification3Line,
  RiRefreshLine,
} from 'react-icons/ri';

import { useAdminClient } from '../api/auth';
import type { components } from '../api/schema';

type CronMessage = components['schemas']['ChatCronMessage'];

interface CronInboxProps {
  /** Bumped externally to force a refetch — used when the chat WS reports
   *  activity for a session_id the main list doesn't know about (the
   *  likely shape of a cron fire that just minted a fresh session). */
  refreshSignal: number;
}

/** Background refresh interval. Cron tick latency is bounded by the
 *  scheduler's 10s tick, so 30s keeps the panel within roughly one
 *  fire's worth of staleness without the gateway answering an empty
 *  list every few seconds. */
const POLL_INTERVAL_MS = 30_000;

export function CronInbox({ refreshSignal }: CronInboxProps) {
  const client = useAdminClient();
  const [items, setItems] = useState<CronMessage[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const reqGenRef = useRef(0);

  const fetchMessages = useCallback(async () => {
    const gen = ++reqGenRef.current;
    try {
      const { data, error: apiError, response } = await client.GET(
        '/v1/chat/cron-messages',
        {},
      );
      if (gen !== reqGenRef.current) return;
      if (apiError || !response.ok) {
        setError(apiError?.error ?? `HTTP ${response.status}`);
        return;
      }
      setError(null);
      setItems(data?.items ?? []);
    } catch (e) {
      if (gen !== reqGenRef.current) return;
      setError(e instanceof Error ? e.message : 'network error');
    } finally {
      if (gen === reqGenRef.current) setLoading(false);
    }
  }, [client]);

  useEffect(() => {
    void fetchMessages();
    const handle = window.setInterval(() => {
      void fetchMessages();
    }, POLL_INTERVAL_MS);
    return () => window.clearInterval(handle);
  }, [fetchMessages]);

  useEffect(() => {
    if (refreshSignal === 0) return;
    void fetchMessages();
  }, [refreshSignal, fetchMessages]);

  const toggle = useCallback((sessionId: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(sessionId)) {
        next.delete(sessionId);
      } else {
        next.add(sessionId);
      }
      return next;
    });
  }, []);

  const unreadCount = items.length;

  return (
    <aside className="hidden xl:flex flex-col w-[260px] border-l-2 border-black bg-white absolute right-0 top-12 bottom-0 z-10">
      <header className="px-3 py-3 border-b-2 border-black flex items-center gap-2">
        <RiNotification3Line className="text-lg shrink-0" />
        <span className="font-bold uppercase tracking-wider text-[0.85rem] flex-1">
          Cron Inbox
        </span>
        {unreadCount > 0 ? (
          <span className="inline-flex items-center justify-center min-w-[20px] h-[20px] px-1.5 bg-brand text-white border-2 border-black rounded-md text-[0.65rem] font-bold">
            {unreadCount}
          </span>
        ) : null}
        <button
          type="button"
          onClick={() => {
            setLoading(true);
            void fetchMessages();
          }}
          disabled={loading}
          aria-label="Refresh cron messages"
          className="inline-flex items-center justify-center w-7 h-7 border-2 border-black rounded-md bg-white shadow-brutal-xs hover:bg-canvas active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
        >
          <RiRefreshLine className={`text-base ${loading ? 'animate-spin' : ''}`} />
        </button>
      </header>

      <div className="flex-1 overflow-auto">
        {error ? (
          <div className="m-3 p-3 border-[2px] border-err text-err rounded-md text-[0.75rem] font-mono">
            {error}
          </div>
        ) : null}
        {loading && items.length === 0 ? (
          <div className="flex justify-center py-6 text-ink-soft">
            <RiLoader4Line className="text-2xl animate-spin" />
          </div>
        ) : items.length === 0 ? (
          <div className="px-4 py-8 text-center text-ink-soft text-[0.8rem] font-mono">
            No cron fires yet.
          </div>
        ) : (
          <ul className="flex flex-col">
            {items.map((msg) => (
              <CronMessageRow
                key={msg.session_id}
                message={msg}
                expanded={expanded.has(msg.session_id)}
                onToggle={() => toggle(msg.session_id)}
              />
            ))}
          </ul>
        )}
      </div>
    </aside>
  );
}

interface CronMessageRowProps {
  message: CronMessage;
  expanded: boolean;
  onToggle: () => void;
}

function CronMessageRow({ message, expanded, onToggle }: CronMessageRowProps) {
  const summary = useMemo(() => {
    const text = message.response?.trim() || message.prompt.trim();
    if (!text) return '(no content)';
    const oneLine = text.replace(/\s+/g, ' ');
    return oneLine.length > 96 ? `${oneLine.slice(0, 96)}…` : oneLine;
  }, [message.prompt, message.response]);

  const fireLabel = useMemo(() => formatRelative(message.fired_at), [message.fired_at]);

  return (
    <li className="border-b-2 border-black">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        className="w-full text-left px-3 py-2.5 flex flex-col gap-1 hover:bg-canvas cursor-pointer"
      >
        <div className="flex items-center gap-2 text-[0.65rem] font-bold uppercase tracking-wider text-ink-soft">
          <RiAlarmLine className="text-sm text-ink shrink-0" />
          <code
            className="font-mono text-[0.7rem] text-ink truncate"
            title={message.cron_job_id}
          >
            {message.cron_job_id.slice(0, 8)}
          </code>
          <span className="flex-1" />
          <span title={message.fired_at}>{fireLabel}</span>
          {expanded ? (
            <RiArrowUpSLine className="text-base shrink-0 text-ink" />
          ) : (
            <RiArrowDownSLine className="text-base shrink-0 text-ink" />
          )}
        </div>
        {!expanded ? (
          <p className="text-[0.8rem] line-clamp-2 leading-snug">{summary}</p>
        ) : null}
      </button>
      {expanded ? (
        <div className="px-3 pb-3 flex flex-col gap-2">
          <Section label="Prompt" body={message.prompt} />
          <Section label="Response" body={message.response ?? null} />
          <div className="text-[0.65rem] font-mono text-ink-soft break-all">
            <span className="font-bold uppercase tracking-wider mr-1">Session</span>
            {message.session_id}
          </div>
        </div>
      ) : null}
    </li>
  );
}

function Section({ label, body }: { label: string; body: string | null }) {
  return (
    <div>
      <div className="text-[0.65rem] font-bold uppercase tracking-wider text-ink-soft mb-1">
        {label}
      </div>
      {body ? (
        <div className="border-2 border-black bg-canvas rounded-md px-2 py-1.5 font-mono text-[0.8rem] whitespace-pre-wrap break-words leading-snug">
          {body}
        </div>
      ) : (
        <div className="text-[0.75rem] text-ink-soft italic">
          (pending)
        </div>
      )}
    </div>
  );
}

/** "HH:MM" today, "Mon DD" within a week, else "YYYY-MM-DD". The
 *  inbox is dense so we trade absolute precision for scan-ability;
 *  the absolute `fired_at` is available via the row's `title`
 *  tooltip when a user needs it. */
function formatRelative(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  if (sameDay) {
    return d.toLocaleTimeString('sv-SE', {
      hour: '2-digit',
      minute: '2-digit',
    });
  }
  const ageMs = now.getTime() - d.getTime();
  const sevenDaysMs = 7 * 24 * 60 * 60 * 1000;
  if (ageMs < sevenDaysMs && ageMs >= 0) {
    return d.toLocaleDateString('en-US', { month: 'short', day: '2-digit' });
  }
  return d.toLocaleDateString('sv-SE');
}
