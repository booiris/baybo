import { type TouchEvent as ReactTouchEvent, useCallback, useEffect, useRef } from "react";


const LONG_PRESS_MS = 450;

const LONG_PRESS_MOVE_CANCEL_PX = 10;

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
      const watch = (ev: TouchEvent) => {
        // A second finger may land outside this element; the document watch
        // cancels what is now a pinch or scroll rather than a long press.
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
