import { useCallback, useRef, useState } from 'react';


export type ToastTone = 'ok' | 'warn' | 'err';

export type Toast = { id: number; tone: ToastTone; text: string };

const TOAST_MS = 4000;

const TONE_STYLE: Record<ToastTone, string> = {
  ok: 'border-black bg-ink text-canvas',
  warn: 'border-warn bg-warn text-white',
  err: 'border-err bg-err text-white',
};

export function useToasts(): {
  toasts: Toast[];
  pushToast: (tone: ToastTone, text: string) => void;
} {
  const [toasts, setToasts] = useState<Toast[]>([]);
  const nextId = useRef(0);

  const pushToast = useCallback((tone: ToastTone, text: string) => {
    nextId.current += 1;
    const id = nextId.current;
    setToasts((current) => [...current, { id, tone, text }]);
    window.setTimeout(() => {
      setToasts((current) => current.filter((toast) => toast.id !== id));
    }, TOAST_MS);
  }, []);

  return { toasts, pushToast };
}

export function ToastStack({ toasts }: { toasts: Toast[] }) {
  if (toasts.length === 0) return null;
  return (
    <div className="pointer-events-none fixed bottom-5 right-5 z-50 flex flex-col items-end gap-2">
      {toasts.map((toast) => (
        <div
          key={toast.id}
          className={`border-2 rounded-md shadow-brutal-sm px-4 py-2 font-mono text-[0.78rem] font-bold ${TONE_STYLE[toast.tone]}`}
        >
          {toast.text}
        </div>
      ))}
    </div>
  );
}
