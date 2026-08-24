import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from 'react';

import { useDismiss } from '../../components/useDismiss';

/// How long the panel takes to arrive, and to leave.
const PANEL_SLIDE_MS = 180;

/// A board surface's right-hand layer: the activity drawer and the agent
/// profile, which share it and are mutually exclusive. The board page and the
/// column page both mount it, so the way it arrives and leaves has one home.
///
/// It slides in rather than appearing, and it leaves on ✕, on Escape, and on
/// a press anywhere outside it. One home for all three, because both panels
/// are reached from the same places and a rule kept in each would be the same
/// rule only until one of them changed.
///
/// `children` is a function so ✕ leaves the way the other two do. Handed the
/// parent's `onDismiss` directly it would unmount on the spot, and a panel
/// that slides in but blinks out reads as a bug.
export function FloatingPanel({
  onDismiss,
  trigger,
  keepOpenWithin,
  children,
}: {
  onDismiss: () => void;
  /// Where Escape puts the keyboard back. Absent on a panel opened from a
  /// control that is gone from under the cursor by then — a card's avatar
  /// opens the agent profile and the card may have been replaced since.
  trigger?: RefObject<HTMLElement | null>;
  /// A region outside the panel whose presses must not close it — the
  /// controls that swap what it shows. See [`useDismiss`].
  keepOpenWithin?: RefObject<HTMLElement | null>;
  children: (leave: () => void) => ReactNode;
}) {
  const root = useRef<HTMLDivElement>(null);
  const [shown, setShown] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const timer = useRef<number | null>(null);

  // Mounted off-screen and moved on the next frame: a panel that mounts
  // already at its resting place has nothing to transition from.
  useEffect(() => {
    const frame = requestAnimationFrame(() => {
      setShown(true);
    });
    return () => {
      cancelAnimationFrame(frame);
      if (timer.current !== null) window.clearTimeout(timer.current);
    };
  }, []);

  const leave = useCallback(() => {
    if (timer.current !== null) return;
    setLeaving(true);
    // The unmount is the parent's and has to wait for the slide. The cleanup
    // above cancels it, so a panel replaced mid-slide — another avatar
    // pressed — does not take the one that replaced it down with it.
    timer.current = window.setTimeout(onDismiss, PANEL_SLIDE_MS);
  }, [onDismiss]);

  useDismiss({ open: !leaving, root, trigger, keepOpenWithin, onDismiss: leave });

  return (
    <div
      ref={root}
      style={{ transitionDuration: `${PANEL_SLIDE_MS}ms` }}
      className={`absolute inset-y-0 right-0 z-30 flex shadow-brutal transition-transform ease-out motion-reduce:transition-none ${
        shown && !leaving ? 'translate-x-0' : 'translate-x-full'
      }`}
    >
      {children(leave)}
    </div>
  );
}
