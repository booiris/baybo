import { useEffect, useRef, useState, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import {
  RiEditLine,
  RiChat3Line,
  RiFolder3Line,
  RiFolderAddLine,
  RiDeleteBin6Line,
  RiPushpin2Line,
  RiPushpin2Fill,
  RiArrowRightSLine,
} from 'react-icons/ri';
import type { Folder } from './types';

// Right-click menus for folders and chats, built from scratch via a body
// portal so they escape the sidebar's overflow-auto clip and position at the
// raw cursor point. Both share dismissal (outside click + Escape) and the
// brutalist panel styling.

const PANEL =
  'fixed z-[55] min-w-[180px] bg-surface border-2 border-black rounded-md shadow-brutal py-1';
const ROW =
  'w-full text-left px-3 py-1.5 flex items-center gap-2 font-mono text-xs hover:bg-canvas cursor-pointer';
const ROW_DANGER =
  'w-full text-left px-3 py-1.5 flex items-center gap-2 font-mono text-xs text-err hover:bg-err/10 cursor-pointer';

function useDismiss(ref: React.RefObject<HTMLElement | null>, onClose: () => void) {
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('mousedown', onDown);
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('mousedown', onDown);
      document.removeEventListener('keydown', onKey);
    };
  }, [ref, onClose]);
}

function MenuRow({
  icon,
  label,
  danger,
  onClick,
  children,
}: {
  icon: ReactNode;
  label: string;
  danger?: boolean;
  onClick?: () => void;
  children?: ReactNode;
}) {
  return (
    <button type="button" onClick={onClick} className={danger ? ROW_DANGER : ROW}>
      <span className="shrink-0 text-sm">{icon}</span>
      <span className="flex-1 truncate">{label}</span>
      {children}
    </button>
  );
}

export interface FolderMenuProps {
  x: number;
  y: number;
  /** True when the folder may host a subfolder (top-level only — keeps depth ≤ 2). */
  canAddSubfolder: boolean;
  onRename: () => void;
  onNewChatHere: () => void;
  onNewSubfolder: () => void;
  onDelete: () => void;
  onClose: () => void;
}

export function FolderContextMenu({
  x,
  y,
  canAddSubfolder,
  onRename,
  onNewChatHere,
  onNewSubfolder,
  onDelete,
  onClose,
}: FolderMenuProps) {
  const ref = useRef<HTMLDivElement | null>(null);
  useDismiss(ref, onClose);
  const [confirming, setConfirming] = useState(false);

  const left = Math.min(x, window.innerWidth - 200);
  const top = Math.min(y, window.innerHeight - 220);

  return createPortal(
    <div ref={ref} style={{ left, top }} className={PANEL} role="menu">
      <MenuRow
        icon={<RiEditLine />}
        label="Rename"
        onClick={() => {
          onClose();
          onRename();
        }}
      />
      <MenuRow
        icon={<RiChat3Line />}
        label="New chat here"
        onClick={() => {
          onClose();
          onNewChatHere();
        }}
      />
      {canAddSubfolder ? (
        <MenuRow
          icon={<RiFolderAddLine />}
          label="New subfolder"
          onClick={() => {
            onClose();
            onNewSubfolder();
          }}
        />
      ) : null}
      <div className="my-1 border-t-2 border-black/10" />
      {confirming ? (
        <div className="px-3 py-1.5 flex items-center justify-between gap-2 font-mono text-xs">
          <span className="text-err">Delete?</span>
          <span className="flex items-center gap-1.5">
            <button
              type="button"
              onClick={() => {
                onClose();
                onDelete();
              }}
              className="px-2 py-0.5 bg-err text-white border-2 border-black rounded font-bold uppercase tracking-wider text-[0.6rem] cursor-pointer"
            >
              Yes
            </button>
            <button
              type="button"
              onClick={() => setConfirming(false)}
              className="px-2 py-0.5 bg-surface border-2 border-black rounded font-bold uppercase tracking-wider text-[0.6rem] cursor-pointer"
            >
              No
            </button>
          </span>
        </div>
      ) : (
        <MenuRow icon={<RiDeleteBin6Line />} label="Delete folder" danger onClick={() => setConfirming(true)} />
      )}
    </div>,
    document.body,
  );
}

export interface ChatMenuProps {
  x: number;
  y: number;
  pinned: boolean;
  /** False on a **cron fire** — it groups by its cron job and its `folder_id`
   *  is ignored (`docs/cron-groups.md`), so the whole "Move to folder ▸"
   *  submenu is hidden rather than offering a move that would never show. */
  canMoveToFolder: boolean;
  folders: Folder[];
  onMoveToFolder: (folderId: string | null) => void;
  /** Open the row's inline title editor. Offered on cron fires too: a fire's
   *  own row renders `session.title`, so the rename shows once its group is
   *  expanded. (The collapsed group header is the cron job's title and is
   *  unaffected — renaming a recurring conversation means editing the job.) */
  onRename: () => void;
  onTogglePin: () => void;
  onHide: () => void;
  onClose: () => void;
}

export function ChatContextMenu({
  x,
  y,
  pinned,
  canMoveToFolder,
  folders,
  onMoveToFolder,
  onRename,
  onTogglePin,
  onHide,
  onClose,
}: ChatMenuProps) {
  const ref = useRef<HTMLDivElement | null>(null);
  useDismiss(ref, onClose);
  const [submenu, setSubmenu] = useState(false);

  const left = Math.min(x, window.innerWidth - 200);
  const top = Math.min(y, window.innerHeight - 160);
  // Anchor the move submenu to the parent panel; flip left if it would clip.
  const subFlip = left + 180 + 180 > window.innerWidth;
  const subLeft = subFlip ? left - 178 : left + 178;

  return createPortal(
    <div ref={ref} style={{ left, top }} className={PANEL} role="menu">
      {canMoveToFolder ? (
        <>
          <div
            className="relative"
            onMouseEnter={() => setSubmenu(true)}
            onMouseLeave={() => setSubmenu(false)}
          >
            <MenuRow icon={<RiArrowRightSLine />} label="Move to folder">
              <RiArrowRightSLine className="text-sm text-ink-soft" />
            </MenuRow>
            {submenu ? (
              <div
                style={{ position: 'fixed', left: subLeft, top }}
                className="min-w-[178px] max-h-[50vh] overflow-auto bg-surface border-2 border-black rounded-md shadow-brutal py-1 z-[56]"
              >
                <button
                  type="button"
                  onClick={() => {
                    onClose();
                    onMoveToFolder(null);
                  }}
                  className={ROW}
                >
                  <span className="h-3 w-3 shrink-0 rounded-sm border-2 border-black bg-canvas" />
                  <span className="flex-1 truncate text-ink-soft">Uncategorized</span>
                </button>
                {folders.map((f) => (
                  <button
                    key={f.id}
                    type="button"
                    onClick={() => {
                      onClose();
                      onMoveToFolder(f.id);
                    }}
                    className={ROW}
                  >
                    <RiFolder3Line className="shrink-0 text-sm text-ink-soft" aria-hidden />
                    <span className="flex-1 truncate">{f.name}</span>
                  </button>
                ))}
              </div>
            ) : null}
          </div>
          <div className="my-1 border-t-2 border-black/10" />
        </>
      ) : null}
      <MenuRow
        icon={<RiEditLine />}
        label="Rename"
        onClick={() => {
          onClose();
          onRename();
        }}
      />
      <MenuRow
        icon={pinned ? <RiPushpin2Fill /> : <RiPushpin2Line />}
        label={pinned ? 'Unpin' : 'Pin to top'}
        onClick={() => {
          onClose();
          onTogglePin();
        }}
      />
      <MenuRow
        icon={<RiDeleteBin6Line />}
        label="Hide"
        danger
        onClick={() => {
          onClose();
          onHide();
        }}
      />
    </div>,
    document.body,
  );
}
