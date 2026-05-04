/**
 * Tiny FIFO semaphore for backpressuring concurrent download phases.
 *
 * Why we need this: the Lark SDK fires `message` events without
 * awaiting their handlers, so several inbound messages with
 * attachments can race into `dispatchInbound` simultaneously. Each
 * download is byte-capped, but N concurrent inbounds compound to
 * N × 9 × 50 MiB in the worst case. Holding a slot across the
 * download loop bounds the per-bot peak to `permits × 9 × 50 MiB`.
 *
 * Why FIFO: starvation matters here — a bot getting hammered with
 * media should still process the oldest user's message first. A LIFO
 * stack would let new messages jump the line when a slot frees.
 */
export class Semaphore {
  private free: number;
  private readonly waiters: Array<() => void> = [];

  constructor(permits: number) {
    if (permits < 1) {
      throw new Error(`Semaphore permits must be >= 1 (got ${permits})`);
    }
    this.free = permits;
  }

  /** Run `fn` while holding a permit. Releases on return *or* throw. */
  async withPermit<T>(fn: () => Promise<T>): Promise<T> {
    await this.acquire();
    try {
      return await fn();
    } finally {
      this.release();
    }
  }

  private acquire(): Promise<void> {
    if (this.free > 0) {
      this.free -= 1;
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      this.waiters.push(resolve);
    });
  }

  private release(): void {
    const next = this.waiters.shift();
    if (next) {
      // Hand the permit straight to the waiter without going through
      // `free`; otherwise a fast `acquire` could overtake.
      next();
      return;
    }
    this.free += 1;
  }
}
