import { useEffect, useState } from 'react';

export interface AsyncState<T> {
  data?: T;
  error?: string;
  loading: boolean;
}

/**
 * Run an async fetch on mount / when `deps` change, with cancellation so
 * a stale response can't overwrite a newer one. `fn` is intentionally
 * not in the dep list — callers pass the real inputs via `deps`.
 */
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]): AsyncState<T> {
  const [state, setState] = useState<AsyncState<T>>({ loading: true });
  useEffect(() => {
    let cancelled = false;
    setState({ loading: true });
    fn()
      .then((data) => {
        if (!cancelled) setState({ data, loading: false });
      })
      .catch((e: unknown) => {
        if (!cancelled) setState({ error: e instanceof Error ? e.message : String(e), loading: false });
      });
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  return state;
}
