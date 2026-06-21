import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import type { Folder } from './types';

// Chat-list folders. The folder *list* is server state — seeded from
// `GET /v1/chat/folders` and replaced wholesale on every
// `Frame::FoldersChanged` snapshot (folders are few, so no patch-merge).
// Only the per-folder collapsed/expanded state is client-local, persisted
// under a single `aura.folders.collapsed` localStorage key and synced
// across tabs via the storage event.

const COLLAPSED_KEY = 'aura.folders.collapsed';

function readCollapsed(): Set<string> {
  try {
    const raw = window.localStorage.getItem(COLLAPSED_KEY);
    if (raw === null) return new Set();
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return new Set();
    return new Set(parsed.filter((x): x is string => typeof x === 'string'));
  } catch {
    return new Set();
  }
}

function writeCollapsed(set: Set<string>): void {
  try {
    if (set.size === 0) {
      window.localStorage.removeItem(COLLAPSED_KEY);
      return;
    }
    window.localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...set]));
  } catch {
    /* storage disabled; in-memory tracking still works this session */
  }
}

// Imperative handle. Stable across renders so the WS onFrame closure can
// capture it once; `replaceFolders` is the convergence seam.
interface FolderApi {
  /** Replace the server folder list wholesale (bootstrap + WS snapshot). */
  replaceFolders: (folders: Folder[]) => void;
  /** Flip a folder's collapsed state. */
  toggleCollapse: (id: string) => void;
  /** Force a folder collapsed (true) or expanded (false). */
  setCollapsed: (id: string, collapsed: boolean) => void;
  /** Ensure each id is expanded — used to reveal the active chat's folder
   *  path on load. No-op when none are collapsed. */
  ensureExpanded: (ids: string[]) => void;
}

interface FolderState {
  folders: Folder[];
  collapsed: Set<string>;
}

const FolderApiContext = createContext<FolderApi | null>(null);
const FolderStateContext = createContext<FolderState>({ folders: [], collapsed: new Set() });

export function FolderProvider({ children }: { children: ReactNode }) {
  const [folders, setFolders] = useState<Folder[]>([]);
  const [collapsed, setCollapsedState] = useState<Set<string>>(() => readCollapsed());
  const collapsedRef = useRef<Set<string>>(collapsed);
  collapsedRef.current = collapsed;

  const replaceFolders = useCallback((next: Folder[]) => {
    setFolders(next);
  }, []);

  // applyCollapsed updates the ref synchronously before setState so two
  // mutations in one tick compose instead of clobbering through a stale ref.
  const applyCollapsed = useCallback((fn: (cur: Set<string>) => Set<string>): void => {
    const next = fn(collapsedRef.current);
    if (next === collapsedRef.current) return;
    collapsedRef.current = next;
    writeCollapsed(next);
    setCollapsedState(next);
  }, []);

  const toggleCollapse = useCallback(
    (id: string) =>
      applyCollapsed((cur) => {
        const next = new Set(cur);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        return next;
      }),
    [applyCollapsed],
  );

  const setCollapsed = useCallback(
    (id: string, c: boolean) =>
      applyCollapsed((cur) => {
        if (c === cur.has(id)) return cur;
        const next = new Set(cur);
        if (c) next.add(id);
        else next.delete(id);
        return next;
      }),
    [applyCollapsed],
  );

  const ensureExpanded = useCallback(
    (ids: string[]) =>
      applyCollapsed((cur) => {
        if (!ids.some((id) => cur.has(id))) return cur;
        const next = new Set(cur);
        for (const id of ids) next.delete(id);
        return next;
      }),
    [applyCollapsed],
  );

  // Ingest collapse-state writes from OTHER tabs (the storage event never
  // fires in the writer tab).
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key !== COLLAPSED_KEY) return;
      const next = readCollapsed();
      collapsedRef.current = next;
      setCollapsedState(next);
    };
    window.addEventListener('storage', onStorage);
    return () => window.removeEventListener('storage', onStorage);
  }, []);

  const api = useMemo<FolderApi>(
    () => ({ replaceFolders, toggleCollapse, setCollapsed, ensureExpanded }),
    [replaceFolders, toggleCollapse, setCollapsed, ensureExpanded],
  );
  const state = useMemo<FolderState>(() => ({ folders, collapsed }), [folders, collapsed]);

  return (
    <FolderApiContext.Provider value={api}>
      <FolderStateContext.Provider value={state}>{children}</FolderStateContext.Provider>
    </FolderApiContext.Provider>
  );
}

// Imperative handle for the WS onFrame closure + bootstrap. Does NOT
// re-render on folder change.
export function useFolderStore(): FolderApi {
  const ctx = useContext(FolderApiContext);
  if (!ctx) throw new Error('useFolderStore must be used inside <FolderProvider>');
  return ctx;
}

// Reactive folder list + collapsed set for the sidebar.
export function useFolders(): FolderState {
  return useContext(FolderStateContext);
}
