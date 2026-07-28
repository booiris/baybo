// Server-backed message search, rendered in place of the session tree while the
// sidebar's search box holds a query. See `docs/search.md`.
//
// Deliberately NOT the shared `components/SearchBox`: that one is a client-side
// filter over an already-fetched array (Traces / Cron / Logs) and renders a
// funnel icon. This queries the server, so it owns debounce, in-flight and empty
// states that a filter has no concept of.

import { useCallback, useEffect, useRef, useState } from 'react';
import { RiLoader4Line, RiSearchLine } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import type { components } from '../../api/schema';
import { snippet } from './searchSnippet';

type Hit = components['schemas']['ChatSearchHit'];
type Group = components['schemas']['ChatSearchGroup'];

/** Long enough that typing a word is one request, short enough to feel live. */
const DEBOUNCE_MS = 200;
/** Below this a CJK query matches nearly everything and the result list is
 *  noise — the index makes every Han codepoint its own token, so one character
 *  is a legitimate but useless query. */
const MIN_QUERY_LEN = 2;

type State =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'ok'; groups: Group[]; truncated: boolean }
  | { kind: 'failed'; message: string };

function timeLabel(iso: string): string {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? '' : d.toLocaleString();
}

function Excerpt({ hit, query }: { hit: Hit; query: string }) {
  return (
    <div className="border-l-2 border-black/15 pl-2">
      <p className="text-[0.78rem] leading-snug break-words line-clamp-2">
        {snippet(hit.text, query).map((seg, i) =>
          seg.match ? (
            <mark key={i} className="bg-brand/60 text-ink rounded-[2px]">
              {seg.text}
            </mark>
          ) : (
            <span key={i}>{seg.text}</span>
          ),
        )}
      </p>
      <div className="flex items-center gap-2">
        <span className="text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
          {hit.role}
        </span>
        <span className="text-[0.6rem] font-mono text-ink-soft">{timeLabel(hit.created_at)}</span>
        {hit.superseded_by !== undefined && hit.superseded_by !== null ? (
          // The chat view renders only live rows, so this hit exists in history
          // but is not on screen in the conversation it belongs to.
          <span
            className="text-[0.55rem] font-bold uppercase tracking-wider text-ink-soft border border-ink-soft/40 rounded px-1"
            title="This message was compacted away — the conversation no longer shows it"
          >
            compacted
          </span>
        ) : null}
      </div>
    </div>
  );
}

/** One conversation's matches. Grouped rather than listed flat because a single
 *  chatty conversation would otherwise fill the whole result set — measured on
 *  real data, one took 15 of 30 slots and hid 10 other conversations. */
function GroupCard({
  group,
  query,
  active,
  onOpen,
}: {
  group: Group;
  query: string;
  active: boolean;
  onOpen: (sessionId: string) => void;
}) {
  const more = group.total_hits - group.hits.length;
  return (
    <button
      type="button"
      onClick={() => onOpen(group.session_id)}
      className={`w-full text-left px-2 py-2 border-2 rounded-md cursor-pointer flex flex-col gap-1.5 ${
        active ? 'bg-selected border-black' : 'bg-white border-black/20 hover:border-black'
      }`}
    >
      <div className="flex items-baseline gap-2">
        <span className="text-[0.75rem] font-bold truncate flex-1">
          {group.session_title ?? 'Untitled'}
        </span>
        <span className="text-[0.6rem] font-mono text-ink-soft shrink-0">
          {group.total_hits}
        </span>
      </div>
      {group.hits.map((hit) => (
        <Excerpt key={hit.ordinal} hit={hit} query={query} />
      ))}
      {more > 0 ? (
        <span className="text-[0.6rem] font-mono text-ink-soft">and {more} more</span>
      ) : null}
    </button>
  );
}

export function SearchPanel({
  query,
  activeSessionId,
  onOpen,
}: {
  query: string;
  activeSessionId: string | null | undefined;
  onOpen: (sessionId: string) => void;
}) {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [state, setState] = useState<State>({ kind: 'idle' });
  // Monotonic request id: a slow response for an older query must never
  // overwrite a newer one's results.
  const latest = useRef(0);

  const run = useCallback(
    async (q: string, seq: number) => {
      try {
        const { data, error, response } = await client.GET('/v1/chat/search', {
          params: { query: { q } },
        });
        if (seq !== latest.current) return;
        if (response.status === 401) {
          logout();
          return;
        }
        if (error || !response.ok) {
          setState({ kind: 'failed', message: error?.error || `HTTP Error ${response.status}` });
          return;
        }
        setState({ kind: 'ok', groups: data?.groups ?? [], truncated: data?.truncated ?? false });
      } catch (e) {
        if (seq !== latest.current) return;
        setState({ kind: 'failed', message: e instanceof Error ? e.message : String(e) });
      }
    },
    [client, logout],
  );

  useEffect(() => {
    const q = query.trim();
    const seq = ++latest.current;
    if (q.length < MIN_QUERY_LEN) {
      setState({ kind: 'idle' });
      return;
    }
    setState({ kind: 'loading' });
    const t = setTimeout(() => void run(q, seq), DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [query, run]);

  if (state.kind === 'idle') {
    return (
      <div className="flex flex-col items-center gap-2 py-8 text-ink-soft">
        <RiSearchLine className="text-2xl" />
        <span className="text-xs font-mono text-center px-4">
          Type {MIN_QUERY_LEN}+ characters to search every conversation
        </span>
      </div>
    );
  }
  if (state.kind === 'loading') {
    return (
      <div className="flex justify-center py-6 text-ink-soft">
        <RiLoader4Line className="text-2xl animate-spin" />
      </div>
    );
  }
  if (state.kind === 'failed') {
    return (
      <div className="text-center text-err text-xs py-6 font-mono px-2 break-words">{state.message}</div>
    );
  }
  if (state.groups.length === 0) {
    return (
      <div className="text-center text-ink-soft text-sm py-6 font-mono">No matches.</div>
    );
  }
  return (
    <>
      {state.groups.map((group) => (
        <GroupCard
          key={group.session_id}
          group={group}
          query={query.trim()}
          active={group.session_id === activeSessionId}
          onOpen={onOpen}
        />
      ))}
      {state.truncated ? (
        <div className="text-center text-ink-soft text-[0.7rem] py-2 font-mono">
          More matches — refine the search.
        </div>
      ) : null}
    </>
  );
}
