import type { ConsoleMessage, Dialog, Page, Frame } from "playwright";

const RECENT_DIALOGS_CAP = 20;
const FRAME_TREE_CAP = 30;
const CONSOLE_LOG_CAP = 50;
const DIALOG_WATCHDOG_MS = 5 * 60 * 1000;

export type DialogPolicy = "must_respond" | "auto_dismiss" | "auto_accept";

interface PendingDialog {
  id: string;
  type: string;
  message: string;
  defaultValue: string | null;
  url: string;
  openedAt: number;
  // Hidden Playwright resolver — accept/dismiss completes this side.
  dialog: Dialog;
  watchdog: NodeJS.Timeout;
}

interface RecentDialog {
  id: string;
  type: string;
  message: string;
  resolution: "accepted" | "dismissed" | "auto_dismissed" | "auto_accepted";
  closedAt: number;
}

interface FrameNode {
  url: string;
  name: string | null;
}

interface ConsoleEntry {
  level: string;
  text: string;
  ts: number;
}

export interface SupervisorSnapshot {
  pending_dialogs: Array<Omit<PendingDialog, "dialog" | "watchdog">>;
  recent_dialogs: RecentDialog[];
  frame_tree: FrameNode[];
  console_log: ConsoleEntry[];
}

/**
 * Per-context Hermes-style watcher. Tracks pending dialogs, frame tree
 * and console events so the agent can observe page state without
 * polling Playwright directly.
 */
export class Supervisor {
  policy: DialogPolicy = "must_respond";
  private readonly pending = new Map<string, PendingDialog>();
  private readonly recent: RecentDialog[] = [];
  private readonly consoleBuf: ConsoleEntry[] = [];
  private page: Page | null = null;
  private dialogSeq = 0;

  attach(page: Page): void {
    this.page = page;
    page.on("console", (msg: ConsoleMessage) => {
      this.pushConsole(msg.type(), msg.text());
    });
    page.on("pageerror", (err: Error) => {
      this.pushConsole("error", err.message);
    });
  }

  async onDialog(dialog: Dialog): Promise<void> {
    const id = `d${++this.dialogSeq}`;
    if (this.policy === "auto_dismiss") {
      await dialog.dismiss();
      this.recordResolution(id, dialog, "auto_dismissed");
      return;
    }
    if (this.policy === "auto_accept") {
      await dialog.accept(dialog.defaultValue() || "");
      this.recordResolution(id, dialog, "auto_accepted");
      return;
    }
    const watchdog = setTimeout(() => {
      void this.timeout(id);
    }, DIALOG_WATCHDOG_MS);
    watchdog.unref();
    const entry: PendingDialog = {
      id,
      type: dialog.type(),
      message: dialog.message(),
      defaultValue: dialog.defaultValue() || null,
      url: this.page?.url() ?? "",
      openedAt: Date.now(),
      dialog,
      watchdog,
    };
    this.pending.set(id, entry);
  }

  async respond(
    action: "accept" | "dismiss",
    promptText: string | null,
    dialogId: string | null,
  ): Promise<{ ok: boolean; dialog: Omit<PendingDialog, "dialog" | "watchdog"> | null }> {
    const id = dialogId ?? this.firstPendingId();
    if (!id) return { ok: false, dialog: null };
    const entry = this.pending.get(id);
    if (!entry) return { ok: false, dialog: null };
    clearTimeout(entry.watchdog);
    this.pending.delete(id);
    if (action === "accept") {
      await entry.dialog.accept(promptText || entry.defaultValue || "");
    } else {
      await entry.dialog.dismiss();
    }
    this.recordResolution(id, entry.dialog, action === "accept" ? "accepted" : "dismissed");
    return {
      ok: true,
      dialog: {
        id: entry.id,
        type: entry.type,
        message: entry.message,
        defaultValue: entry.defaultValue,
        url: entry.url,
        openedAt: entry.openedAt,
      },
    };
  }

  snapshot(): SupervisorSnapshot {
    return {
      pending_dialogs: [...this.pending.values()].map((d) => ({
        id: d.id,
        type: d.type,
        message: d.message,
        defaultValue: d.defaultValue,
        url: d.url,
        openedAt: d.openedAt,
      })),
      recent_dialogs: [...this.recent],
      frame_tree: this.frameTree(),
      console_log: [...this.consoleBuf],
    };
  }

  private frameTree(): FrameNode[] {
    if (!this.page) return [];
    const out: FrameNode[] = [];
    const walk = (frame: Frame): void => {
      if (out.length >= FRAME_TREE_CAP) return;
      out.push({ url: frame.url(), name: frame.name() || null });
      for (const child of frame.childFrames()) {
        walk(child);
      }
    };
    walk(this.page.mainFrame());
    return out;
  }

  private firstPendingId(): string | null {
    for (const id of this.pending.keys()) return id;
    return null;
  }

  private async timeout(id: string): Promise<void> {
    const entry = this.pending.get(id);
    if (!entry) return;
    this.pending.delete(id);
    try {
      await entry.dialog.dismiss();
    } catch {
      // Already-handled dialogs throw; treat as recorded.
    }
    this.recordResolution(id, entry.dialog, "auto_dismissed");
  }

  private recordResolution(
    id: string,
    dialog: Dialog,
    resolution: RecentDialog["resolution"],
  ): void {
    this.recent.push({
      id,
      type: dialog.type(),
      message: dialog.message(),
      resolution,
      closedAt: Date.now(),
    });
    while (this.recent.length > RECENT_DIALOGS_CAP) this.recent.shift();
  }

  private pushConsole(level: string, text: string): void {
    this.consoleBuf.push({ level, text, ts: Date.now() });
    while (this.consoleBuf.length > CONSOLE_LOG_CAP) this.consoleBuf.shift();
  }

  consoleLog(): ConsoleEntry[] {
    return [...this.consoleBuf];
  }

  clearConsole(): void {
    this.consoleBuf.length = 0;
  }
}
