/**
 * Minimal `useSyncExternalStore` source. Both PWA stores below are the same
 * shape — a snapshot object, a listener set, a notify — so the boilerplate
 * lives once here and each store contributes only its transitions.
 *
 * `getSnapshot` must return a stable reference while nothing changed, which is
 * why `set` is the only writer: React re-renders on identity, and rebuilding
 * the snapshot on every read would loop.
 */
export class Store<T> {
  private snapshot: T;
  private readonly listeners = new Set<() => void>();

  protected constructor(initial: T) {
    this.snapshot = initial;
  }

  readonly subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  readonly getSnapshot = (): T => this.snapshot;

  protected set(next: T): void {
    this.snapshot = next;
    for (const listener of [...this.listeners]) listener();
  }
}
