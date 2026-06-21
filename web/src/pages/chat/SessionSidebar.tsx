import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import {
  RiAddLine,
  RiDeleteBin6Line,
  RiLoader4Line,
  RiPushpin2Fill,
  RiPushpin2Line,
  RiStackLine,
} from 'react-icons/ri';
import type { SessionSummary } from './types';
import { useQueueCounts } from './queueStore';

// Chat zone 2: the session list sidebar (the global icon rail is zone 1, the
// thread + floating composer is zone 3). A newest-first conversation list of
// compact single-line rows: a coral-red-highlighted active row, the title with
// a right-aligned mono timestamp, an unread badge, and hover-reveal pin + hide
// affordances. Pinned sessions are lifted into a labelled block above the rest.

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
  queueCount,
  onHide,
  onTogglePin,
}: {
  session: SessionSummary;
  active: boolean;
  hasPending: boolean;
  unreadCount: number;
  queueCount: number;
  onHide: (id: string) => void;
  onTogglePin: (id: string, pinned: boolean) => void;
}) {
  // Unread badge only shows on background rows — the active row is
  // already cleared on entry, but guard anyway in case a frame races
  // the clearing effect.
  const showUnread = unreadCount > 0 && !active;
  const iconBtn = `flex items-center justify-center h-5 w-5 rounded shrink-0 ${
    active ? 'text-ink hover:bg-white/40' : 'text-ink-soft hover:bg-white'
  }`;
  return (
    <Link
      to={`/chat/${session.session_id}`}
      className={`group relative flex items-center gap-2 px-3 py-1.5 rounded-md border-2 ${
        active
          ? 'bg-selected text-ink border-black shadow-brutal-sm'
          : 'border-transparent hover:bg-gray-100 text-ink'
      }`}
      title={session.session_id}
    >
      {/* Persistent pin glyph on pinned rows so the state reads at a
          glance even before hovering; hidden on hover where the
          interactive toggle takes its place. */}
      {session.pinned ? (
        <RiPushpin2Fill
          className={`text-[0.7rem] shrink-0 group-hover:hidden ${
            active ? 'text-ink/70' : 'text-ink-soft'
          }`}
          title="Pinned"
        />
      ) : null}
      <span
        className={`text-sm flex-1 truncate text-ink ${active ? 'font-bold' : ''} ${
          session.last_user_text ? '' : 'italic opacity-70'
        }`}
        title={session.last_user_text ?? undefined}
      >
        {session.last_user_text ?? 'New conversation'}
      </span>
      {/* Parked interjection-queue count — shown for ALL sessions (incl. the
          active row); reads the shared queue store, independent of WS subs. */}
      {queueCount > 0 ? (
        <span
          className={`shrink-0 inline-flex items-center gap-0.5 min-w-[20px] h-5 px-1.5 rounded-full border-2 border-black font-mono text-[0.65rem] font-bold leading-none ${
            active ? 'bg-white/60 text-ink' : 'bg-canvas text-ink-soft'
          }`}
          title={`${queueCount} queued message${queueCount === 1 ? '' : 's'}`}
          aria-label={`${queueCount} queued message${queueCount === 1 ? '' : 's'}`}
        >
          <RiStackLine className="text-[0.7rem]" aria-hidden />
          {queueCount > 99 ? '99+' : queueCount}
        </span>
      ) : null}
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
      <div className="hidden group-hover:flex items-center gap-0.5">
        <button
          type="button"
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            onTogglePin(session.session_id, !session.pinned);
          }}
          className={iconBtn}
          title={session.pinned ? 'Unpin from top' : 'Pin to top'}
          aria-label={session.pinned ? 'Unpin conversation' : 'Pin conversation to top'}
        >
          {session.pinned ? (
            <RiPushpin2Fill className="text-sm" />
          ) : (
            <RiPushpin2Line className="text-sm" />
          )}
        </button>
        <button
          type="button"
          onClick={(e) => {
            e.preventDefault();
            e.stopPropagation();
            onHide(session.session_id);
          }}
          className={iconBtn}
          title="Hide from list (server-side row is kept)"
          aria-label="Hide conversation"
        >
          <RiDeleteBin6Line className="text-sm" />
        </button>
      </div>
    </Link>
  );
}

/** Small uppercase divider label for the pinned / recent blocks. */
function SectionLabel({ children }: { children: ReactNode }) {
  return (
    <div className="px-2 pt-2 pb-0.5 font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft select-none">
      {children}
    </div>
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
  onTogglePin,
}: {
  sessions: SessionSummary[];
  activeSessionId: string | null | undefined;
  pendingIds: ReadonlySet<string>;
  creating: boolean;
  loading: boolean;
  onNewChat: () => void;
  onHide: (id: string) => void;
  onTogglePin: (id: string, pinned: boolean) => void;
}) {
  // Partition preserves the incoming newest-first order within each block,
  // so pinning never reshuffles the relative order of conversations.
  const pinned = sessions.filter((s) => s.pinned);
  const rest = sessions.filter((s) => !s.pinned);
  const queueCounts = useQueueCounts();
  const renderRow = (s: SessionSummary) => (
    <SessionRow
      key={s.session_id}
      session={s}
      active={s.session_id === activeSessionId}
      hasPending={pendingIds.has(s.session_id)}
      unreadCount={s.unread}
      queueCount={queueCounts.get(s.session_id) ?? 0}
      onHide={onHide}
      onTogglePin={onTogglePin}
    />
  );
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
          <>
            {pinned.length > 0 ? (
              <>
                <SectionLabel>Pinned</SectionLabel>
                {pinned.map(renderRow)}
                {rest.length > 0 ? <SectionLabel>Recent</SectionLabel> : null}
              </>
            ) : null}
            {rest.map(renderRow)}
          </>
        )}
      </nav>
    </aside>
  );
}
