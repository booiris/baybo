import { useEffect, type RefObject } from 'react';

/// Closes a popover on a press outside it, and on Escape.
///
/// Two rules live here rather than at each panel. Escape is **stopped**: these
/// panels open inside surfaces that close on Escape themselves — the issue
/// rail, the board's floating panels — and without it one key press collapses
/// the whole stack instead of the thing the operator was looking at. And the
/// focus goes back to the trigger rather than onto the body, so Escape leaves
/// the keyboard where it started instead of at the top of the document.
///
/// `onDismiss` is subscribed as given, so pass a stable callback (a
/// `useCallback` around the state setter) unless a re-subscribe per render is
/// what you want.
export function useDismiss({
  open,
  root,
  trigger,
  onDismiss,
}: {
  open: boolean;
  /// The panel and its trigger together: a press inside this is not outside.
  root: RefObject<HTMLElement | null>;
  trigger: RefObject<HTMLElement | null>;
  onDismiss: () => void;
}): void {
  useEffect(() => {
    if (!open) return;
    function onPointerDown(event: MouseEvent) {
      if (root.current?.contains(event.target as Node) === true) return;
      onDismiss();
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== 'Escape') return;
      event.stopPropagation();
      onDismiss();
      trigger.current?.focus();
    }
    document.addEventListener('mousedown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('mousedown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open, root, trigger, onDismiss]);
}
