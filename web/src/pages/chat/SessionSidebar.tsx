import { Link } from 'react-router-dom';
import { RiAddLine, RiDeleteBin6Line, RiLoader4Line } from 'react-icons/ri';
import type { SessionSummary } from './types';

// Chat zone 2: the session list sidebar (the global icon rail is zone 1, the
// thread + floating composer is zone 3). A flat, newest-first conversation
// list of compact single-line rows: a coral-red-highlighted active row, the
// title with a right-aligned mono timestamp, an unread badge, and a
// hover-reveal hide affordance.

/** Compact human-readable age. Same shape the logs page uses, kept
 *  local since the dep would be marginal. */
function relativeAge(iso: string, now: number = Date.now()): string {
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return '';
  const diffSec = Math.max(0, Math.floor((now - t) / 1000));
  if (diffSec < 5) return 'just now';
  if (diffSec < 60) return `${diffSec}s ago`;
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`;
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`;
  if (diffSec < 86400 * 7) return `${Math.floor(diffSec / 86400)}d ago`;
  return new Date(iso).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

function SessionRow({
  session,
  active,
  hasPending,
  unreadCount,
  onHide,
}: {
  session: SessionSummary;
  active: boolean;
  hasPending: boolean;
  unreadCount: number;
  onHide: (id: string) => void;
}) {
  // Unread badge only shows on background rows — the active row is
  // already cleared on entry, but guard anyway in case a frame races
  // the clearing effect.
  const showUnread = unreadCount > 0 && !active;
  return (
    <Link
      to={`/chat/${session.session_id}`}
      className={`group relative flex items-center gap-2 px-3 py-1.5 rounded-md border-2 ${
        active
          ? 'bg-selected text-ink border-black'
          : 'border-transparent hover:bg-gray-100 text-ink'
      }`}
      title={session.session_id}
    >
      <span
        className={`text-sm flex-1 truncate text-ink ${active ? 'font-bold' : ''} ${
          session.last_user_text ? '' : 'italic opacity-70'
        }`}
        title={session.last_user_text ?? undefined}
      >
        {session.last_user_text ?? 'New conversation'}
      </span>
      {showUnread ? (
        <span
          className="shrink-0 min-w-[20px] h-5 px-1.5 rounded-full bg-brand text-ink border-2 border-black font-mono text-[0.65rem] font-bold flex items-center justify-center leading-none"
          title={`${unreadCount} unread message${unreadCount === 1 ? '' : 's'}`}
        >
          {unreadCount > 99 ? '99+' : unreadCount}
        </span>
      ) : null}
      {hasPending ? (
        <span
          className={`w-2 h-2 rounded-full shrink-0 ${active ? 'bg-white' : 'bg-warn'}`}
          title="Approval pending"
        />
      ) : null}
      <span
        className={`shrink-0 font-mono text-[0.65rem] group-hover:hidden ${
          active ? 'text-ink/70' : 'text-ink-soft'
        }`}
      >
        {relativeAge(session.last_active)}
      </span>
      <button
        type="button"
        onClick={(e) => {
          e.preventDefault();
          e.stopPropagation();
          onHide(session.session_id);
        }}
        className={`hidden group-hover:flex items-center justify-center h-5 w-5 rounded shrink-0 ${
          active ? 'text-ink hover:bg-white/40' : 'text-ink-soft hover:bg-white'
        }`}
        title="Hide from list (server-side row is kept)"
        aria-label="Hide conversation"
      >
        <RiDeleteBin6Line className="text-sm" />
      </button>
    </Link>
  );
}

export function SessionSidebar({
  sessions,
  activeSessionId,
  pendingIds,
  creating,
  loading,
  onNewChat,
  onHide,
}: {
  sessions: SessionSummary[];
  activeSessionId: string | null | undefined;
  pendingIds: ReadonlySet<string>;
  creating: boolean;
  loading: boolean;
  onNewChat: () => void;
  onHide: (id: string) => void;
}) {
  return (
    <aside className="w-[260px] border-r-2 border-black flex flex-col bg-canvas shrink-0">
      <div className="px-3 py-3 border-b-2 border-black">
        <button
          type="button"
          onClick={onNewChat}
          disabled={creating}
          className="w-full flex items-center justify-center gap-2 px-3 py-2 bg-brand text-ink border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.85rem] hover:bg-brand-hover active:translate-x-[2px] active:translate-y-[2px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
        >
          <RiAddLine className="text-lg" />
          New chat
        </button>
      </div>
      <nav className="flex-1 overflow-auto px-2 py-2 flex flex-col gap-1">
        {loading ? (
          <div className="flex justify-center py-6 text-ink-soft">
            <RiLoader4Line className="text-2xl animate-spin" />
          </div>
        ) : sessions.length === 0 ? (
          <div className="text-center text-ink-soft text-sm py-6 font-mono">
            No conversations yet.
          </div>
        ) : (
          sessions.map((s) => (
            <SessionRow
              key={s.session_id}
              session={s}
              active={s.session_id === activeSessionId}
              hasPending={pendingIds.has(s.session_id)}
              unreadCount={s.unread}
              onHide={onHide}
            />
          ))
        )}
      </nav>
    </aside>
  );
}
