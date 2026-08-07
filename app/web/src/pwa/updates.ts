import { Store } from './store';

/** Message that tells a waiting worker to take over. Mirrored in `pwa/service-worker.js`. */
export const SKIP_WAITING = 'SKIP_WAITING';

export interface PwaUpdateState {
  /** A newer bundle is installed and waiting for this page to let go. */
  readonly updateReady: boolean;
}

/** The one thing this store needs from a `ServiceWorker`. */
export interface WaitingWorker {
  postMessage(message: { type: string }): void;
}

const IDLE: PwaUpdateState = { updateReady: false };

/**
 * Tracks the waiting-worker handoff and owns the two rules that are easy to get
 * wrong and invisible in a type check:
 *
 *  1. A worker reaching `installed` is only an *update* if this page already
 *     had a controller. On a first install there is nothing to reload for, and
 *     prompting there means every new visitor is told the app is out of date.
 *  2. `controllerchange` fires both when the user accepts a reload AND when a
 *     first worker claims the page. Reloading unconditionally turns the first
 *     visit into a flash — and into a loop if anything re-registers.
 */
export class ServiceWorkerUpdates extends Store<PwaUpdateState> {
  private waiting: WaitingWorker | null = null;
  private reloadRequested = false;

  constructor(private readonly reload: () => void) {
    super(IDLE);
  }

  /** A worker finished installing. `pageWasControlled` is rule 1 above. */
  offer(worker: WaitingWorker, pageWasControlled: boolean): void {
    if (!pageWasControlled) return;
    this.waiting = worker;
    if (this.getSnapshot().updateReady) return;
    this.set({ updateReady: true });
  }

  /** Hand over to the waiting worker. The reload follows from its `controllerchange`. */
  apply(): void {
    if (!this.waiting) return;
    this.reloadRequested = true;
    this.waiting.postMessage({ type: SKIP_WAITING });
  }

  /** The new worker took control — rule 2 above. */
  onControllerChange(): void {
    if (!this.reloadRequested) return;
    this.reloadRequested = false;
    this.reload();
  }
}
