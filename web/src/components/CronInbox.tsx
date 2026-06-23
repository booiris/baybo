import { type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  RiArrowDownSLine,
  RiArrowUpSLine,
  RiCheckDoubleLine,
  RiHashtag,
  RiLoader4Line,
  RiNotification3Fill,
  RiNotificationOffLine,
  RiQuillPenLine,
  RiRefreshLine,
  RiRobot2Line,
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

/** localStorage key holding the JSON array of cron fire `session_id`s
 *  the user has acknowledged (by opening a row or "mark all read").
 *  Read-state lives only in the browser by design — the gateway has no
 *  per-fire read tracking. `null` means "no baseline yet" (first run),
 *  which is distinct from "baselined, nothing unread". */
const SEEN_KEY = 'baybo.cron.seen';

function readSeen(): Set<string> | null {
  try {
    const raw = window.localStorage.getItem(SEEN_KEY);
    if (raw === null) return null;
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return null;
    return new Set(parsed.filter((x): x is string => typeof x === 'string'));
  } catch {
    return null;
  }
}

function writeSeen(seen: Set<string>): void {
  try {
    window.localStorage.setItem(SEEN_KEY, JSON.stringify([...seen]));
  } catch {
    /* storage disabled; in-memory tracking still works this session */
  }
}

export function CronInbox({ refreshSignal }: CronInboxProps) {
  const client = useAdminClient();
  const [items, setItems] = useState<CronMessage[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [seen, setSeen] = useState<Set<string> | null>(() => readSeen());
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

  const markSeen = useCallback((sessionId: string) => {
    setSeen((prev) => {
      const base = prev ?? new Set<string>();
      if (base.has(sessionId)) return prev;
      const next = new Set(base);
      next.add(sessionId);
      writeSeen(next);
      return next;
    });
  }, []);

  const toggle = useCallback(
    (sessionId: string) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(sessionId)) {
          next.delete(sessionId);
        } else {
          next.add(sessionId);
        }
        return next;
      });
      // Opening a fire (or re-collapsing it) counts as reading it, so
      // its "new" accent and the header tally clear right away — the
      // affordance a user reaches for first.
      markSeen(sessionId);
    },
    [markSeen],
  );

  // First run (no stored baseline) treats every fire already in the
  // list as seen, so opening the panel the first time doesn't flag the
  // whole backlog. Afterwards a fire stays unread until its session_id
  // is acknowledged.
  useEffect(() => {
    if (seen !== null || items.length === 0) return;
    const all = new Set(items.map((m) => m.session_id));
    setSeen(all);
    writeSeen(all);
  }, [items, seen]);

  const markAllSeen = useCallback(() => {
    const all = new Set(items.map((m) => m.session_id));
    setSeen(all);
    writeSeen(all);
  }, [items]);

  const unreadCount = useMemo(
    () => (seen === null ? 0 : items.filter((m) => !seen.has(m.session_id)).length),
    [items, seen],
  );

  // Kept short on purpose: at 260px the mark-all + refresh buttons leave the
  // subtitle ~110px, so a combined "N unread · M fires" clips. Lead with the
  // count that matters — unread when there is any, total otherwise.
  const subtitle =
    items.length === 0
      ? loading
        ? 'Loading…'
        : 'No fires yet'
      : unreadCount > 0
        ? `${unreadCount} unread`
        : `${items.length} ${items.length === 1 ? 'fire' : 'fires'}`;

  return (
    <aside className="hidden xl:flex flex-col w-[260px] border-l-2 border-black bg-canvas absolute right-0 top-12 bottom-0 z-10">
      <header className="px-3 h-14 shrink-0 border-b-2 border-black flex items-center gap-2.5">
        <span className="inline-flex items-center justify-center w-8 h-8 rounded-md border-2 border-black bg-brand shadow-brutal-xs shrink-0">
          <RiNotification3Fill className="text-[1.05rem] text-ink" />
        </span>
        <div className="flex-1 min-w-0">
          <div className="font-bold uppercase tracking-wider text-[0.8rem] leading-none">
            Notifications
          </div>
          <div className="text-[0.62rem] text-ink-soft mt-1.5 leading-none truncate">
            {subtitle}
          </div>
        </div>
        {unreadCount > 0 ? (
          <button
            type="button"
            onClick={markAllSeen}
            title="Mark all as read"
            aria-label={`${unreadCount} new cron fires — mark all as read`}
            className="inline-flex items-center justify-center w-8 h-8 rounded-md border-2 border-transparent text-ink-soft hover:text-ink hover:border-black hover:bg-white transition-colors cursor-pointer"
          >
            <RiCheckDoubleLine className="text-base" />
          </button>
        ) : null}
        <button
          type="button"
          onClick={() => {
            setLoading(true);
            void fetchMessages();
          }}
          disabled={loading}
          aria-label="Refresh cron messages"
          className="inline-flex items-center justify-center w-8 h-8 rounded-md border-2 border-transparent text-ink-soft hover:text-ink hover:border-black hover:bg-white transition-colors disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer"
        >
          <RiRefreshLine className={`text-base ${loading ? 'animate-spin' : ''}`} />
        </button>
      </header>

      <div className="chat-scroll flex-1 overflow-y-auto overflow-x-hidden flex flex-col">
        {error ? (
          <div className="m-2.5 p-2.5 border-2 border-err/60 bg-err/10 text-err rounded-md text-[0.72rem] font-mono break-words">
            {error}
          </div>
        ) : null}
        {loading && items.length === 0 ? (
          <div className="flex-1 flex items-center justify-center py-12 text-ink-soft">
            <RiLoader4Line className="text-2xl animate-spin" />
          </div>
        ) : items.length === 0 ? (
          <div className="flex-1 flex flex-col items-center justify-center text-center px-6 gap-2.5 py-12">
            <span className="inline-flex items-center justify-center w-12 h-12 rounded-full border-2 border-dashed border-ink/30 text-ink-soft">
              <RiNotificationOffLine className="text-xl" />
            </span>
            <div className="text-[0.82rem] font-bold text-ink/80">All caught up</div>
            <div className="text-[0.72rem] text-ink-soft leading-snug max-w-[200px]">
              Scheduled cron fires show up here as they run.
            </div>
          </div>
        ) : (
          <ul className="flex flex-col gap-2 px-2 py-2.5">
            {items.map((msg) => (
              <CronMessageRow
                key={msg.session_id}
                message={msg}
                isNew={seen !== null && !seen.has(msg.session_id)}
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
  isNew: boolean;
  expanded: boolean;
  onToggle: () => void;
}

function CronMessageRow({ message, isNew, expanded, onToggle }: CronMessageRowProps) {
  // Title leads with the instruction (first non-empty line of the prompt);
  // the preview leads with the agent's reply. Length is left to CSS
  // (`truncate` / `line-clamp-2`) so the full text stays available on hover.
  const title = useMemo(() => {
    const firstLine = message.prompt
      .split('\n')
      .map((l) => l.trim())
      .find((l) => l.length > 0);
    return firstLine ?? 'Cron fire';
  }, [message.prompt]);

  const preview = useMemo(() => {
    const text = message.response?.trim();
    return text ? text.replace(/\s+/g, ' ') : null;
  }, [message.response]);

  const fireLabel = useMemo(() => formatRelative(message.fired_at), [message.fired_at]);

  return (
    <li
      className={`relative rounded-md overflow-hidden border-2 ${
        isNew ? 'border-black bg-surface shadow-brutal-xs' : 'border-ink/25 bg-canvas'
      }`}
    >
      {isNew ? (
        <span className="absolute left-0 top-0 bottom-0 w-1 bg-brand" aria-hidden />
      ) : null}
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        className={`w-full text-left ${
          isNew ? 'pl-3.5' : 'pl-3'
        } pr-2.5 py-2.5 flex flex-col gap-1.5 hover:bg-brand/5 cursor-pointer`}
      >
        <div className="flex items-center gap-1.5">
          <span
            className={`text-[0.8rem] leading-tight flex-1 min-w-0 truncate ${
              isNew ? 'font-bold text-ink' : 'font-medium text-ink/90'
            }`}
            title={message.prompt}
          >
            {title}
          </span>
          <span
            className="text-[0.62rem] text-ink-soft tabular-nums shrink-0"
            title={message.fired_at}
          >
            {fireLabel}
          </span>
          {expanded ? (
            <RiArrowUpSLine className="text-base shrink-0 text-ink-soft" />
          ) : (
            <RiArrowDownSLine className="text-base shrink-0 text-ink-soft" />
          )}
        </div>
        {!expanded ? (
          preview ? (
            <p
              className={`text-[0.78rem] leading-snug line-clamp-2 ${
                isNew ? 'text-ink/80' : 'text-ink-soft'
              }`}
            >
              {preview}
            </p>
          ) : (
            <p className="flex items-center gap-1 text-[0.72rem] italic text-ink-soft">
              <RiLoader4Line className="text-[0.85rem] animate-spin shrink-0" />
              Awaiting response…
            </p>
          )
        ) : null}
      </button>
      {expanded ? (
        <div className={`${isNew ? 'pl-3.5' : 'pl-3'} pr-2.5 pb-3 flex flex-col gap-2.5`}>
          <Section icon={<RiQuillPenLine />} label="Prompt" body={message.prompt} />
          <Section icon={<RiRobot2Line />} label="Response" body={message.response ?? null} />
          <div className="flex items-center gap-1.5 text-[0.6rem] font-mono text-ink-soft/80 pt-0.5">
            <RiHashtag className="text-[0.8rem] shrink-0" />
            <span
              className="truncate"
              title={`job ${message.cron_job_id}\nsession ${message.session_id}`}
            >
              {message.cron_job_id.slice(0, 8)} · {message.session_id}
            </span>
          </div>
        </div>
      ) : null}
    </li>
  );
}

function Section({
  icon,
  label,
  body,
}: {
  icon: ReactNode;
  label: string;
  body: string | null;
}) {
  return (
    <div>
      <div className="flex items-center gap-1.5 text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft mb-1">
        <span className="text-[0.85rem] shrink-0">{icon}</span>
        {label}
      </div>
      {body ? (
        <div className="rounded border border-ink/10 bg-ink/[0.04] px-2.5 py-1.5 font-mono text-[0.76rem] whitespace-pre-wrap break-words leading-snug">
          {body}
        </div>
      ) : (
        <div className="text-[0.72rem] text-ink-soft italic">(pending)</div>
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
