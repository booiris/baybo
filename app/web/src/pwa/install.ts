import { Store } from './store';

/**
 * The Chromium-only `beforeinstallprompt` event. Not in `lib.dom`, and
 * deliberately declared as only what we call: `preventDefault` to stop the
 * browser's own mini-infobar, `prompt` to raise the dialog from our button.
 */
export interface InstallPromptEvent {
  preventDefault(): void;
  prompt(): Promise<unknown>;
}

export interface PwaInstallState {
  /** The browser offered an install prompt we are holding on to. */
  readonly canInstall: boolean;
}

/**
 * Holds the deferred install prompt so the rail can offer "install" at a
 * moment the user is looking at, instead of whenever Chrome decides to.
 *
 * Safari never fires the event (installs go through Share → Add to Home
 * Screen), so `canInstall` staying false is the normal state on iOS — the
 * button simply never appears.
 */
export class InstallPrompt extends Store<PwaInstallState> {
  private deferred: InstallPromptEvent | null = null;

  constructor() {
    super({ canInstall: false });
  }

  offer(event: InstallPromptEvent): void {
    this.deferred = event;
    if (this.getSnapshot().canInstall) return;
    this.set({ canInstall: true });
  }

  async prompt(): Promise<void> {
    const event = this.deferred;
    if (!event) return;
    // A deferred prompt is single-use — a second `prompt()` on the same event
    // rejects — so it is spent before the dialog is even raised.
    this.clear();
    await event.prompt();
  }

  /** Installed (by our button or the browser's own affordance). */
  onInstalled(): void {
    this.clear();
  }

  private clear(): void {
    this.deferred = null;
    if (!this.getSnapshot().canInstall) return;
    this.set({ canInstall: false });
  }
}
