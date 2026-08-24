import { type TouchEvent as ReactTouchEvent, useCallback, useEffect, useRef } from "react";

/// Press-and-hold, shared by the two surfaces that want it: a message bubble
/// (hold to copy) and a file card (hold to share).
///
/// Its own module rather than living in either one — each is the other's
/// consumer, so whichever owned it would be imported by the other for a
/// gesture that has nothing to do with what that module is about.

/// Long-press-to-copy on a user bubble: hold ~450ms without dragging (a drag is
/// a scroll, which cancels). Native owns the clipboard write + confirming haptic
/// (`copyText`); the web side plays the squish + "copied" pill for
/// `COPY_TOAST_MS` before it fades.
const LONG_PRESS_MS = 450;

const LONG_PRESS_MOVE_CANCEL_PX = 10;

/// Fire `onLongPress` after a still ~`LONG_PRESS_MS` press; any drag past
/// `LONG_PRESS_MOVE_CANCEL_PX` (a scroll) or a lift first cancels it. Touch-only
/// — the pointer here is always a finger on the transcript webview.
export function useLongPress(onLongPress: () => void): {
  onTouchStart: (e: ReactTouchEvent) => void;
  onTouchMove: (e: ReactTouchEvent) => void;
  onTouchEnd: () => void;
} {
  const timer = useRef<number | undefined>(undefined);
  const origin = useRef<{ x: number; y: number } | null>(null);
  // The document-level second-finger watch, live only while a press is armed.
  const docWatch = useRef<((e: TouchEvent) => void) | null>(null);

  const cancel = useCallback(() => {
    clearTimeout(timer.current);
    timer.current = undefined;
    origin.current = null;
    if (docWatch.current) {
      document.removeEventListener("touchstart", docWatch.current, true);
      docWatch.current = null;
    }
  }, []);

  // Clear an armed press (timer + document watch) on unmount.
  useEffect(() => cancel, [cancel]);

  const onTouchStart = useCallback(
    (e: ReactTouchEvent) => {
      cancel();
      if (e.touches.length !== 1) return;
      const t = e.touches[0];
      origin.current = { x: t.clientX, y: t.clientY };
      timer.current = window.setTimeout(() => {
        cancel();
        onLongPress();
      }, LONG_PRESS_MS);
      // A second finger landing anywhere — even off the bubble — is a pinch or
      // scroll, not a copy. onTouchStart only re-fires for touches ON the bubble,
      // so watch the whole document while armed; `cancel` removes the listener.
      const watch = (ev: TouchEvent) => {
        if (ev.touches.length > 1) cancel();
      };
      docWatch.current = watch;
      document.addEventListener("touchstart", watch, { passive: true, capture: true });
    },
    [cancel, onLongPress],
  );

  const onTouchMove = useCallback(
    (e: ReactTouchEvent) => {
      if (origin.current === null || timer.current === undefined) return;
      if (e.touches.length !== 1) {
        cancel();
        return;
      }
      const t = e.touches[0];
      if (
        Math.abs(t.clientX - origin.current.x) > LONG_PRESS_MOVE_CANCEL_PX ||
        Math.abs(t.clientY - origin.current.y) > LONG_PRESS_MOVE_CANCEL_PX
      ) {
        cancel();
      }
    },
    [cancel],
  );

  return { onTouchStart, onTouchMove, onTouchEnd: cancel };
}
