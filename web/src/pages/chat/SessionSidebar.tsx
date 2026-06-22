import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { Link } from 'react-router-dom';
import {
  RiAddLine,
  RiChat1Line,
  RiDeleteBin6Line,
  RiLoader4Line,
  RiPushpin2Fill,
  RiPushpin2Line,
  RiStackLine,
  RiArrowRightSLine,
  RiArrowDownSLine,
  RiFolderAddLine,
  RiFolder3Line,
  RiFolderOpenLine,
} from 'react-icons/ri';
import {
  DndContext,
  DragOverlay,
  PointerSensor,
  KeyboardSensor,
  useSensor,
  useSensors,
  useDraggable,
  useDroppable,
  pointerWithin,
  type DragStartEvent,
  type DragEndEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  sortableKeyboardCoordinates,
  verticalListSortingStrategy,
  useSortable,
  arrayMove,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import type { Folder, SessionSummary } from './types';
import { useQueueCounts } from './queueStore';
import { useFolders, useFolderStore } from './folderStore';
import { ChatContextMenu, FolderContextMenu } from './FolderContextMenu';

// Chat zone 2: the session list sidebar (the global icon rail is zone 1, the
// thread + floating composer is zone 3). A newest-first conversation list of
// compact rows organised into a 2-level folder tree, plus lifted Pinned and
// trailing Uncategorized buckets. Drag a chat onto a folder header to file it,
// drag folders to reorder/nest; right-click for the full action set.

const MAX_DEPTH = 2;

// dnd item id prefixes — folders and chats share one DndContext, so ids are
// namespaced and parsed back on drag end.
const CHAT_PREFIX = 'chat:';
const FOLDER_PREFIX = 'folder:';
const UNCATEGORIZED_DROP_ID = 'drop:uncategorized';

type DragKind = { kind: 'chat'; id: string } | { kind: 'folder'; id: string } | null;

function parseDragId(raw: string): DragKind {
  if (raw.startsWith(CHAT_PREFIX)) return { kind: 'chat', id: raw.slice(CHAT_PREFIX.length) };
  if (raw.startsWith(FOLDER_PREFIX)) return { kind: 'folder', id: raw.slice(FOLDER_PREFIX.length) };
  return null;
}

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

interface RowCallbacks {
  onHide: (id: string) => void;
  onTogglePin: (id: string, pinned: boolean) => void;
  onContextMenu: (session: SessionSummary, x: number, y: number) => void;
}

function SessionRow({
  session,
  active,
  hasPending,
  unreadCount,
  queueCount,
  depth,
  onHide,
  onTogglePin,
  onContextMenu,
}: {
  session: SessionSummary;
  active: boolean;
  hasPending: boolean;
  unreadCount: number;
  queueCount: number;
  /** Indent level (0 for top-level buckets, 1+ inside folders). */
  depth: number;
} & RowCallbacks) {
  // A chat is a plain draggable, NOT a sortable: chat order is server-driven
  // (newest-first), so dragging one must not reflow the list — it only lifts
  // out to drop onto a folder. The floating preview is the DragOverlay; the
  // source row just dims in place (no transform), so nothing slides around.
  const { attributes, listeners, setNodeRef, isDragging } = useDraggable({
    id: `${CHAT_PREFIX}${session.session_id}`,
  });
  const style = {
    paddingLeft: depth > 0 ? `${depth * 14 + 12}px` : undefined,
    opacity: isDragging ? 0.4 : undefined,
  };
  // Unread badge only shows on background rows — the active row is
  // already cleared on entry, but guard anyway in case a frame races
  // the clearing effect.
  const showUnread = unreadCount > 0 && !active;
  const iconBtn = `flex items-center justify-center h-5 w-5 rounded shrink-0 ${
    active ? 'text-ink hover:bg-white/40' : 'text-ink-soft hover:bg-white'
  }`;
  return (
    <Link
      ref={setNodeRef}
      style={style}
      {...attributes}
      {...listeners}
      to={`/chat/${session.session_id}`}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu(session, e.clientX, e.clientY);
      }}
      className={`group relative flex items-center gap-2 px-3 py-1.5 rounded-md border-2 touch-none ${
        active
          ? 'bg-selected text-ink border-black shadow-brutal-sm'
          : 'border-transparent hover:bg-gray-100 text-ink'
      }`}
      title={session.session_id}
    >
      {/* Leading conversation glyph — distinguishes a chat row from a
          folder row (which carries a folder glyph) at a glance. */}
      <RiChat1Line
        className={`text-[0.8rem] shrink-0 ${active ? 'text-ink/70' : 'text-ink-soft'}`}
        aria-hidden
      />
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
          onPointerDown={(e) => e.stopPropagation()}
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
          onPointerDown={(e) => e.stopPropagation()}
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

/** A static (non-draggable) preview of a chat row for the DragOverlay. */
function ChatDragPreview({ session }: { session: SessionSummary }) {
  return (
    <div className="flex items-center gap-2 px-3 py-1.5 rounded-md border-2 border-black bg-surface shadow-brutal-sm w-[240px]">
      <RiChat1Line className="text-[0.8rem] shrink-0 text-ink-soft" aria-hidden />
      <span className="text-sm flex-1 truncate text-ink">
        {session.last_user_text ?? 'New conversation'}
      </span>
    </div>
  );
}

/** Small uppercase divider label for the pinned / uncategorized blocks. */
function SectionLabel({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div className="px-2 pt-2 pb-0.5 flex items-center justify-between gap-2 select-none">
      <span className="font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        {children}
      </span>
      {action}
    </div>
  );
}

/** A folder header: drag handle (sortable), drop target (droppable), caret,
 *  folder icon, and name (or inline rename input). */
function FolderHeader({
  folder,
  depth,
  collapsed,
  renaming,
  renameDraft,
  onRenameDraft,
  onRenameCommit,
  onRenameCancel,
  onToggle,
  onContextMenu,
}: {
  folder: Folder;
  depth: number;
  collapsed: boolean;
  renaming: boolean;
  renameDraft: string;
  onRenameDraft: (v: string) => void;
  onRenameCommit: () => void;
  onRenameCancel: () => void;
  onToggle: () => void;
  onContextMenu: (folder: Folder, x: number, y: number) => void;
}) {
  // `useSortable` registers this folder as BOTH a sortable and a droppable under
  // the one id, so its `setNodeRef`/`isOver` cover the drop-target highlight —
  // a separate `useDroppable` on the same id would double-register and race the
  // unregister on unmount.
  const { attributes, listeners, setNodeRef, transform, transition, isDragging, isOver } =
    useSortable({
      id: `${FOLDER_PREFIX}${folder.id}`,
    });
  const style = {
    transform: CSS.Transform.toString(transform),
    transition,
    paddingLeft: depth > 0 ? `${depth * 14 + 4}px` : '4px',
    opacity: isDragging ? 0.4 : undefined,
  };
  return (
    <div
      ref={setNodeRef}
      style={style}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu(folder, e.clientX, e.clientY);
      }}
      className={`group flex items-center gap-1.5 pr-2 py-1 rounded-md border-2 touch-none ${
        isOver ? 'border-black bg-brand/30 shadow-brutal-xs' : 'border-transparent hover:bg-gray-100'
      }`}
    >
      <button
        type="button"
        onPointerDown={(e) => e.stopPropagation()}
        onClick={onToggle}
        className="shrink-0 flex items-center justify-center h-5 w-5 text-ink-soft hover:text-ink cursor-pointer"
        title={collapsed ? 'Expand folder' : 'Collapse folder'}
        aria-label={collapsed ? 'Expand folder' : 'Collapse folder'}
      >
        {collapsed ? (
          <RiArrowRightSLine className="text-base" />
        ) : (
          <RiArrowDownSLine className="text-base" />
        )}
      </button>
      <span className="shrink-0 cursor-grab active:cursor-grabbing flex items-center text-ink-soft" {...attributes} {...listeners}>
        {collapsed ? (
          <RiFolder3Line className="text-base" aria-hidden />
        ) : (
          <RiFolderOpenLine className="text-base" aria-hidden />
        )}
      </span>
      {renaming ? (
        <input
          autoFocus
          value={renameDraft}
          onChange={(e) => onRenameDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              onRenameCommit();
            } else if (e.key === 'Escape') {
              e.preventDefault();
              onRenameCancel();
            }
          }}
          onBlur={onRenameCommit}
          className="flex-1 min-w-0 bg-surface border-2 border-black rounded px-1.5 py-0.5 font-mono text-xs text-ink focus:outline-none"
        />
      ) : (
        <span
          className="flex-1 min-w-0 truncate font-mono text-xs font-bold text-ink cursor-grab active:cursor-grabbing"
          {...attributes}
          {...listeners}
          title={folder.name}
        >
          {folder.name}
        </span>
      )}
    </div>
  );
}

/** An inline new-folder input row inserted at a given depth. Enter commits,
 *  Escape cancels, blur commits-or-cancels. */
function NewFolderRow({
  depth,
  onCommit,
  onCancel,
}: {
  depth: number;
  onCommit: (name: string) => void;
  onCancel: () => void;
}) {
  const [draft, setDraft] = useState('');
  const commit = useCallback(() => {
    const name = draft.trim();
    if (name.length > 0) onCommit(name);
    else onCancel();
  }, [draft, onCommit, onCancel]);
  return (
    <div
      className="flex items-center gap-1.5 py-1 pr-2"
      style={{ paddingLeft: depth > 0 ? `${depth * 14 + 4}px` : '4px' }}
    >
      <span className="shrink-0 h-5 w-5 flex items-center justify-center text-ink-soft">
        <RiFolderAddLine className="text-base" />
      </span>
      <input
        autoFocus
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            commit();
          } else if (e.key === 'Escape') {
            e.preventDefault();
            onCancel();
          }
        }}
        onBlur={commit}
        placeholder="Folder name…"
        className="flex-1 min-w-0 bg-surface border-2 border-black rounded px-1.5 py-0.5 font-mono text-xs text-ink focus:outline-none"
      />
    </div>
  );
}

/** The trailing Uncategorized bucket doubles as a drop target that clears a
 *  chat's folder (and promotes a dropped folder to top-level). */
function UncategorizedDropZone({ children }: { children: ReactNode }) {
  const { setNodeRef, isOver } = useDroppable({ id: UNCATEGORIZED_DROP_ID });
  return (
    <div
      ref={setNodeRef}
      className={`rounded-md ${isOver ? 'bg-brand/20 ring-2 ring-black/30' : ''}`}
    >
      {children}
    </div>
  );
}

interface FolderMenuState {
  folder: Folder;
  x: number;
  y: number;
}
interface ChatMenuState {
  session: SessionSummary;
  x: number;
  y: number;
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
  onAssignFolder,
  onCreateFolder,
  onRenameFolder,
  onMoveFolder,
  onReorderFolders,
  onDeleteFolder,
  onNewChatInFolder,
}: {
  sessions: SessionSummary[];
  activeSessionId: string | null | undefined;
  pendingIds: ReadonlySet<string>;
  creating: boolean;
  loading: boolean;
  onNewChat: () => void;
  onHide: (id: string) => void;
  onTogglePin: (id: string, pinned: boolean) => void;
  onAssignFolder: (id: string, folderId: string | null) => void;
  onCreateFolder: (name: string, parentId?: string) => void;
  onRenameFolder: (id: string, name: string) => void;
  onMoveFolder: (id: string, parentId: string | null) => void;
  onReorderFolders: (parentId: string | null, orderedIds: string[]) => void;
  onDeleteFolder: (id: string) => void;
  onNewChatInFolder: (folderId: string) => void;
}) {
  const queueCounts = useQueueCounts();
  const { folders, collapsed } = useFolders();
  const { toggleCollapse, ensureExpanded } = useFolderStore();

  // ── Buckets ────────────────────────────────────────────────────────────
  // Pinned chats are lifted out of the tree entirely. Non-pinned chats are
  // grouped by folder_id; folder ids that no longer exist fall back to
  // uncategorized so a stale folder_id never hides a chat. Incoming
  // newest-first order is preserved within each bucket.
  const folderById = useMemo(() => {
    const m = new Map<string, Folder>();
    for (const f of folders) m.set(f.id, f);
    return m;
  }, [folders]);

  // Folder ids the tree walk can actually reach from a root within MAX_DEPTH:
  // every top-level folder (depth 0) plus every folder whose parent is itself a
  // top-level folder (depth 1). This inherently excludes dangling parents,
  // cycles among non-top-level folders, and depth-≥2 grandchildren — folders
  // that exist but render nowhere. A chat filed under such a folder must still
  // surface, so bucketing keys "is this folder real?" off reachability, not
  // mere existence, and falls the chat back to Uncategorized otherwise.
  const reachableFolderIds = useMemo(() => {
    const topLevel = new Set<string>();
    for (const f of folders) {
      if (f.parent_id === undefined) topLevel.add(f.id);
    }
    const reachable = new Set<string>(topLevel);
    for (const f of folders) {
      if (f.parent_id !== undefined && topLevel.has(f.parent_id)) reachable.add(f.id);
    }
    return reachable;
  }, [folders]);

  const { pinned, chatsByFolder, uncategorized } = useMemo(() => {
    const pinnedRows: SessionSummary[] = [];
    const byFolder = new Map<string, SessionSummary[]>();
    const loose: SessionSummary[] = [];
    for (const s of sessions) {
      if (s.pinned) {
        pinnedRows.push(s);
        continue;
      }
      if (s.folder_id && reachableFolderIds.has(s.folder_id)) {
        const arr = byFolder.get(s.folder_id) ?? [];
        arr.push(s);
        byFolder.set(s.folder_id, arr);
      } else {
        loose.push(s);
      }
    }
    // Keep every bucket newest-first by last_active (the server's
    // `ORDER BY last_active DESC`), re-applied here so a `session_activity`
    // pulse — which updates `last_active` in place — actually lifts the row.
    // Without it an autonomously-active session (a running `/goal`, a cron
    // fire) refreshes its timestamp/unread but never rises to the top.
    const byRecency = (a: SessionSummary, b: SessionSummary) =>
      Date.parse(b.last_active) - Date.parse(a.last_active);
    pinnedRows.sort(byRecency);
    for (const arr of byFolder.values()) arr.sort(byRecency);
    loose.sort(byRecency);
    return { pinned: pinnedRows, chatsByFolder: byFolder, uncategorized: loose };
  }, [sessions, reachableFolderIds]);

  // Children of each parent, sorted by position ascending. `null` parent key
  // holds the top-level folders.
  const childFolders = useMemo(() => {
    const m = new Map<string | null, Folder[]>();
    for (const f of folders) {
      const key = f.parent_id ?? null;
      const arr = m.get(key) ?? [];
      arr.push(f);
      m.set(key, arr);
    }
    for (const arr of m.values()) arr.sort((a, b) => a.position - b.position);
    return m;
  }, [folders]);

  const topLevelFolders = childFolders.get(null) ?? [];
  const folderHasChildren = useCallback(
    (id: string) => (childFolders.get(id)?.length ?? 0) > 0,
    [childFolders],
  );

  // ── Auto-expand active chat's folder path ────────────────────────────────
  const activeSession = sessions.find((s) => s.session_id === activeSessionId);
  const activeFolderId = activeSession?.pinned ? undefined : activeSession?.folder_id;
  useEffect(() => {
    if (!activeFolderId) return;
    const folder = folderById.get(activeFolderId);
    if (!folder) return;
    const path = folder.parent_id ? [folder.parent_id, folder.id] : [folder.id];
    ensureExpanded(path);
  }, [activeFolderId, folderById, ensureExpanded]);

  // ── Inline create / rename state ─────────────────────────────────────────
  // `creatingIn`: undefined = no create open; null = a top-level create;
  // string = a subfolder create under that parent id.
  const [creatingIn, setCreatingIn] = useState<string | null | undefined>(undefined);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState('');

  const startRename = useCallback((f: Folder) => {
    setRenamingId(f.id);
    setRenameDraft(f.name);
  }, []);
  const commitRename = useCallback(() => {
    // Fire the PATCH from the handler body, not inside a state updater — React
    // StrictMode double-invokes updaters in dev, which would send it twice. The
    // updater is reserved for clearing the renaming state.
    if (renamingId !== null) {
      const name = renameDraft.trim();
      const cur = folderById.get(renamingId);
      if (name.length > 0 && cur && name !== cur.name) onRenameFolder(renamingId, name);
    }
    setRenamingId(null);
  }, [renamingId, renameDraft, folderById, onRenameFolder]);
  const cancelRename = useCallback(() => setRenamingId(null), []);

  // ── Context menu state ───────────────────────────────────────────────────
  const [folderMenu, setFolderMenu] = useState<FolderMenuState | null>(null);
  const [chatMenu, setChatMenu] = useState<ChatMenuState | null>(null);

  const openFolderMenu = useCallback((folder: Folder, x: number, y: number) => {
    setChatMenu(null);
    setFolderMenu({ folder, x, y });
  }, []);
  const openChatMenu = useCallback((session: SessionSummary, x: number, y: number) => {
    setFolderMenu(null);
    setChatMenu({ session, x, y });
  }, []);

  // ── Drag & drop ──────────────────────────────────────────────────────────
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  const [dragging, setDragging] = useState<DragKind>(null);

  const onDragStart = useCallback((e: DragStartEvent) => {
    setDragging(parseDragId(String(e.active.id)));
  }, []);

  const onDragEnd = useCallback(
    (event: DragEndEvent) => {
      setDragging(null);
      const { active, over } = event;
      if (!over) return;
      const src = parseDragId(String(active.id));
      const overRaw = String(over.id);
      if (!src) return;

      // ── Dragging a CHAT ──
      if (src.kind === 'chat') {
        if (overRaw === UNCATEGORIZED_DROP_ID) {
          onAssignFolder(src.id, null);
          return;
        }
        const overParsed = parseDragId(overRaw);
        if (overParsed?.kind === 'folder') {
          onAssignFolder(src.id, overParsed.id);
        }
        // Dropping a chat over another chat is ignored — chat order is
        // server-driven (newest-first), not user-sortable.
        return;
      }

      // ── Dragging a FOLDER ──
      if (src.kind === 'folder') {
        const dragged = folderById.get(src.id);
        if (!dragged) return;

        // Promote to top-level.
        if (overRaw === UNCATEGORIZED_DROP_ID) {
          if (dragged.parent_id != null) onMoveFolder(src.id, null);
          return;
        }

        const overParsed = parseDragId(overRaw);
        if (overParsed?.kind !== 'folder') return;
        const target = folderById.get(overParsed.id);
        if (!target || target.id === dragged.id) return;

        const sameParent = (dragged.parent_id ?? null) === (target.parent_id ?? null);
        if (sameParent) {
          // Reorder within the sibling group.
          const parentId = dragged.parent_id ?? null;
          const siblings = (childFolders.get(parentId) ?? []).map((f) => f.id);
          const oldIndex = siblings.indexOf(dragged.id);
          const newIndex = siblings.indexOf(target.id);
          if (oldIndex < 0 || newIndex < 0 || oldIndex === newIndex) return;
          onReorderFolders(parentId, arrayMove(siblings, oldIndex, newIndex));
          return;
        }

        // Nest under `target`. Validity (client-side; server is final arbiter):
        //  - reject if target is itself nested (would exceed depth 2),
        //  - reject if the dragged folder already has children,
        //  - reject if target is a descendant (self-drop covered above).
        if (target.parent_id != null) return;
        if (folderHasChildren(dragged.id)) return;
        onMoveFolder(dragged.id, target.id);
      }
    },
    [folderById, childFolders, folderHasChildren, onAssignFolder, onMoveFolder, onReorderFolders],
  );

  // ── Render helpers ───────────────────────────────────────────────────────
  const renderChatRow = useCallback(
    (s: SessionSummary, depth: number) => (
      <SessionRow
        key={s.session_id}
        session={s}
        active={s.session_id === activeSessionId}
        hasPending={pendingIds.has(s.session_id)}
        unreadCount={s.unread}
        queueCount={queueCounts.get(s.session_id) ?? 0}
        depth={depth}
        onHide={onHide}
        onTogglePin={onTogglePin}
        onContextMenu={openChatMenu}
      />
    ),
    [activeSessionId, pendingIds, queueCounts, onHide, onTogglePin, openChatMenu],
  );

  const renderFolderSubtree = useCallback(
    (folder: Folder, depth: number): ReactNode => {
      const isCollapsed = collapsed.has(folder.id);
      const directChats = chatsByFolder.get(folder.id) ?? [];
      const subFolders = depth + 1 < MAX_DEPTH ? (childFolders.get(folder.id) ?? []) : [];
      return (
        <div key={folder.id} className="flex flex-col">
          <FolderHeader
            folder={folder}
            depth={depth}
            collapsed={isCollapsed}
            renaming={renamingId === folder.id}
            renameDraft={renameDraft}
            onRenameDraft={setRenameDraft}
            onRenameCommit={commitRename}
            onRenameCancel={cancelRename}
            onToggle={() => toggleCollapse(folder.id)}
            onContextMenu={openFolderMenu}
          />
          {!isCollapsed ? (
            <div className="flex flex-col gap-0.5">
              {/* Sub-folders first, then the folder's loose chats. */}
              {subFolders.map((sf) => renderFolderSubtree(sf, depth + 1))}
              {creatingIn === folder.id ? (
                <NewFolderRow
                  depth={depth + 1}
                  onCommit={(name) => {
                    onCreateFolder(name, folder.id);
                    setCreatingIn(undefined);
                  }}
                  onCancel={() => setCreatingIn(undefined)}
                />
              ) : null}
              {directChats.map((s) => renderChatRow(s, depth + 1))}
            </div>
          ) : null}
        </div>
      );
    },
    [
      collapsed,
      chatsByFolder,
      childFolders,
      renamingId,
      renameDraft,
      commitRename,
      cancelRename,
      toggleCollapse,
      openFolderMenu,
      creatingIn,
      onCreateFolder,
      renderChatRow,
    ],
  );

  // Only FOLDERS are sortable (reorder within a sibling group). Chats are
  // plain draggables and deliberately stay OUT of the SortableContext, so
  // dragging a chat — or a folder — never reflows the chat rows.
  const allSortableIds = useMemo(() => {
    const ids: string[] = [];
    for (const f of folders) ids.push(`${FOLDER_PREFIX}${f.id}`);
    return ids;
  }, [folders, sessions]);

  const draggingSession =
    dragging?.kind === 'chat'
      ? sessions.find((s) => s.session_id === dragging.id)
      : undefined;
  const draggingFolder =
    dragging?.kind === 'folder' ? folderById.get(dragging.id) : undefined;

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
        ) : sessions.length === 0 && folders.length === 0 ? (
          <div className="text-center text-ink-soft text-sm py-6 font-mono">
            No conversations yet.
          </div>
        ) : (
          <DndContext
            sensors={sensors}
            collisionDetection={pointerWithin}
            onDragStart={onDragStart}
            onDragEnd={onDragEnd}
            onDragCancel={() => setDragging(null)}
          >
            <SortableContext items={allSortableIds} strategy={verticalListSortingStrategy}>
              {pinned.length > 0 ? (
                <>
                  <SectionLabel>Pinned</SectionLabel>
                  {pinned.map((s) => renderChatRow(s, 0))}
                </>
              ) : null}

              <SectionLabel
                action={
                  <button
                    type="button"
                    onClick={() => {
                      setCreatingIn(null);
                      ensureExpanded([]);
                    }}
                    className="flex items-center justify-center h-5 w-5 rounded text-ink-soft hover:bg-white hover:text-ink cursor-pointer"
                    title="New folder"
                    aria-label="New folder"
                  >
                    <RiFolderAddLine className="text-xs" />
                  </button>
                }
              >
                Folders
              </SectionLabel>
              {creatingIn === null ? (
                <NewFolderRow
                  depth={0}
                  onCommit={(name) => {
                    onCreateFolder(name);
                    setCreatingIn(undefined);
                  }}
                  onCancel={() => setCreatingIn(undefined)}
                />
              ) : null}
              {topLevelFolders.length === 0 && creatingIn !== null ? (
                <div className="px-3 py-1 font-mono text-[0.65rem] text-ink-soft italic">
                  No folders yet.
                </div>
              ) : null}
              {topLevelFolders.map((f) => renderFolderSubtree(f, 0))}

              <UncategorizedDropZone>
                <SectionLabel>Uncategorized</SectionLabel>
                {uncategorized.length > 0 ? (
                  uncategorized.map((s) => renderChatRow(s, 0))
                ) : (
                  <div className="px-3 py-1.5 font-mono text-[0.65rem] text-ink-soft italic select-none">
                    Drop a chat here to remove its folder.
                  </div>
                )}
              </UncategorizedDropZone>
            </SortableContext>

            <DragOverlay>
              {draggingSession ? (
                <ChatDragPreview session={draggingSession} />
              ) : draggingFolder ? (
                <div className="flex items-center gap-1.5 px-2 py-1 rounded-md border-2 border-black bg-surface shadow-brutal-sm w-[220px]">
                  <RiFolder3Line className="text-base text-ink-soft shrink-0" aria-hidden />
                  <span className="font-mono text-xs font-bold truncate">
                    {draggingFolder.name}
                  </span>
                </div>
              ) : null}
            </DragOverlay>
          </DndContext>
        )}
      </nav>

      {folderMenu ? (
        <FolderContextMenu
          x={folderMenu.x}
          y={folderMenu.y}
          canAddSubfolder={folderMenu.folder.parent_id == null}
          onRename={() => startRename(folderMenu.folder)}
          onNewChatHere={() => onNewChatInFolder(folderMenu.folder.id)}
          onNewSubfolder={() => {
            setCreatingIn(folderMenu.folder.id);
            ensureExpanded([folderMenu.folder.id]);
          }}
          onDelete={() => onDeleteFolder(folderMenu.folder.id)}
          onClose={() => setFolderMenu(null)}
        />
      ) : null}

      {chatMenu ? (
        <ChatContextMenu
          x={chatMenu.x}
          y={chatMenu.y}
          pinned={chatMenu.session.pinned}
          folders={folders}
          onMoveToFolder={(folderId) => onAssignFolder(chatMenu.session.session_id, folderId)}
          onTogglePin={() => onTogglePin(chatMenu.session.session_id, !chatMenu.session.pinned)}
          onHide={() => onHide(chatMenu.session.session_id)}
          onClose={() => setChatMenu(null)}
        />
      ) : null}
    </aside>
  );
}
