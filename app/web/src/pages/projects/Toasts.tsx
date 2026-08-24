import { useCallback, useRef, useState } from 'react';

export type ToastTone = 'ok' | 'warn' | 'err';

/// A toast may carry one action. Only failures use it: a rollback tells the
/// operator what happened, and the useful next thing is almost always "do
/// that again" — offering it here saves re-finding the card and re-dragging
/// it to a column the board has already snapped back from.
export type ToastAction = { label: string; run: () => void };

export type Toast = { id: number; tone: ToastTone; text: string; action?: ToastAction };

const TOAST_MS = 4000;
/// A toast you can act on has to outlive the reflex to read it.
const ACTIONABLE_MS = 10_000;

const TONE_STYLE: Record<ToastTone, string> = {
  ok: 'border-black bg-ink text-canvas',
  warn: 'border-warn bg-warn text-white',
  err: 'border-err bg-err text-white',
};

const TONE_GLYPH: Record<ToastTone, string> = {
  ok: '▣',
  warn: '✕',
  err: '✕',
};

export function useToasts(): {
  toasts: Toast[];
  pushToast: (tone: ToastTone, text: string, action?: ToastAction) => void;
  dismissToast: (id: number) => void;
} {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const nextId = useRef(0);

  const dismissToast = useCallback((id: number) => {
    setToasts((current) => current.filter((toast) => toast.id !== id));
  }, []);

  const pushToast = useCallback(
    (tone: ToastTone, text: string, action?: ToastAction) => {
      nextId.current += 1;
      const id = nextId.current;
      setToasts((current) => [...current, { id, tone, text, action }]);
      window.setTimeout(
        () => {
          dismissToast(id);
        },
        action === undefined ? TOAST_MS : ACTIONABLE_MS,
      );
    },
    [dismissToast],
  );

  return { toasts, pushToast, dismissToast };
}

export function ToastStack({
  toasts,
  onDismiss,
}: {
  toasts: Toast[];
  onDismiss?: (id: number) => void;
}) {
  if (toasts.length === 0) return null;
  return (
    <div className="pointer-events-none fixed bottom-5 right-5 z-50 flex flex-col items-end gap-2">
      {toasts.map((toast) => (
        <div
          key={toast.id}
          role="status"
          className={`pointer-events-auto flex items-center gap-3 border-2 rounded-md shadow-brutal-sm px-4 py-2 font-mono text-[0.78rem] font-bold ${TONE_STYLE[toast.tone]}`}
        >
          <span aria-hidden="true">{TONE_GLYPH[toast.tone]}</span>
          <span>{toast.text}</span>
          {toast.action !== undefined ? (
            <button
              type="button"
              className="underline cursor-pointer shrink-0"
              onClick={() => {
                toast.action?.run();
                onDismiss?.(toast.id);
              }}
            >
              {toast.action.label}
            </button>
          ) : null}
        </div>
      ))}
    </div>
  );
}
