// Per-viewer layout of the chat page's browser panel: whether it is open and
// how wide it was dragged. A convenience only, so storage that throws or comes
// back empty just means the defaults.

const PANEL_PREFS_KEY = 'baybo.chat.browserPanel';

export const PANEL_MIN_WIDTH_PX = 320;
export const PANEL_DEFAULT_WIDTH_PX = 520;
/** The panel takes at most this share of the space beside the sidebar… */
export const PANEL_MAX_SHARE = 0.7;
/** …and always leaves the thread at least this wide. */
export const THREAD_MIN_WIDTH_PX = 384;

export interface PanelPrefs {
  open: boolean;
  width: number;
}

export const DEFAULT_PANEL_PREFS: PanelPrefs = { open: false, width: PANEL_DEFAULT_WIDTH_PX };

/** The same bounds as CSS, so a window narrower than the stored width
 *  squeezes the panel without the stored width being rewritten. */
export const PANEL_MAX_WIDTH_CSS = `min(${PANEL_MAX_SHARE * 100}%, calc(100% - ${THREAD_MIN_WIDTH_PX}px))`;

export function clampPanelWidth(width: number, available: number): number {
  const max = Math.max(
    PANEL_MIN_WIDTH_PX,
    Math.floor(Math.min(available * PANEL_MAX_SHARE, available - THREAD_MIN_WIDTH_PX)),
  );
  return Math.round(Math.min(max, Math.max(PANEL_MIN_WIDTH_PX, width)));
}

export function readPanelPrefs(): PanelPrefs {
  try {
    const raw = window.localStorage.getItem(PANEL_PREFS_KEY);
    if (raw === null) return DEFAULT_PANEL_PREFS;
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null) return DEFAULT_PANEL_PREFS;
    const { open, width } = parsed as Partial<Record<keyof PanelPrefs, unknown>>;
    return {
      open: open === true,
      width:
        typeof width === 'number' && Number.isFinite(width)
          ? Math.max(PANEL_MIN_WIDTH_PX, width)
          : PANEL_DEFAULT_WIDTH_PX,
    };
  } catch {
    return DEFAULT_PANEL_PREFS;
  }
}

export function writePanelPrefs(prefs: PanelPrefs): void {
  try {
    window.localStorage.setItem(PANEL_PREFS_KEY, JSON.stringify(prefs));
  } catch {
    /* storage disabled; the panel still works for this page load */
  }
}
