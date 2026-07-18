// vitest's jsdom window exposes a non-functional `localStorage` (an empty
// object — `setItem`/`getItem`/`clear` are all undefined), so any suite that
// exercises real persistence installs a Map-backed one. Unlike the pure
// outbox test — which stubs the WHOLE window — this replaces only
// `window.localStorage`, leaving `addEventListener` / `dispatchEvent` /
// `StorageEvent` intact for the cross-tab `storage` path.
export function installMemoryLocalStorage(): void {
  const map = new Map<string, string>();
  const storage = {
    get length() {
      return map.size;
    },
    clear: () => map.clear(),
    getItem: (k: string) => map.get(k) ?? null,
    key: (i: number) => Array.from(map.keys())[i] ?? null,
    removeItem: (k: string) => {
      map.delete(k);
    },
    setItem: (k: string, v: string) => {
      map.set(k, String(v));
    },
  } satisfies Storage;
  Object.defineProperty(window, 'localStorage', {
    value: storage,
    configurable: true,
    writable: true,
  });
}
