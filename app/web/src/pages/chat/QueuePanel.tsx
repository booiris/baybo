import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import {
  RiDraggable,
  RiSendPlane2Line,
  RiDeleteBin6Line,
  RiEditLine,
  RiFileLine,
  RiCheckLine,
  RiCloseLine,
  RiArrowDownSLine,
} from 'react-icons/ri';
import {
  DndContext,
  PointerSensor,
  KeyboardSensor,
  useSensor,
  useSensors,
  closestCenter,
  type DragEndEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  sortableKeyboardCoordinates,
  verticalListSortingStrategy,
  useSortable,
  arrayMove,
} from '@dnd-kit/sortable';
import { restrictToVerticalAxis, restrictToParentElement } from '@dnd-kit/modifiers';
import { CSS } from '@dnd-kit/utilities';
import { AttachmentImage } from './AttachmentImage';
import { useSessionQueue, type PauseReason, type QueuedItem } from './queueStore';

// The parked-message queue rendered above the composer pill. FIFO: newest at
// the bottom, the top item ("next") fires first. Drag the 6-dot handle to
// reorder; per-row send / delete / edit; a pinned banner offers bulk-resume
// after a /stop or error.

export interface QueuePanelProps {
  sessionId: string;
  baseUrl: string;
  channelToken: string | null;
  onFire: (item: QueuedItem) => void;
  onResume: () => void;
}

export function QueuePanel({
  sessionId,
  baseUrl,
  channelToken,
  onFire,
  onResume,
}: QueuePanelProps) {
  const { items, pauseReason, reorder } = useSessionQueue(sessionId);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 4 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  const handleDragEnd = useCallback(
    (event: DragEndEvent) => {
      const { active, over } = event;
      if (!over || active.id === over.id) return;
      const oldIndex = items.findIndex((i) => i.id === active.id);
      const newIndex = items.findIndex((i) => i.id === over.id);
      if (oldIndex < 0 || newIndex < 0) return;
      reorder(arrayMove(items, oldIndex, newIndex).map((i) => i.id));
    },
    [items, reorder],
  );

  // "Scroll for more" hint: shown while the list overflows and isn't scrolled
  // to the bottom. Recomputed on scroll and whenever the items change.
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const [showMore, setShowMore] = useState(false);
  const updateScrollHint = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    setShowMore(el.scrollHeight - el.scrollTop - el.clientHeight > 4);
  }, []);
  useEffect(() => {
    updateScrollHint();
  }, [items, updateScrollHint]);

  if (items.length === 0) return null;

  return (
    <div className="pointer-events-auto relative z-10 mx-auto w-full max-w-3xl flex flex-col gap-1.5">
      {pauseReason ? <CancelledBanner reason={pauseReason} onResume={onResume} /> : null}
      {/* Slightly narrower than the input box and resting flush on the pill.
          No bottom border — the pill's top border is the single shared divider
          so the box's bottom coincides with the input box. Scrolls past max
          height. */}
      <div
        ref={scrollRef}
        onScroll={updateScrollHint}
        className="max-h-56 overflow-y-auto border-2 border-black border-b-0 rounded-t-md bg-canvas [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
      >
        <div className="flex flex-col divide-y divide-black/15">
          <DndContext
            sensors={sensors}
            collisionDetection={closestCenter}
            modifiers={[restrictToVerticalAxis, restrictToParentElement]}
            onDragEnd={handleDragEnd}
          >
            <SortableContext items={items.map((i) => i.id)} strategy={verticalListSortingStrategy}>
              {items.map((item) => (
                <QueuedRow
                  key={item.id}
                  sessionId={sessionId}
                  item={item}
                  baseUrl={baseUrl}
                  channelToken={channelToken}
                  onFire={() => onFire(item)}
                />
              ))}
            </SortableContext>
          </DndContext>
        </div>
      </div>
      {showMore ? (
        <div className="pointer-events-none absolute bottom-0.5 left-0.5 right-0.5 flex items-end justify-center rounded-b-md pt-6 pb-1 bg-linear-to-t from-canvas to-transparent">
          <span className="flex items-center gap-1 font-mono text-[0.55rem] font-bold uppercase tracking-wider text-ink-soft">
            <RiArrowDownSLine className="text-xs" />
            scroll for more
          </span>
        </div>
      ) : null}
    </div>
  );
}

function QueuedRow({
  sessionId,
  item,
  baseUrl,
  channelToken,
  onFire,
}: {
  sessionId: string;
  item: QueuedItem;
  baseUrl: string;
  channelToken: string | null;
  onFire: () => void;
}) {
  const { removeItem, editItem } = useSessionQueue(sessionId);
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: item.id,
  });
  const style = { transform: CSS.Transform.toString(transform), transition };
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(item.text);
  const startEdit = useCallback(() => {
    setDraft(item.text);
    setEditing(true);
  }, [item.text]);
  const saveEdit = useCallback(() => {
    const next = draft.trim();
    // Allow clearing text only when the item still carries attachments;
    // never collapse a row to fully empty.
    if (next !== item.text && (next.length > 0 || item.attachments.length > 0)) {
      editItem(item.id, next);
    }
    setEditing(false);
  }, [draft, item.text, item.id, item.attachments.length, editItem]);
  const cancelEdit = useCallback(() => {
    setEditing(false);
    setDraft(item.text);
  }, [item.text]);
  return (
    <div
      ref={setNodeRef}
      style={style}
      className={`group relative flex items-start gap-2 px-2.5 py-2 ${
        isDragging ? 'z-30 bg-surface shadow-brutal-xs' : 'bg-canvas hover:bg-surface'
      }`}
    >
      <button
        type="button"
        {...(editing ? {} : attributes)}
        {...(editing ? {} : listeners)}
        disabled={editing}
        className={`shrink-0 -ml-1 h-5 flex items-center text-ink-soft focus:outline-none ${
          editing ? 'opacity-30 cursor-default' : 'cursor-grab active:cursor-grabbing hover:text-ink'
        }`}
        aria-label="Drag to reorder queued message"
        title="Drag to reorder"
      >
        <RiDraggable className="text-sm" />
      </button>
      <div className="min-w-0 flex-1 flex flex-col gap-1">
        {editing ? (
          <textarea
            autoFocus
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                saveEdit();
              } else if (e.key === 'Escape') {
                e.preventDefault();
                cancelEdit();
              }
            }}
            rows={Math.min(6, Math.max(1, draft.split('\n').length))}
            className="w-full resize-none bg-canvas border-2 border-black rounded-md px-2 py-1 font-sans text-sm text-ink focus:outline-none"
            placeholder="Edit message… (Enter saves, Esc cancels)"
          />
        ) : item.text ? (
          <span className="font-sans text-sm text-ink whitespace-pre-wrap line-clamp-3 break-words">
            {item.text}
          </span>
        ) : null}
        {item.attachments.length > 0 ? (
          <div className="flex flex-wrap gap-1.5">
            {item.attachments.map((a, i) =>
              a.kind === 'image' ? (
                <div key={`${a.blob_id}-${i}`} className="max-h-16 overflow-hidden rounded-md">
                  <AttachmentImage
                    blobId={a.blob_id}
                    alt={a.filename ?? 'image'}
                    baseUrl={baseUrl}
                    channelToken={channelToken}
                  />
                </div>
              ) : (
                <span
                  key={`${a.blob_id}-${i}`}
                  className="flex items-center gap-1.5 px-2 py-0.5 bg-canvas border-2 border-black rounded-md font-mono text-[0.65rem] max-w-full"
                  title={a.filename ?? a.mime_type}
                >
                  <RiFileLine className="text-xs shrink-0" />
                  <span className="truncate">{a.filename ?? a.mime_type}</span>
                </span>
              ),
            )}
          </div>
        ) : null}
      </div>
      <div className="shrink-0 flex items-center gap-1">
        {editing ? (
          <>
            <RowIconButton
              onClick={saveEdit}
              title="Save"
              label="Save edit"
              className="bg-brand hover:bg-brand-hover text-ink"
            >
              <RiCheckLine className="text-sm" />
            </RowIconButton>
            <RowIconButton
              onClick={cancelEdit}
              title="Cancel"
              label="Cancel edit"
              className="bg-surface text-ink-soft hover:bg-canvas hover:text-ink"
            >
              <RiCloseLine className="text-sm" />
            </RowIconButton>
          </>
        ) : (
          <>
            <RowIconButton
              onClick={onFire}
              title="Send now"
              label="Send this message now"
              className="bg-brand hover:bg-brand-hover text-ink"
            >
              <RiSendPlane2Line className="text-sm" />
            </RowIconButton>
            <RowIconButton
              onClick={() => removeItem(item.id)}
              title="Remove from queue"
              label="Remove from queue"
              className="bg-surface text-ink-soft hover:bg-err hover:text-white"
            >
              <RiDeleteBin6Line className="text-sm" />
            </RowIconButton>
            <RowIconButton
              onClick={startEdit}
              title="Edit message"
              label="Edit message"
              className="bg-surface text-ink-soft hover:bg-canvas hover:text-ink"
            >
              <RiEditLine className="text-sm" />
            </RowIconButton>
          </>
        )}
      </div>
    </div>
  );
}

function RowIconButton({
  onClick,
  title,
  label,
  className,
  children,
}: {
  onClick: () => void;
  title: string;
  label: string;
  className: string;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onPointerDown={(e) => e.stopPropagation()}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        onClick();
      }}
      className={`h-6 w-6 flex items-center justify-center border-2 border-black rounded-md shadow-brutal-xs active:translate-x-[1px] active:translate-y-[1px] active:shadow-none cursor-pointer transition-colors ${className}`}
      title={title}
      aria-label={label}
    >
      {children}
    </button>
  );
}

const BANNER_TEXT: Record<Exclude<PauseReason, null>, string> = {
  cancelled: 'Turn cancelled — send the remaining queued messages?',
  error: 'Turn failed — send the remaining queued messages?',
};

function CancelledBanner({
  reason,
  onResume,
}: {
  reason: Exclude<PauseReason, null>;
  onResume: () => void;
}) {
  const tone = reason === 'error' ? 'border-err bg-err/10' : 'border-warn bg-warn/10';
  return (
    <div className={`flex items-center justify-between gap-3 border-2 rounded-md px-3 py-2 ${tone}`}>
      <span className="font-mono text-[0.7rem] text-ink min-w-0">{BANNER_TEXT[reason]}</span>
      <button
        type="button"
        onClick={(e) => {
          e.preventDefault();
          onResume();
        }}
        className="shrink-0 px-2.5 py-1 bg-brand hover:bg-brand-hover text-ink border-2 border-black rounded-md shadow-brutal-xs font-mono text-[0.65rem] font-bold uppercase tracking-wider active:translate-x-[1px] active:translate-y-[1px] active:shadow-none cursor-pointer"
      >
        Send remaining
      </button>
    </div>
  );
}
